#!/usr/bin/env bash
# 백엔드 전체(Rust 서버 + PostgreSQL + S3/SQS 에뮬레이터 + 이미지 Lambda + esm-poller + PushWorker/가짜 FCM + HTTPS edge)를
# 한 번에 올린다. 필요한 준비(도구 다운로드, Lambda 의존성, Linux 바이너리 빌드)를 건너뛰어도 되는 만큼 건너뛴다.
#
#   scripts/up-all.sh                 # 준비 -> 기동 -> 스모크
#   scripts/up-all.sh --no-smoke      # 스모크 생략
#   scripts/up-all.sh --rebuild       # Rust 바이너리를 다시 빌드한다(코드를 바꾼 뒤)
#   scripts/up-all.sh --real-oidc     # env/real-oidc.env 를 겹쳐 실제 Kakao·Apple 로그인으로 띄운다
#   scripts/up-all.sh status          # 컨테이너 상태와 접속 정보
#   scripts/up-all.sh down            # 내린다(DB 볼륨은 남는다)
#   scripts/up-all.sh reset           # 내리고 DB·큐 데이터까지 지운다(down -v)
#
# 환경변수: HOST_IP(기본 .env 또는 127.0.0.1. 실기기는 노트북 LAN IP), ESM_CONCURRENCY(Lambda 동시 실행 수, 기본 1).
# 필요한 것: Docker(Compose v2), python3(스모크용), curl. Rust 는 설치하지 않아도 된다(Docker 안에서 Linux 로 빌드).
set -euo pipefail
cd "$(dirname "$0")/.."          # local-aws/
ROOT="$(cd .. && pwd)"
STATE="$PWD/.state"

cmd=up; smoke=1; rebuild=0; real_oidc=0
for a in "$@"; do
  case "$a" in
    up|down|reset|status) cmd="$a" ;;
    --no-smoke) smoke=0 ;;
    --rebuild) rebuild=1 ;;
    --real-oidc) real_oidc=1 ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "알 수 없는 인자: $a (-h 참고)" >&2; exit 2 ;;
  esac
done

say() { printf '\n== %s\n' "$*"; }
die() { echo "오류: $*" >&2; exit 1; }

command -v docker >/dev/null || die "docker 가 없다"
docker compose version >/dev/null 2>&1 || die "docker compose(v2)가 없다"
docker info >/dev/null 2>&1 || die "docker 데몬이 꺼져 있다(Docker Desktop 을 켠다)"

# HOST_IP: 환경변수 > .env > 127.0.0.1
if [[ ! -f .env ]]; then cp .env.example .env; fi
if [[ -n "${HOST_IP:-}" ]]; then
  if grep -q '^HOST_IP=' .env; then sed -i.bak "s|^HOST_IP=.*|HOST_IP=$HOST_IP|" .env && rm -f .env.bak
  else echo "HOST_IP=$HOST_IP" >> .env; fi
fi
HOST_IP="$(grep '^HOST_IP=' .env | tail -1 | cut -d= -f2)"; HOST_IP="${HOST_IP:-127.0.0.1}"
export HOST_IP

# 컨테이너에 마운트할 Linux 바이너리 위치(compose 의 CHALKAK_SERVER_BIN, CHALKAK_FAKES_BIN)
export CHALKAK_SERVER_BIN="$STATE/bin/chalkak-server"
export CHALKAK_FAKES_BIN="$STATE/bin/chalkak-fakes"

files=(-f compose.yaml)
if [[ $real_oidc -eq 1 ]]; then
  [[ -f env/real-oidc.env ]] || die "env/real-oidc.env 가 없다(env/real-oidc.env.example 을 복사해 채운다)"
  files+=(-f compose.real-oidc.yaml)
fi
dc() { docker compose "${files[@]}" --profile rust "$@"; }

case "$cmd" in
  down)  dc down; exit 0 ;;
  reset) dc down -v; rm -rf "$STATE/caddy"; echo "DB·큐·로컬 CA 를 지웠다(CA 를 지웠으므로 기기에 CA 를 다시 설치해야 한다)"; exit 0 ;;
  status)
    dc ps
    echo; echo "API: https://api.${HOST_IP}.sslip.io/api/v1/   (디버그 http://127.0.0.1:18080)"
    echo "로컬 CA: $STATE/caddy/data/caddy/pki/authorities/local/root.crt"
    exit 0 ;;
esac

# ---- 준비 -------------------------------------------------------------------------------------
if (echo > /dev/tcp/127.0.0.1/443) 2>/dev/null && [[ -z "$(dc ps -q edge 2>/dev/null)" ]]; then
  die "호스트 443 포트를 다른 프로세스가 쓰고 있다(presigned URL 서명 때문에 443 고정이다). 비우고 다시 실행한다"
fi

say "1/5 도구 바이너리(imds-mock, toxiproxy)"
scripts/fetch-tools.sh

say "2/5 Lambda 의존성(boto3, Pillow)"
if [[ -d "$STATE/lambda-deps/boto3" && -d "$STATE/lambda-deps/PIL" ]]; then echo "있음: $STATE/lambda-deps"
else scripts/prepare-lambda.sh; fi

say "3/5 Rust 서버·가짜 외부 서버 Linux 바이너리 빌드(Docker 안, 처음엔 몇 분)"
if [[ $rebuild -eq 1 || ! -x "$STATE/bin/chalkak-server" || ! -x "$STATE/bin/chalkak-fakes" ]]; then
  mkdir -p "$STATE/bin"
  # 대상 디렉터리·레지스트리는 이름 있는 볼륨에 둬서 다음 빌드가 증분이 된다. 런타임 이미지(debian trixie)와 같은 glibc.
  docker run --rm \
    -v "$ROOT:/src:ro" -v "$STATE/bin:/out" \
    -v chalkak-cargo-target:/target -v chalkak-cargo-registry:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target -w /src rust:1.97-trixie \
    sh -c 'cargo build --release -p chalkak-server -p chalkak-fakes && cp /target/release/chalkak-server /target/release/chalkak-fakes /out/'
else
  echo "있음: $STATE/bin/chalkak-server (다시 빌드하려면 --rebuild)"
fi

# ---- 기동 -------------------------------------------------------------------------------------
say "4/5 컨테이너 기동(postgres, aws, edge, imds-mock, fakes, lambda-image, esm-poller, rust)"
dc up -d --wait --wait-timeout 300

# ---- 확인 -------------------------------------------------------------------------------------
if [[ $smoke -eq 1 ]]; then
  say "5/5 스모크(가입 -> 로그인 -> 게시물 -> 이미지 Lambda -> 승인 -> 알림)"
  command -v python3 >/dev/null || die "python3 가 없다(--no-smoke 로 생략하거나 설치한다)"
  scripts/smoke.sh
else
  say "5/5 스모크 생략"
fi

cat <<MSG

백엔드 전체가 떠 있다.
  API 주소      https://api.${HOST_IP}.sslip.io/api/v1/
  디버그 포트   http://127.0.0.1:18080 (백엔드 직결), DB 127.0.0.1:15440
  로컬 CA       $STATE/caddy/data/caddy/pki/authorities/local/root.crt   (앱·기기에 설치해야 HTTPS 가 통한다)
  로그          docker compose --profile rust logs -f rust lambda-image esm-poller
  내리기        scripts/up-all.sh down   (DB 까지 지우려면 reset)
iOS 시뮬레이터 연동: docs/local-aws/ios-demo.md
MSG
