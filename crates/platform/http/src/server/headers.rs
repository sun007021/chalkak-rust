//! 순서를 보존하고 같은 이름을 여러 번 담을 수 있는 헤더 목록.

/// 헤더 목록. 삽입 순서와 이름 대소문자를 그대로 보존하고, 조회는 대소문자를 구분하지 않는다.
/// 원본은 `Vary` 헤더를 3개 따로 보내므로 같은 이름을 여러 개 허용해야 한다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    /// 끝에 추가한다. 같은 이름이 이미 있어도 덮어쓰지 않는다.
    pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries.push((name.into(), value.into()));
    }

    /// 같은 이름을 모두 지우고 하나만 둔다(첫 항목 위치에 둔다).
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        match self
            .entries
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(&name))
        {
            Some(i) => {
                self.entries[i] = (name.clone(), value);
                let mut seen = 0usize;
                self.entries.retain(|(n, _)| {
                    if n.eq_ignore_ascii_case(&name) {
                        seen += 1;
                        seen == 1
                    } else {
                        true
                    }
                });
            }
            None => self.entries.push((name, value)),
        }
    }

    /// 첫 번째 값.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 이름이 같은 모든 값(등장 순서).
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// 같은 이름을 모두 지우고 지운 개수를 돌려준다.
    pub fn remove(&mut self, name: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        before - self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 쉼표로 구분된 토큰 목록 헤더(`Connection` 등)에 `token`이 있는지(대소문자 무시).
    pub fn has_token(&self, name: &str, token: &str) -> bool {
        self.get_all(name)
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_order_and_duplicates() {
        let mut h = Headers::new();
        h.append("Vary", "Origin");
        h.append("X-A", "1");
        h.append("Vary", "Access-Control-Request-Method");
        h.append("vary", "Access-Control-Request-Headers");
        let all: Vec<_> = h.get_all("VARY").collect();
        assert_eq!(
            all,
            [
                "Origin",
                "Access-Control-Request-Method",
                "Access-Control-Request-Headers"
            ]
        );
        let names: Vec<_> = h.iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["Vary", "X-A", "Vary", "vary"]);
    }

    #[test]
    fn set_replaces_all_in_first_position() {
        let mut h = Headers::new();
        h.append("A", "1");
        h.append("B", "2");
        h.append("a", "3");
        h.set("A", "9");
        let v: Vec<_> = h.iter().collect();
        assert_eq!(v, [("A", "9"), ("B", "2")]);
        h.set("C", "x");
        assert_eq!(h.get("c"), Some("x"));
    }

    #[test]
    fn remove_and_tokens() {
        let mut h = Headers::new();
        h.append("Connection", "keep-alive, Upgrade");
        h.append("connection", "CLOSE");
        assert!(h.has_token("connection", "close"));
        assert!(h.has_token("Connection", "upgrade"));
        assert!(!h.has_token("Connection", "te"));
        assert_eq!(h.remove("CONNECTION"), 2);
        assert!(h.is_empty());
    }
}
