//! 서명 검증을 통과한 페이로드를 Spring `Jwt`가 보는 형태로 옮긴다.
//!
//! 근거: Nimbus `JWTClaimsSet.parse`("Malformed payload")와 Spring `MappedJwtClaimSetConverter` + `Jwt` 생성자.
//! - `exp` `iat` `nbf`는 JSON 숫자(정수 또는 실수, 초 단위로 내림). 문자열이면 형식 오류.
//! - `iss` `sub` `jti`는 문자열. `aud`는 문자열 또는 문자열 배열(빈 배열은 없는 것과 같다).
//! - 값이 `null`인 클레임은 없는 것으로 본다.
//! - `iat`와 `exp`가 모두 있으면 `exp`가 `iat`보다 뒤여야 한다. 같거나 앞서면 허용 오차와 상관없이 거절한다
//!   (`Jwt` 생성자의 `Assert.isTrue(expiresAt.isAfter(issuedAt))`가 던지는 `BadJwtException`).

use crate::JwtError;
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Claims {
    map: Map<String, Value>,
    exp: Option<i64>,
    iat: Option<i64>,
    nbf: Option<i64>,
}

fn malformed(name: &str) -> JwtError {
    JwtError::Malformed(format!("Malformed payload: invalid {name} claim"))
}

fn seconds(map: &Map<String, Value>, name: &str) -> Result<Option<i64>, JwtError> {
    match map.get(name) {
        None => Ok(None),
        Some(Value::Number(n)) => {
            Ok(Some(n.as_i64().unwrap_or_else(|| {
                n.as_f64().map_or(0, |f| f.floor() as i64)
            })))
        }
        Some(_) => Err(malformed(name)),
    }
}

impl Claims {
    pub fn from_slice(payload: &[u8]) -> Result<Claims, JwtError> {
        match serde_json::from_slice::<Value>(payload) {
            Ok(Value::Object(map)) => Claims::from_map(map),
            _ => Err(JwtError::Malformed("Malformed payload".into())),
        }
    }

    pub fn from_map(mut map: Map<String, Value>) -> Result<Claims, JwtError> {
        map.retain(|_, v| !v.is_null());
        for name in ["iss", "sub", "jti"] {
            if map.get(name).is_some_and(|v| !v.is_string()) {
                return Err(malformed(name));
            }
        }
        match map.get("aud") {
            None | Some(Value::String(_)) => {}
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => {}
            Some(_) => return Err(malformed("aud")),
        }
        let exp = seconds(&map, "exp")?;
        let iat = seconds(&map, "iat")?;
        let nbf = seconds(&map, "nbf")?;
        if let (Some(iat), Some(exp)) = (iat, exp)
            && exp <= iat
        {
            return Err(JwtError::InvalidClaims(
                "expiresAt must be after issuedAt".into(),
            ));
        }
        Ok(Claims { map, exp, iat, nbf })
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.map
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.map.get(name)
    }

    /// 문자열 클레임. 없거나 문자열이 아니면 `None`.
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.map.get(name).and_then(Value::as_str)
    }

    pub fn issuer(&self) -> Option<&str> {
        self.get_str("iss")
    }

    pub fn subject(&self) -> Option<&str> {
        self.get_str("sub")
    }

    pub fn jwt_id(&self) -> Option<&str> {
        self.get_str("jti")
    }

    /// Spring `Jwt.getAudience()`: 문자열 하나도 목록으로 본다. 클레임이 없거나 null이면 `None`(원본은 NPE).
    ///
    /// 빈 배열이나 문자열이 아닌 값(`[]`, `5`)은 `None`이 아니다. 원본 java-a가 이런 토큰을 401(audience 불일치)로
    /// 처리하므로 빈 목록으로 돌려준다.
    pub fn audience(&self) -> Option<Vec<&str>> {
        match self.map.get("aud")? {
            Value::Null => None,
            Value::String(s) => Some(vec![s.as_str()]),
            Value::Array(items) => Some(items.iter().filter_map(Value::as_str).collect()),
            _ => Some(Vec::new()),
        }
    }

    /// 에포크 초.
    pub fn expires_at(&self) -> Option<i64> {
        self.exp
    }

    pub fn issued_at(&self) -> Option<i64> {
        self.iat
    }

    pub fn not_before(&self) -> Option<i64> {
        self.nbf
    }
}
