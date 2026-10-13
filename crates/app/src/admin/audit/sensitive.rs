//! 원본 `AdminAuditSensitiveValueValidator`. 감사 로그 상태 값과 사유에 숨은 민감정보를 찾는다.
//!
//! 원본은 정규식 10개 중 하나라도 `find()`되면 거절한다. 정규식 crate를 쓰지 않으므로 패턴마다 손으로 옮겼다.
//! 자바 정규식의 성질을 그대로 따른다.
//! - `CASE_INSENSITIVE`는 ASCII 대소문자만 같게 본다(`UNICODE_CASE` 없음).
//! - `\s`는 `[ \t\n\x0B\f\r]`, `\S`는 그 밖의 모든 문자, `\d`는 ASCII 숫자.
//! - `\b`는 `[A-Za-z0-9_]`와 그 밖의 문자 사이의 경계다.
//!
//! 패턴은 `find()`(앞뒤 고정 없음)로 쓰이므로 "그런 부분 문자열이 있는가"만 판정하면 된다.

/// 원본 패턴 중 하나라도 값 안에서 찾아지면 `true`.
pub fn contains_sensitive_value(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars.iter().map(char::to_ascii_lowercase).collect();
    let m = Text {
        chars: &chars,
        lower: &lower,
    };
    m.bearer()
        || m.credential_assignment()
        || m.slack_webhook()
        || m.fcm_send_url()
        || m.fcm_registration_token()
        || m.image_url()
        || m.image_data_uri()
        || m.signed_url_parameter()
        || m.bcrypt_hash()
        || m.jwt()
}

struct Text<'a> {
    chars: &'a [char],
    /// ASCII 소문자로 바꾼 같은 길이의 문자열(대소문자 무시 패턴용).
    lower: &'a [char],
}

fn is_regex_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r')
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `[A-Za-z0-9_-]`
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

impl Text<'_> {
    fn len(&self) -> usize {
        self.chars.len()
    }

    /// `lower[at..]`이 `literal`(소문자)로 시작하는가.
    fn lit(&self, at: usize, literal: &str) -> bool {
        for (i, expected) in (at..).zip(literal.chars()) {
            if self.lower.get(i) != Some(&expected) {
                return false;
            }
        }
        true
    }

    /// `\S+`가 `at`에서 시작해 한 글자 이상 잡히는가(`at`의 글자가 공백이 아니면 된다).
    fn non_space_at(&self, at: usize) -> bool {
        self.chars.get(at).is_some_and(|c| !is_regex_space(*c))
    }

    fn skip_spaces(&self, mut at: usize) -> usize {
        while self.chars.get(at).is_some_and(|c| is_regex_space(*c)) {
            at += 1;
        }
        at
    }

    /// `\b` + 단어 문자로 시작하는 패턴의 앞 경계.
    fn word_boundary_before(&self, at: usize) -> bool {
        at == 0 || !is_word(self.chars[at - 1])
    }

    /// `\bbearer\s+\S+`
    fn bearer(&self) -> bool {
        (0..self.len()).any(|i| {
            if !self.word_boundary_before(i) || !self.lit(i, "bearer") {
                return false;
            }
            let after_word = i + "bearer".len();
            let after_spaces = self.skip_spaces(after_word);
            after_spaces > after_word && self.non_space_at(after_spaces)
        })
    }

    /// `\b(?:password|passwd|token|...)\s*[:=]\s*\S+`
    fn credential_assignment(&self) -> bool {
        // `~`는 `[_-]?`.
        const KEYWORDS: [&str; 23] = [
            "password",
            "passwd",
            "token",
            "access~token",
            "refresh~token",
            "id~token",
            "api~key",
            "secret",
            "credential",
            "authorization",
            "cookie",
            "webhook",
            "webhook~url",
            "fcm~token",
            "registration~token",
            "device~token",
            "push~token",
            "storage~key",
            "image~url",
            "photo~url",
            "thumbnail~url",
            "signature~key",
            "signature~url",
        ];
        (0..self.len()).any(|i| {
            if !self.word_boundary_before(i) {
                return false;
            }
            KEYWORDS.iter().any(|template| {
                self.template_ends(i, template.as_bytes())
                    .into_iter()
                    .any(|end| self.assignment_after(end))
            })
        })
    }

    /// 키워드 뒤의 `\s*[:=]\s*\S+`.
    fn assignment_after(&self, at: usize) -> bool {
        let at = self.skip_spaces(at);
        if !matches!(self.chars.get(at), Some(':' | '=')) {
            return false;
        }
        let at = self.skip_spaces(at + 1);
        self.non_space_at(at)
    }

    /// `template`(`~` = 선택적 `[_-]`)이 `at`에서 끝날 수 있는 모든 위치.
    fn template_ends(&self, at: usize, template: &[u8]) -> Vec<usize> {
        let Some((&first, rest)) = template.split_first() else {
            return vec![at];
        };
        if first == b'~' {
            let mut ends = self.template_ends(at, rest);
            if matches!(self.chars.get(at), Some('_' | '-')) {
                ends.extend(self.template_ends(at + 1, rest));
            }
            return ends;
        }
        if self.lower.get(at) == Some(&char::from(first)) {
            self.template_ends(at + 1, rest)
        } else {
            Vec::new()
        }
    }

    /// `https://hooks\.slack(?:-gov)?\.com/services/\S+`
    fn slack_webhook(&self) -> bool {
        const HEAD: &str = "https://hooks.slack";
        const TAIL: &str = ".com/services/";
        (0..self.len()).any(|i| {
            if !self.lit(i, HEAD) {
                return false;
            }
            let after = i + HEAD.len();
            let tail_ok =
                |start: usize| self.lit(start, TAIL) && self.non_space_at(start + TAIL.len());
            tail_ok(after) || (self.lit(after, "-gov") && tail_ok(after + "-gov".len()))
        })
    }

    /// `https://fcm\.googleapis\.com/fcm/send/\S+`
    fn fcm_send_url(&self) -> bool {
        const HEAD: &str = "https://fcm.googleapis.com/fcm/send/";
        (0..self.len()).any(|i| self.lit(i, HEAD) && self.non_space_at(i + HEAD.len()))
    }

    /// `(?:[A-Za-z0-9_-]{10,}:)?APA91[A-Za-z0-9_-]{20,}`. 앞의 선택 부분은 찾는 결과를 바꾸지 않는다.
    fn fcm_registration_token(&self) -> bool {
        const HEAD: &str = "apa91";
        (0..self.len()).any(|i| {
            if !self.lit(i, HEAD) {
                return false;
            }
            let start = i + HEAD.len();
            (start..start + 20).all(|k| self.chars.get(k).is_some_and(|c| is_token_char(*c)))
        })
    }

    /// `https?://[^\s?#]+\.(?:png|jpe?g|webp|gif|avif|heic|svg)(?:[?#]\S*)?`
    fn image_url(&self) -> bool {
        const EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "webp", "gif", "avif", "heic", "svg"];
        (0..self.len()).any(|i| {
            if !self.lit(i, "http") {
                return false;
            }
            let mut at = i + 4;
            if self.lit(at, "s") {
                at += 1;
            }
            if !self.lit(at, "://") {
                return false;
            }
            let run_start = at + 3;
            let mut run_end = run_start;
            while self
                .chars
                .get(run_end)
                .is_some_and(|c| !is_regex_space(*c) && *c != '?' && *c != '#')
            {
                run_end += 1;
            }
            // `[^\s?#]+`가 한 글자 이상 잡은 뒤 `\.`가 와야 하므로 점은 둘째 글자부터다.
            (run_start + 1..run_end).any(|dot| {
                self.chars[dot] == '.' && EXTENSIONS.iter().any(|ext| self.lit(dot + 1, ext))
            })
        })
    }

    /// `data:image/[^;\s]+;base64,\S+`
    fn image_data_uri(&self) -> bool {
        const HEAD: &str = "data:image/";
        const MARK: &str = ";base64,";
        (0..self.len()).any(|i| {
            if !self.lit(i, HEAD) {
                return false;
            }
            let run_start = i + HEAD.len();
            let mut run_end = run_start;
            while self
                .chars
                .get(run_end)
                .is_some_and(|c| *c != ';' && !is_regex_space(*c))
            {
                run_end += 1;
            }
            run_end > run_start
                && self.lit(run_end, MARK)
                && self.non_space_at(run_end + MARK.len())
        })
    }

    /// `(?:x-amz-(?:credential|signature|security-token)|x-goog-signature)=\S+`
    fn signed_url_parameter(&self) -> bool {
        const HEADS: [&str; 4] = [
            "x-amz-credential=",
            "x-amz-signature=",
            "x-amz-security-token=",
            "x-goog-signature=",
        ];
        (0..self.len()).any(|i| {
            HEADS
                .iter()
                .any(|head| self.lit(i, head) && self.non_space_at(i + head.len()))
        })
    }

    /// `\$2[aby]\$\d{2}\$[./A-Za-z0-9]{53}` (대소문자 구분)
    fn bcrypt_hash(&self) -> bool {
        let c = self.chars;
        (0..self.len()).any(|i| {
            let at = |k: usize| c.get(i + k).copied();
            at(0) == Some('$')
                && at(1) == Some('2')
                && matches!(at(2), Some('a' | 'b' | 'y'))
                && at(3) == Some('$')
                && at(4).is_some_and(|d| d.is_ascii_digit())
                && at(5).is_some_and(|d| d.is_ascii_digit())
                && at(6) == Some('$')
                && (7..7 + 53).all(|k| {
                    at(k).is_some_and(|d| d.is_ascii_alphanumeric() || d == '.' || d == '/')
                })
        })
    }

    /// `\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b` (대소문자 구분)
    fn jwt(&self) -> bool {
        let c = self.chars;
        let run_end = |from: usize| {
            let mut end = from;
            while c.get(end).is_some_and(|ch| is_token_char(*ch)) {
                end += 1;
            }
            end
        };
        (0..self.len()).any(|i| {
            if !self.word_boundary_before(i) || c.get(i..i + 3) != Some(&['e', 'y', 'J']) {
                return false;
            }
            let first_end = run_end(i + 3);
            if first_end == i + 3 || c.get(first_end) != Some(&'.') {
                return false;
            }
            let second_start = first_end + 1;
            let second_end = run_end(second_start);
            if second_end == second_start || c.get(second_end) != Some(&'.') {
                return false;
            }
            let third_start = second_end + 1;
            let third_end = run_end(third_start);
            // 마지막 `+`는 줄어들 수 있으므로 `\b`가 성립하는 끝을 찾는다.
            (third_start + 1..=third_end).any(|end| {
                let before = is_word(c[end - 1]);
                let after = c.get(end).is_some_and(|ch| is_word(*ch));
                before != after
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(s: &str) -> bool {
        contains_sensitive_value(s)
    }

    // AdminAuditSnapshotTest#from_sensitiveStringValue_throwsBusinessException,
    // AdminAuditLogTest#create_reasonContainingSensitiveValue_throwsBusinessException
    #[test]
    fn 원본_테스트의_민감값을_찾는다() {
        for value in [
            "accessToken=secret-value",
            "처리 사유 accessToken=secret-value",
            "https://hooks.slack.com/services/T000/B000/SECRET",
            "fcmToken=fake-device-registration-token",
            "https://storage.example.com/private/photo.webp?version=1",
            "data:image/png;base64,AAAA",
        ] {
            assert!(hit(value), "{value}");
        }
    }

    // AdminAuditSnapshotTest#from_benignStringValue_createsSnapshot
    #[test]
    fn 일반_문장과_문서_url은_통과한다() {
        for value in [
            "토큰 만료 관련 문의: https://docs.example.com/admin/moderation",
            "검수 기준 충족",
            "",
            "token",
            "token expired",
            "bearer",
            "bearer ",
            "https://example.com/page",
        ] {
            assert!(!hit(value), "{value}");
        }
    }

    #[test]
    fn bearer는_단어_경계와_공백_뒤_값이_필요하다() {
        assert!(hit("Bearer abc"));
        assert!(hit("see BEARER\tabc"));
        assert!(hit("Authorization: Bearer abc"));
        assert!(!hit("bearer"));
        assert!(!hit("bearer   "));
        assert!(!hit("xbearer abc"));
        assert!(!hit("bearerabc def"));
        assert!(hit("_x bearer abc"));
        assert!(!hit("_bearer abc"));
    }

    #[test]
    fn 자격_증명_대입은_구분자와_값이_있어야_한다() {
        for value in [
            "password=x",
            "PASSWORD : x",
            "passwd: x",
            "token=x",
            "access-token=x",
            "access_token = x",
            "accesstoken=x",
            "refreshToken=x",
            "refresh-token:x",
            "id_token=x",
            "api_key=x",
            "apikey=x",
            "secret=x",
            "credential=x",
            "authorization:x",
            "cookie=x",
            "webhook=x",
            "webhook-url=x",
            "webhookurl=x",
            "fcm_token=x",
            "registrationToken=x",
            "deviceToken=x",
            "pushToken=x",
            "storage_key=x",
            "imageUrl=x",
            "photo-url=x",
            "thumbnail_url=x",
            "signatureKey=x",
            "signature-url=x",
            "token==",
            "token=  value",
        ] {
            assert!(hit(value), "{value}");
        }
        for value in [
            "password",
            "password=",
            "password =  ",
            "mytoken=x",
            "_token=x",
            "token x",
            "tokens=x",
            "signature=x",
            "webhook_=x",
        ] {
            assert!(!hit(value), "{value}");
        }
    }

    #[test]
    fn 서비스_고유_주소와_토큰() {
        assert!(hit("HTTPS://HOOKS.SLACK.COM/services/x"));
        assert!(hit("https://hooks.slack-gov.com/services/x"));
        assert!(!hit("https://hooks.slack.com/services/"));
        assert!(!hit("https://hooks.slack.com/services/ x"));
        assert!(hit("https://fcm.googleapis.com/fcm/send/abc"));
        assert!(!hit("https://fcm.googleapis.com/fcm/send/"));
        assert!(hit(&format!("APA91{}", "a".repeat(20))));
        assert!(hit(&format!("{}:apa91{}", "x".repeat(12), "B_-".repeat(7))));
        assert!(!hit(&format!("APA91{}", "a".repeat(19))));
        assert!(!hit(&format!("APA9{}", "a".repeat(30))));
    }

    #[test]
    fn 이미지_주소와_데이터_uri() {
        for value in [
            "http://a.png",
            "https://x.test/a.JPG",
            "https://x.test/a.jpeg?x=1",
            "https://x.test/dir/a.b.webp#frag",
            "https://x.test/a.gif",
            "https://x.test/a.avif",
            "https://x.test/a.heic",
            "https://x.test/a.svg",
            "https://x.test/a.pngx",
            "see https://x.test/a.png now",
            "https://x.test/.png",
        ] {
            assert!(hit(value), "{value}");
        }
        for value in [
            "https://x.test/a.pdf",
            "https://x.test/a?file=b.png",
            "https://x.test/a#b.png",
            "ftp://x.test/a.png",
            "https://",
            "https://x.test/a. png",
        ] {
            assert!(!hit(value), "{value}");
        }
        assert!(hit("data:image/png;base64,AAAA"));
        assert!(hit("DATA:IMAGE/svg+xml;BASE64,AA"));
        assert!(!hit("data:image/png;base64,"));
        assert!(!hit("data:image/;base64,AA"));
        assert!(!hit("data:image/png base64,AA"));
        assert!(!hit("data:text/plain;base64,AA"));
    }

    #[test]
    fn 서명_url_매개변수와_해시() {
        assert!(hit("X-Amz-Signature=abc"));
        assert!(hit("?x-amz-credential=abc&y=1"));
        assert!(hit("x-amz-security-token=abc"));
        assert!(hit("X-Goog-Signature=abc"));
        assert!(!hit("x-amz-signature="));
        assert!(!hit("x-amz-date=abc"));

        let hash = format!("$2a$10${}", "a./Z9".repeat(11).get(..53).unwrap());
        assert!(hit(&hash));
        assert!(hit(&format!("x {hash} y").replace("$2a", "$2y")));
        assert!(!hit(&hash.replace("$2a", "$2c")));
        assert!(!hit(&hash[..hash.len() - 1]));
        assert!(!hit(&hash.replace("$10$", "$1x$")));
    }

    #[test]
    fn jwt는_세_구간과_경계가_필요하다() {
        assert!(hit("eyJhbGciOi.eyJzdWIi.sig-_x"));
        assert!(hit("token: eyJa.b.c"));
        assert!(!hit("eyJa.b"));
        assert!(!hit("xeyJa.b.c"));
        assert!(!hit("_eyJa.b.c"));
        assert!(!hit("eyJ.b.c"));
        assert!(!hit("eyja.b.c"));
        // 마지막 구간이 `-`로 끝나도 줄여서 `\b`를 만족한다.
        assert!(hit("eyJa.b.c-"));
        assert!(hit("eyJa.b.c-!"));
        // 앞 구간이 `-`로 끝나면 `\.`가 이어 붙을 수 있다.
        assert!(hit("eyJa-.b.c"));
    }
}
