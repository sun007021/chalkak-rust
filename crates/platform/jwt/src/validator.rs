//! 서명 검증을 통과한 클레임에 적용하는 규칙. 토큰 종류마다 원본이 조립한 `DelegatingOAuth2TokenValidator`를 그대로 옮긴다.
//!
//! | 토큰 | 원본 | 구성 (실행 순서) |
//! | --- | --- | --- |
//! | 액세스 | `JwtAccessTokenProvider#createJwtDecoder` | 시각(30초, exp 필수), 발급자, aud 포함, purpose=ACCESS, sub가 UUID |
//! | 소셜 가입 | `JwtSocialSignupTokenProvider#createJwtDecoder` | 시각(30초), 발급자, aud 포함, purpose=SOCIAL_SIGNUP, iat·exp 필수·iat 미래 30초 |
//! | OIDC ID | `OidcIdTokenDecoderFactory#createDecoder` | 시각(60초 기본값), 발급자, aud 정확히 하나, exp·iat 필수·iat 미래 60초 |
//!
//! 원본과 같게 **모든 검증을 실행**하고 첫 실패를 돌려준다. 단 aud가 없는 토큰은 원본 액세스·가입 토큰 디코더가
//! `NullPointerException`을 던지므로(`ClaimError::AudienceMissing`) 다른 실패와 상관없이 그 오류가 우선한다.
//!
//! "exp가 iat보다 앞선 토큰"은 이 단계 전에 [`Claims`] 생성에서 거절된다(Spring `Jwt` 생성자의 단언).

use crate::Claims;
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    /// `now - 허용오차 > exp` ("Jwt expired at ...").
    Expired,
    /// `now + 허용오차 < nbf` ("Jwt used before ...").
    NotYetValid,
    /// exp 없음 ("exp is required", `setAllowEmptyExpiryClaim(false)`).
    ExpiryRequired,
    /// 필수 시각 클레임이 없거나(iat·exp) iat가 허용 범위보다 미래.
    TimestampsInvalid(String),
    /// iss가 없거나 다름 ("The iss claim is not valid").
    IssuerMismatch,
    /// aud가 기대와 다름.
    AudienceMismatch,
    /// aud가 없거나 비어 있음. 원본 액세스·가입 토큰 디코더는 `NullPointerException`을 던져 401이 아니라
    /// 서블릿 오류(500)로 끝난다. 원본과 응답을 맞추려면 호출 측이 이 값을 500으로 매핑한다.
    AudienceMissing,
    /// 이름이 같은 문자열 클레임이 기대 값과 다름(purpose 등).
    ClaimMismatch(String),
    /// sub가 없거나 `UUID.fromString`이 받지 않는 형식.
    SubjectInvalid,
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClaimError::Expired => write!(f, "Jwt expired"),
            ClaimError::NotYetValid => write!(f, "Jwt used before nbf"),
            ClaimError::ExpiryRequired => write!(f, "exp is required"),
            ClaimError::TimestampsInvalid(m) => write!(f, "Invalid timestamps: {m}"),
            ClaimError::IssuerMismatch => write!(f, "The iss claim is not valid"),
            ClaimError::AudienceMismatch => write!(f, "The aud claim is not valid"),
            ClaimError::AudienceMissing => write!(f, "The aud claim is missing"),
            ClaimError::ClaimMismatch(name) => write!(f, "The {name} claim is not valid"),
            ClaimError::SubjectInvalid => write!(f, "The sub claim is not valid"),
        }
    }
}

impl std::error::Error for ClaimError {}

#[derive(Debug, Clone)]
enum Check {
    /// `JwtTimestampValidator(skew)`. `allow_empty_exp`는 `setAllowEmptyExpiryClaim`.
    Timestamp {
        skew_ms: i64,
        allow_empty_exp: bool,
    },
    Issuer(String),
    /// `jwt.getAudience().contains(expected)`. aud가 없으면 NPE(`AudienceMissing`).
    AudienceContains(String),
    /// `OidcIdTokenAudienceValidator`: 목록 크기가 1이고 기대 값을 포함.
    AudienceOnly(String),
    ClaimEquals {
        name: String,
        value: String,
    },
    /// `UUID.fromString(sub)`가 던지지 않음.
    SubjectJavaUuid,
    /// 소셜 가입 토큰의 `validateRequiredTimestamps`.
    RequiredTimestamps {
        skew_ms: i64,
    },
    /// `OidcIdTokenClaimsValidator`.
    OidcClaims {
        skew_ms: i64,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Validator {
    checks: Vec<Check>,
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl Validator {
    /// 검증이 하나도 없는 빈 검증기. 아래 빌더로 조립한다.
    pub fn new() -> Validator {
        Validator::default()
    }

    /// `JwtAccessTokenProvider`의 디코더 검증. 허용 오차 30초.
    pub fn access_token(issuer: &str, audience: &str) -> Validator {
        let skew = Duration::from_secs(30);
        Validator::new()
            .timestamps(skew, false)
            .issuer(issuer)
            .audience_contains(audience)
            .claim_equals("purpose", "ACCESS")
            .subject_is_java_uuid()
    }

    /// `JwtSocialSignupTokenProvider`의 디코더 검증. 허용 오차 30초.
    pub fn social_signup_token(issuer: &str, audience: &str) -> Validator {
        let skew = Duration::from_secs(30);
        Validator::new()
            .timestamps(skew, true)
            .issuer(issuer)
            .audience_contains(audience)
            .claim_equals("purpose", "SOCIAL_SIGNUP")
            .required_timestamps(skew)
    }

    /// Google·Kakao·Apple ID 토큰 디코더의 검증. 허용 오차는 Spring 기본값 60초.
    pub fn oidc_id_token(issuer: &str, audience: &str) -> Validator {
        let skew = Duration::from_secs(60);
        Validator::new()
            .timestamps(skew, true)
            .issuer(issuer)
            .audience_only(audience)
            .oidc_claims(skew)
    }

    /// `JwtTimestampValidator`: exp(허용 오차 지난 뒤 만료)와 nbf(허용 오차만큼 일찍 허용).
    pub fn timestamps(mut self, skew: Duration, allow_empty_exp: bool) -> Validator {
        self.checks.push(Check::Timestamp {
            skew_ms: millis(skew),
            allow_empty_exp,
        });
        self
    }

    pub fn issuer(mut self, issuer: &str) -> Validator {
        self.checks.push(Check::Issuer(issuer.to_string()));
        self
    }

    pub fn audience_contains(mut self, audience: &str) -> Validator {
        self.checks
            .push(Check::AudienceContains(audience.to_string()));
        self
    }

    pub fn audience_only(mut self, audience: &str) -> Validator {
        self.checks.push(Check::AudienceOnly(audience.to_string()));
        self
    }

    pub fn claim_equals(mut self, name: &str, value: &str) -> Validator {
        self.checks.push(Check::ClaimEquals {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    pub fn subject_is_java_uuid(mut self) -> Validator {
        self.checks.push(Check::SubjectJavaUuid);
        self
    }

    pub fn required_timestamps(mut self, skew: Duration) -> Validator {
        self.checks.push(Check::RequiredTimestamps {
            skew_ms: millis(skew),
        });
        self
    }

    pub fn oidc_claims(mut self, skew: Duration) -> Validator {
        self.checks.push(Check::OidcClaims {
            skew_ms: millis(skew),
        });
        self
    }

    /// `now_ms`는 에포크 밀리초. 모든 검증을 실행하고 첫 실패를 돌려준다(aud 누락은 항상 우선).
    pub fn validate(&self, claims: &Claims, now_ms: i64) -> Result<(), ClaimError> {
        let mut first = None;
        for check in &self.checks {
            if let Err(e) = run(check, claims, now_ms) {
                if e == ClaimError::AudienceMissing {
                    return Err(e);
                }
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }
}

fn to_ms(seconds: i64) -> i64 {
    seconds.saturating_mul(1000)
}

fn run(check: &Check, claims: &Claims, now_ms: i64) -> Result<(), ClaimError> {
    match check {
        Check::Timestamp {
            skew_ms,
            allow_empty_exp,
        } => {
            match claims.expires_at() {
                Some(exp) if now_ms.saturating_sub(*skew_ms) > to_ms(exp) => {
                    return Err(ClaimError::Expired);
                }
                None if !allow_empty_exp => return Err(ClaimError::ExpiryRequired),
                _ => {}
            }
            match claims.not_before() {
                Some(nbf) if now_ms.saturating_add(*skew_ms) < to_ms(nbf) => {
                    Err(ClaimError::NotYetValid)
                }
                _ => Ok(()),
            }
        }
        Check::Issuer(expected) => {
            if claims.issuer() == Some(expected.as_str()) {
                Ok(())
            } else {
                Err(ClaimError::IssuerMismatch)
            }
        }
        Check::AudienceContains(expected) => match claims.audience() {
            None => Err(ClaimError::AudienceMissing),
            Some(list) if list.contains(&expected.as_str()) => Ok(()),
            Some(_) => Err(ClaimError::AudienceMismatch),
        },
        Check::AudienceOnly(expected) => match claims.audience() {
            Some(list) if list.len() == 1 && list[0] == expected => Ok(()),
            _ => Err(ClaimError::AudienceMismatch),
        },
        Check::ClaimEquals { name, value } => {
            if claims.get_str(name) == Some(value.as_str()) {
                Ok(())
            } else {
                Err(ClaimError::ClaimMismatch(name.clone()))
            }
        }
        Check::SubjectJavaUuid => match claims.subject() {
            Some(sub) if java_uuid_accepts(sub) => Ok(()),
            _ => Err(ClaimError::SubjectInvalid),
        },
        Check::RequiredTimestamps { skew_ms } => {
            let (Some(iat), Some(exp)) = (claims.issued_at(), claims.expires_at()) else {
                return Err(ClaimError::TimestampsInvalid(
                    "iat and exp are required".into(),
                ));
            };
            if to_ms(iat) > now_ms.saturating_add(*skew_ms) {
                return Err(ClaimError::TimestampsInvalid("iat is in the future".into()));
            }
            if exp <= iat {
                return Err(ClaimError::TimestampsInvalid(
                    "exp must be after iat".into(),
                ));
            }
            Ok(())
        }
        Check::OidcClaims { skew_ms } => {
            if claims.expires_at().is_none() {
                return Err(ClaimError::TimestampsInvalid("exp claim is missing".into()));
            }
            let Some(iat) = claims.issued_at() else {
                return Err(ClaimError::TimestampsInvalid("iat claim is missing".into()));
            };
            if to_ms(iat) > now_ms.saturating_add(*skew_ms) {
                return Err(ClaimError::TimestampsInvalid("iat is in the future".into()));
            }
            Ok(())
        }
    }
}

/// `java.util.UUID.fromString`이 예외 없이 받는 문자열인지.
///
/// 표준 형식만 받는 것이 아니다. 길이 36 이하, `-`가 정확히 4개, 다섯 성분 각각이
/// `Long.parseLong(성분, 16)`을 통과하면 된다(앞의 `+`와 앞자리 0 허용, 값은 i64 범위). 예: `1-2-3-4-5`, 대문자.
fn java_uuid_accepts(text: &str) -> bool {
    if text.len() > 36 || text.matches('-').count() != 4 {
        return false;
    }
    text.split('-').all(parse_long_radix16_ok)
}

fn parse_long_radix16_ok(part: &str) -> bool {
    let digits = part.strip_prefix('+').unwrap_or(part);
    if digits.is_empty() {
        return false;
    }
    let mut value: u128 = 0;
    for c in digits.chars() {
        let Some(d) = c.to_digit(16) else {
            return false;
        };
        value = value * 16 + u128::from(d);
        if value > i64::MAX as u128 {
            return false;
        }
    }
    true
}
