//! JSON 본문을 YAML로 바꾼다. `Accept: application/yaml`을 받은 컨트롤러 응답(Spring이 `jackson-dataformat-yaml`로 쓴다).
//!
//! 키 순서를 지켜야 해서 `serde_json::Value`(키를 정렬한다)를 쓰지 않고 작은 파서를 둔다. 출력 모양은 java-3에
//! `Accept: application/yaml`을 보내 얻은 것이다: 문서 시작 `---`, 문자열은 항상 큰따옴표, 배열은 키와 같은 들여쓰기에서
//! `- `로 시작한다. 긴 문자열 줄바꿈(snakeyaml 80칸 접기)은 하지 않는다.
//!
//! YAML 매퍼는 Boot가 설정하지 않은 기본 `ObjectMapper`라 날짜·시각이 숫자로 나간다. 타입 정보가 없어 필드 이름으로 고른다:
//! `…At`(Instant)은 `초.나노초(9자리)`, `topicDate`(LocalDate)는 `[연, 월, 일]`, Boot 오류 본문의 `timestamp`(Date)는
//! 밀리초다. 값이 해당 형식 문자열일 때만 바꾼다.

enum Node {
    Null,
    Bool(bool),
    /// JSON에 적힌 그대로의 숫자.
    Number(String),
    Text(String),
    List(Vec<Node>),
    Map(Vec<(String, Node)>),
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> Option<()> {
        (self.peek() == Some(b)).then(|| self.pos += 1)
    }

    fn literal(&mut self, word: &str) -> Option<()> {
        self.bytes[self.pos..]
            .starts_with(word.as_bytes())
            .then(|| self.pos += word.len())
    }

    fn value(&mut self) -> Option<Node> {
        self.skip_ws();
        match self.peek()? {
            b'{' => {
                self.pos += 1;
                let mut pairs = Vec::new();
                self.skip_ws();
                if self.eat(b'}').is_some() {
                    return Some(Node::Map(pairs));
                }
                loop {
                    self.skip_ws();
                    let key = self.string()?;
                    self.skip_ws();
                    self.eat(b':')?;
                    pairs.push((key, self.value()?));
                    self.skip_ws();
                    if self.eat(b',').is_none() {
                        self.eat(b'}')?;
                        return Some(Node::Map(pairs));
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if self.eat(b']').is_some() {
                    return Some(Node::List(items));
                }
                loop {
                    items.push(self.value()?);
                    self.skip_ws();
                    if self.eat(b',').is_none() {
                        self.eat(b']')?;
                        return Some(Node::List(items));
                    }
                }
            }
            b'"' => self.string().map(Node::Text),
            b't' => self.literal("true").map(|()| Node::Bool(true)),
            b'f' => self.literal("false").map(|()| Node::Bool(false)),
            b'n' => self.literal("null").map(|()| Node::Null),
            _ => {
                let start = self.pos;
                while self.peek().is_some_and(|b| {
                    b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E')
                }) {
                    self.pos += 1;
                }
                (self.pos > start).then(|| {
                    Node::Number(String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned())
                })
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let b = self.peek()?;
            self.pos += 1;
            match b {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = self.peek()?;
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex = std::str::from_utf8(self.bytes.get(self.pos..self.pos + 4)?)
                                .ok()?;
                            let mut unit = u32::from_str_radix(hex, 16).ok()?;
                            self.pos += 4;
                            if (0xD800..0xDC00).contains(&unit)
                                && self.bytes.get(self.pos..self.pos + 2) == Some(b"\\u")
                            {
                                let low = std::str::from_utf8(
                                    self.bytes.get(self.pos + 2..self.pos + 6)?,
                                )
                                .ok()?;
                                let low = u32::from_str_radix(low, 16).ok()?;
                                self.pos += 6;
                                unit = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                            }
                            let c = char::from_u32(unit)?;
                            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
    }
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn scalar(node: &Node) -> Option<String> {
    Some(match node {
        Node::Null => "null".to_owned(),
        Node::Bool(b) => b.to_string(),
        Node::Number(n) => n.clone(),
        Node::Text(s) => quote(s),
        Node::List(items) if items.is_empty() => "[]".to_owned(),
        Node::Map(pairs) if pairs.is_empty() => "{}".to_owned(),
        _ => return None,
    })
}

/// 매핑의 키-값을 `indent`칸 들여쓴 줄들로 쓴다.
fn write_map(pairs: &[(String, Node)], indent: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(indent);
    for (key, node) in pairs {
        match scalar(node) {
            Some(text) => out.push(format!("{pad}{key}: {text}")),
            None => {
                out.push(format!("{pad}{key}:"));
                match node {
                    Node::Map(inner) => write_map(inner, indent + 2, out),
                    Node::List(items) => write_list(items, indent, out),
                    _ => {}
                }
            }
        }
    }
}

/// 배열 항목을 `indent`칸에서 `- `로 쓴다.
fn write_list(items: &[Node], indent: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(indent);
    for node in items {
        match scalar(node) {
            Some(text) => out.push(format!("{pad}- {text}")),
            None => {
                let start = out.len();
                match node {
                    Node::Map(inner) => write_map(inner, indent + 2, out),
                    Node::List(inner) => write_list(inner, indent + 2, out),
                    _ => {}
                }
                // 첫 줄의 들여쓰기 두 칸을 `- `로 바꾼다.
                let first = &mut out[start];
                *first = format!("{pad}- {}", &first[indent + 2..]);
            }
        }
    }
}

/// 1970-01-01부터 센 날짜 번호(proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD`를 (연, 월, 일)로.
fn parse_date(s: &str) -> Option<(i64, i64, i64)> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    Some((num(0..4)?, num(5..7)?, num(8..10)?))
}

/// `YYYY-MM-DDTHH:MM:SS[.f…]Z`를 (초, 나노초)로.
fn parse_instant(s: &str) -> Option<(i64, u32)> {
    let body = s.strip_suffix('Z')?;
    let (date, time) = body.split_once('T')?;
    let (y, m, d) = parse_date(date)?;
    let (clock, fraction) = match time.split_once('.') {
        Some((c, f)) => (c, f),
        None => (time, ""),
    };
    let mut parts = clock.split(':');
    let (h, mi, sec) = (
        parts.next()?.parse::<i64>().ok()?,
        parts.next()?.parse::<i64>().ok()?,
        parts.next()?.parse::<i64>().ok()?,
    );
    if parts.next().is_some() || fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let nanos = format!("{fraction:0<9}").parse::<u32>().ok()?;
    Some((
        days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + sec,
        nanos,
    ))
}

/// 필드 이름으로 날짜·시각 문자열을 원본 YAML 매퍼의 숫자 표현으로 바꾼다.
fn retype(key: &str, node: Node) -> Node {
    let Node::Text(text) = &node else {
        return node;
    };
    if key == "timestamp"
        && let Some((secs, nanos)) = parse_instant(text)
    {
        return Node::Number((secs * 1000 + i64::from(nanos / 1_000_000)).to_string());
    }
    if key.ends_with("At")
        && let Some((secs, nanos)) = parse_instant(text)
    {
        return Node::Number(format!("{secs}.{nanos:09}"));
    }
    if key == "topicDate"
        && let Some((y, m, d)) = parse_date(text)
    {
        return Node::List(vec![
            Node::Number(y.to_string()),
            Node::Number(m.to_string()),
            Node::Number(d.to_string()),
        ]);
    }
    node
}

fn retype_all(node: Node) -> Node {
    match node {
        Node::Map(pairs) => Node::Map(
            pairs
                .into_iter()
                .map(|(k, v)| {
                    let v = retype(&k, retype_all(v));
                    (k, v)
                })
                .collect(),
        ),
        Node::List(items) => Node::List(items.into_iter().map(retype_all).collect()),
        other => other,
    }
}

/// JSON 본문을 YAML 문서로 바꾼다. JSON으로 읽을 수 없으면 `None`.
pub fn from_json(json: &[u8]) -> Option<Vec<u8>> {
    let mut parser = Parser {
        bytes: json,
        pos: 0,
    };
    let root = retype_all(parser.value()?);
    parser.skip_ws();
    if parser.pos != json.len() {
        return None;
    }
    let mut lines = vec!["---".to_owned()];
    match &root {
        Node::Map(pairs) if !pairs.is_empty() => write_map(pairs, 0, &mut lines),
        Node::List(items) if !items.is_empty() => write_list(items, 0, &mut lines),
        other => lines.push(scalar(other)?),
    }
    let mut text = lines.join("\n");
    text.push('\n');
    Some(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(json: &str) -> String {
        String::from_utf8(from_json(json.as_bytes()).unwrap()).unwrap()
    }

    // java-3: GET /v3/api-docs Accept: application/yaml -> 406 오류 본문
    #[test]
    fn 오류_본문은_원본과_같다() {
        assert_eq!(
            yaml(
                r#"{"errorCode":"BUSINESS_ERROR","message":"지원하지 않는 요청 방식이거나 형식입니다."}"#
            ),
            "---\nerrorCode: \"BUSINESS_ERROR\"\nmessage: \"지원하지 않는 요청 방식이거나 형식입니다.\"\n"
        );
    }

    #[test]
    fn 키_순서를_지키고_중첩을_들여쓴다() {
        let out = yaml(
            r#"{"z":1,"a":{"b":[1,2],"c":{"d":null}},"list":[{"x":"1","y":true},{"x":"2"}],"e":[],"f":{},"g":[[1,2],[3]]}"#,
        );
        assert_eq!(
            out,
            "---\nz: 1\na:\n  b:\n  - 1\n  - 2\n  c:\n    d: null\nlist:\n- x: \"1\"\n  y: true\n- x: \"2\"\ne: []\nf: {}\ng:\n- - 1\n  - 2\n- - 3\n"
        );
    }

    #[test]
    fn 문자열은_이스케이프한다() {
        assert_eq!(
            yaml(r#"{"a":"q\"b\\c\nd\u0001é"}"#),
            "---\na: \"q\\\"b\\\\c\\nd\\x01é\"\n"
        );
    }

    // java-3: Boot 오류 본문 timestamp는 밀리초, Instant는 초.나노초, LocalDate는 [연,월,일]
    #[test]
    fn 날짜와_시각은_숫자로_바꾼다() {
        assert_eq!(
            yaml(
                r#"{"timestamp":"2026-10-12T00:00:01.401Z","createdAt":"2026-10-12T00:00:01.953659Z","readAt":"2026-10-12T00:00:01Z","topicDate":"2026-10-12","note":"2026-10-12"}"#
            ),
            "---\ntimestamp: 1791763201401\ncreatedAt: 1791763201.953659000\nreadAt: 1791763201.000000000\ntopicDate:\n- 2026\n- 10\n- 12\nnote: \"2026-10-12\"\n"
        );
        // 형식이 다르면 문자열 그대로
        assert_eq!(
            yaml(r#"{"createdAt":"어제"}"#),
            "---\ncreatedAt: \"어제\"\n"
        );
    }

    #[test]
    fn json이_아니면_none이다() {
        assert!(from_json(b"{").is_none());
        assert!(from_json(b"{} x").is_none());
    }
}
