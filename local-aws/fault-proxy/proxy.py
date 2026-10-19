"""S3 장애 주입 프록시(L5 시험용). edge(Caddy) 와 LocalStack 사이에 서서 요청을 그대로 넘기되, 규칙에 맞는 요청에는 오류·지연을 준다.

toxiproxy 는 TCP 만 만져서 "이 경로만 403", "HeadObject 만 500" 같은 HTTP 수준 장애를 못 만든다. 이 프록시가 그 자리를 채운다.
표준 라이브러리만 쓴다(Lambda 기본 이미지의 파이썬으로 돈다).

제어 API (장애 주입 포트 FAULT_ADMIN_PORT, 기본 9099, 호스트 127.0.0.1:18090 으로 열린다)
  PUT    /rules      규칙 목록을 통째로 바꾼다. 본문 [{"name": "...", "method": "HEAD", "path": "정규식", "status": 500, "body": "...",
                      "delay_ms": 0, "count": 0, "forward": false}, ...]
                      - method(생략=전부), path(요청 경로 정규식, 생략=전부), host(호스트 정규식, 생략=전부)
                      - status 가 있으면 그 응답을 돌려준다(forward=true 면 delay 뒤에 원래 요청도 LocalStack 으로 보낸다. 객체는 만들어진다)
                      - status 가 없고 delay_ms 만 있으면 지연 뒤 정상 처리
                      - count: 이 규칙을 적용할 최대 횟수(0=무제한)
  DELETE /rules      규칙을 모두 지운다
  GET    /stats      규칙별 적용 횟수와 전체 요청 수
데이터 포트(PROXY_PORT, 기본 8080)는 edge 가 부른다. 업스트림은 UPSTREAM(기본 aws:4566).
"""

import http.client
import json
import os
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = os.environ.get("UPSTREAM", "aws:4566")
PROXY_PORT = int(os.environ.get("PROXY_PORT", "8080"))
ADMIN_PORT = int(os.environ.get("FAULT_ADMIN_PORT", "9099"))
HOP_BY_HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers", "transfer-encoding", "upgrade"}

_lock = threading.Lock()
_rules = []  # 각 규칙: dict + "_hits"
_total = 0


def log(**fields):
    print(json.dumps({"ts": time.strftime("%Y-%m-%dT%H:%M:%S"), **fields}, ensure_ascii=False), flush=True)


def match_rule(method, path, host):
    """맞는 첫 규칙을 골라 적용 횟수를 올려 돌려준다."""
    with _lock:
        for rule in _rules:
            if rule.get("method") and rule["method"].upper() != method:
                continue
            if rule.get("path") and not re.search(rule["path"], path):
                continue
            if rule.get("host") and not re.search(rule["host"], host):
                continue
            limit = int(rule.get("count", 0) or 0)
            if limit and rule["_hits"] >= limit:
                continue
            rule["_hits"] += 1
            return dict(rule)
    return None


def read_body(handler):
    length = handler.headers.get("Content-Length")
    if length is not None:
        return handler.rfile.read(int(length))
    if handler.headers.get("Transfer-Encoding", "").lower() == "chunked":
        chunks = []
        while True:
            size = int(handler.rfile.readline().strip().split(b";")[0] or b"0", 16)
            if size == 0:
                handler.rfile.readline()
                break
            chunks.append(handler.rfile.read(size))
            handler.rfile.readline()
        return b"".join(chunks)
    return b""


class Proxy(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def respond(self, status, headers, body, head=False):
        self.send_response(status)
        sent_length = False
        for name, value in headers:
            if name.lower() in HOP_BY_HOP or name.lower() == "server" or name.lower() == "date":
                continue
            if name.lower() == "content-length":
                sent_length = True
            self.send_header(name, value)
        if not sent_length and not head:
            self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if not head:
            self.wfile.write(body)

    def handle_any(self):
        global _total
        body = read_body(self)
        with _lock:
            _total += 1
        method = self.command
        host = self.headers.get("Host", "")
        rule = match_rule(method, self.path.split("?")[0], host)
        forwarded = False
        if rule:
            if rule.get("delay_ms"):
                time.sleep(rule["delay_ms"] / 1000)
            log(event="fault", rule=rule.get("name"), method=method, path=self.path.split("?")[0], status=rule.get("status"))
            if rule.get("status"):
                if rule.get("forward"):
                    self.forward(method, body, quiet=True)
                    forwarded = True
                payload = (rule.get("body") or "").encode()
                self.respond(int(rule["status"]), [("Content-Type", "application/xml")], payload, head=method == "HEAD")
                return
        if not forwarded:
            status, headers, data = self.forward(method, body)
            self.respond(status, headers, data, head=method == "HEAD")

    def forward(self, method, body, quiet=False):
        headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_BY_HOP}
        headers["Content-Length"] = str(len(body))
        conn = http.client.HTTPConnection(UPSTREAM, timeout=120)
        try:
            conn.putrequest(method, self.path, skip_host=True, skip_accept_encoding=True)
            for name, value in headers.items():
                conn.putheader(name, value)
            conn.endheaders(body)
            response = conn.getresponse()
            data = response.read() if method != "HEAD" else b""
            return response.status, response.getheaders(), data
        except OSError as error:
            if quiet:
                return 502, [], b""
            return 502, [("Content-Type", "text/plain")], f"upstream error: {error}".encode()
        finally:
            conn.close()

    do_GET = do_PUT = do_POST = do_DELETE = do_HEAD = do_OPTIONS = handle_any


class Admin(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def send_json(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_PUT(self):
        global _rules
        rules = json.loads(read_body(self) or b"[]")
        with _lock:
            _rules = [{**rule, "_hits": 0} for rule in rules]
        self.send_json(200, {"rules": len(rules)})

    def do_DELETE(self):
        global _rules
        with _lock:
            _rules = []
        self.send_json(200, {"rules": 0})

    def do_GET(self):
        with _lock:
            self.send_json(200, {"total": _total, "rules": [{"name": r.get("name"), "hits": r["_hits"]} for r in _rules]})


def main():
    admin = ThreadingHTTPServer(("0.0.0.0", ADMIN_PORT), Admin)
    admin.daemon_threads = True
    threading.Thread(target=admin.serve_forever, daemon=True).start()
    server = ThreadingHTTPServer(("0.0.0.0", PROXY_PORT), Proxy)
    server.daemon_threads = True
    server.request_queue_size = 256
    log(event="start", upstream=UPSTREAM, port=PROXY_PORT, admin=ADMIN_PORT)
    server.serve_forever()


if __name__ == "__main__":
    main()
