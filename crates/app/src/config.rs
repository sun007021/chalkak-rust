//! auth·user 도메인이 읽는 설정. 원본 `application*.yml`의 `chalkak.auth.*`, `chalkak.image.*`, `chalkak.callback.*`,
//! `chalkak.user.*`와 같은 이름의 환경변수를 읽는다(Spring 느슨한 바인딩: `chalkak.auth.apple.token.token-uri` ->
//! `CHALKAK_AUTH_APPLE_TOKEN_TOKENURI`).
//!
//! 필수 값이 없거나 형식이 틀리면 기동을 실패시킨다(Spring `@Validated` 속성 바인딩과 같다).

use std::time::Duration;

use chalkak_aws::S3Endpoint;
use chalkak_core::config::{ConfigError, Env};

use crate::common::hex_decode;

/// `chalkak.auth.*`의 발급자·대상. 원본 `application.yml`의 고정값이다.
pub const TOKEN_ISSUER: &str = "chalkak-backend";
pub const ACCESS_TOKEN_AUDIENCE: &str = "chalkak-access";
pub const SIGNUP_TOKEN_AUDIENCE: &str = "chalkak-social-signup";
/// 회원가입 토큰 수명(`chalkak.auth.social-signup-token.expiration`).
pub const SIGNUP_TOKEN_LIFETIME: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct OidcProvider {
    pub issuer: String,
    pub jwks_uri: String,
    /// 우리 앱의 `aud`(Google·Apple은 client-id, Kakao는 app-key).
    pub audience: String,
}

#[derive(Clone, Debug)]
pub struct AppleSettings {
    pub team_id: String,
    pub key_id: String,
    pub private_key_base64: String,
    pub token_uri: String,
    pub revoke_uri: String,
    /// `chalkak.auth.oidc.apple.client-id`: 토큰 교환의 `client_id`와 client secret의 `sub`.
    pub client_id: String,
    /// Apple 리프레시 토큰 AES-256 키(바이트).
    pub encryption_key: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct AuthSettings {
    pub access_token_key: Vec<u8>,
    pub access_token_expiration: Duration,
    pub signup_token_key: Vec<u8>,
    pub social_identity_key: Vec<u8>,
    pub refresh_inactivity: Duration,
    pub refresh_absolute: Duration,
    pub google: OidcProvider,
    pub kakao: OidcProvider,
    pub apple_oidc: OidcProvider,
    pub apple: AppleSettings,
}

#[derive(Clone, Debug)]
pub struct ImageSettings {
    pub bucket: String,
    pub region: String,
    pub base_url: String,
    pub root_prefix: String,
    pub environment: String,
    pub signature_max_bytes: u64,
    pub signature_content_types: Vec<String>,
    pub signature_cache_control: String,
    pub anonymous_access: bool,
    pub endpoint: S3Endpoint,
}

#[derive(Clone, Debug)]
pub struct CallbackSettings {
    pub secret: String,
    pub max_body_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct AppSettings {
    pub auth: AuthSettings,
    pub image: ImageSettings,
    pub callback: CallbackSettings,
    /// `chalkak.user.signature-processing-timeout`.
    pub signature_processing_timeout: Duration,
}

fn invalid(key: &str, reason: &str) -> ConfigError {
    ConfigError::Invalid {
        key: key.to_owned(),
        reason: reason.to_owned(),
    }
}

fn fixed_hex(env: &Env, key: &str, len: usize) -> Result<Vec<u8>, ConfigError> {
    let raw = env.required(key)?;
    if raw.len() != len * 2 {
        return Err(invalid(key, "64자리 16진수여야 합니다"));
    }
    hex_decode(raw).ok_or_else(|| invalid(key, "64자리 16진수여야 합니다"))
}

fn not_blank<'a>(env: &'a Env, key: &str) -> Result<&'a str, ConfigError> {
    let value = env.required(key)?;
    if value.trim().is_empty() {
        return Err(invalid(key, "비어 있을 수 없습니다"));
    }
    Ok(value)
}

fn positive(env: &Env, key: &str) -> Result<Duration, ConfigError> {
    let d = env.required_duration(key)?;
    if d.is_zero() {
        return Err(invalid(key, "0보다 커야 합니다"));
    }
    Ok(d)
}

impl AppSettings {
    /// `image_environment`는 프로필 이름(`local`, `dev`, `prod`)이다. local만 버킷·CDN 주소·prefix에 기본값이 있고
    /// 익명 접근이 기본 켜진다(`application-local.yml`).
    pub fn from_env(
        env: &Env,
        image_environment: &str,
        access_token_key: Vec<u8>,
        access_token_expiration: Duration,
    ) -> Result<AppSettings, ConfigError> {
        let local = image_environment == "local";

        let refresh_inactivity = positive(env, "REFRESH_TOKEN_INACTIVITY_EXPIRATION")?;
        let refresh_absolute = positive(env, "REFRESH_TOKEN_ABSOLUTE_EXPIRATION")?;
        if refresh_inactivity > refresh_absolute {
            return Err(invalid(
                "REFRESH_TOKEN_INACTIVITY_EXPIRATION",
                "비활동 만료는 절대 만료를 넘을 수 없습니다",
            ));
        }

        let apple_key = not_blank(env, "APPLE_PRIVATE_KEY_BASE64")?;
        // 기동 시점에 형식을 확인한다(원본 `AppleClientSecretGenerator` 생성 시 키를 읽는다).
        chalkak_jwt::Es256Signer::from_base64_pem(apple_key)
            .map_err(|e| invalid("APPLE_PRIVATE_KEY_BASE64", &e.to_string()))?;

        let uri = |key: &str, default: &str| env.string_or(key, default).to_owned();
        let auth = AuthSettings {
            access_token_key,
            access_token_expiration,
            signup_token_key: fixed_hex(env, "SOCIAL_SIGNUP_TOKEN_SECRET", 32)?,
            social_identity_key: fixed_hex(env, "SOCIAL_IDENTITY_HMAC_SECRET", 32)?,
            refresh_inactivity,
            refresh_absolute,
            google: OidcProvider {
                issuer: "https://accounts.google.com".to_owned(),
                jwks_uri: uri(
                    "CHALKAK_AUTH_OIDC_GOOGLE_JWKSETURI",
                    "https://www.googleapis.com/oauth2/v3/certs",
                ),
                audience: not_blank(env, "GOOGLE_OIDC_CLIENT_ID")?.to_owned(),
            },
            kakao: OidcProvider {
                issuer: "https://kauth.kakao.com".to_owned(),
                jwks_uri: uri(
                    "CHALKAK_AUTH_OIDC_KAKAO_JWKSETURI",
                    "https://kauth.kakao.com/.well-known/jwks.json",
                ),
                audience: not_blank(env, "KAKAO_OIDC_APP_KEY")?.to_owned(),
            },
            apple_oidc: OidcProvider {
                issuer: "https://appleid.apple.com".to_owned(),
                jwks_uri: uri(
                    "CHALKAK_AUTH_OIDC_APPLE_JWKSETURI",
                    "https://appleid.apple.com/auth/keys",
                ),
                audience: not_blank(env, "APPLE_OIDC_CLIENT_ID")?.to_owned(),
            },
            apple: AppleSettings {
                team_id: not_blank(env, "APPLE_TEAM_ID")?.to_owned(),
                key_id: not_blank(env, "APPLE_KEY_ID")?.to_owned(),
                private_key_base64: not_blank(env, "APPLE_PRIVATE_KEY_BASE64")?.to_owned(),
                token_uri: uri(
                    "CHALKAK_AUTH_APPLE_TOKEN_TOKENURI",
                    "https://appleid.apple.com/auth/token",
                ),
                revoke_uri: uri(
                    "CHALKAK_AUTH_APPLE_TOKEN_REVOKEURI",
                    "https://appleid.apple.com/auth/revoke",
                ),
                client_id: not_blank(env, "APPLE_OIDC_CLIENT_ID")?.to_owned(),
                encryption_key: {
                    let key = fixed_hex(env, "APPLE_REFRESH_TOKEN_ENCRYPTION_KEY", 32)?;
                    <[u8; 32]>::try_from(key.as_slice()).map_err(|_| {
                        invalid("APPLE_REFRESH_TOKEN_ENCRYPTION_KEY", "32바이트여야 합니다")
                    })?
                },
            },
        };

        let pick = |key: &str, local_default: &str| -> Result<String, ConfigError> {
            if local {
                Ok(env.string_or(key, local_default).to_owned())
            } else {
                Ok(env.required(key)?.to_owned())
            }
        };
        let root_prefix = pick("S3_PREFIX", "chalkak")?;
        if root_prefix.is_empty() || root_prefix.split('/').any(str::is_empty) {
            return Err(invalid(
                "S3_PREFIX",
                "root-prefix는 앞뒤 슬래시와 빈 구간 없이 작성해야 합니다",
            ));
        }
        let endpoint = S3Endpoint::from_env(&|name| env.get(name).map(str::to_owned))
            .map_err(|e| invalid("AWS_ENDPOINT_URL_S3", &e.to_string()))?;
        let image = ImageSettings {
            bucket: pick("S3_BUCKET", "chalkak-local-images")?,
            region: env.string_or("AWS_REGION", "ap-northeast-2").to_owned(),
            base_url: pick("CLOUDFRONT_BASE_URL", "http://localhost:4566")?,
            root_prefix,
            environment: image_environment.to_owned(),
            signature_max_bytes: 1_048_576,
            signature_content_types: vec!["image/png".to_owned()],
            signature_cache_control: "public, max-age=86400".to_owned(),
            anonymous_access: env.bool_or("CHALKAK_IMAGE_ANONYMOUSACCESS", local)?,
            endpoint,
        };

        let secret = env.required("IMAGE_PROCESSOR_CALLBACK_SECRET")?;
        if secret.chars().count() < 32 {
            return Err(invalid(
                "IMAGE_PROCESSOR_CALLBACK_SECRET",
                "32자 이상이어야 합니다",
            ));
        }
        let max_body_bytes: u64 = env.required_parsed("CALLBACK_MAX_BODY_BYTES")?;
        if max_body_bytes == 0 {
            return Err(invalid("CALLBACK_MAX_BODY_BYTES", "0보다 커야 합니다"));
        }

        Ok(AppSettings {
            auth,
            image,
            callback: CallbackSettings {
                secret: secret.to_owned(),
                max_body_bytes,
            },
            signature_processing_timeout: positive(env, "SIGNATURE_PROCESSING_TIMEOUT")?,
        })
    }
}

/// 테스트가 `Settings::from_env`에 넘길 수 있는 최소 환경변수(auth·user 도메인 필수 값). 실제 비밀값이 아니다.
#[doc(hidden)]
pub const TEST_ENV: [(&str, &str); 17] = [
    (
        "SOCIAL_SIGNUP_TOKEN_SECRET",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ),
    (
        "SOCIAL_IDENTITY_HMAC_SECRET",
        "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899",
    ),
    ("REFRESH_TOKEN_INACTIVITY_EXPIRATION", "30d"),
    ("REFRESH_TOKEN_ABSOLUTE_EXPIRATION", "90d"),
    ("GOOGLE_OIDC_CLIENT_ID", "test-google-client-id"),
    ("KAKAO_OIDC_APP_KEY", "test-kakao-app-key"),
    ("APPLE_OIDC_CLIENT_ID", "test-apple-client-id"),
    ("APPLE_TEAM_ID", "test-team-id"),
    ("APPLE_KEY_ID", "test-key-id"),
    (
        "APPLE_PRIVATE_KEY_BASE64",
        "LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0tCk1JR0hBZ0VBTUJNR0J5cUdTTTQ5QWdFR0NDcUdTTTQ5QXdFSEJHMHdhd0lCQVFRZ1E3QXVUOCtqanoxMWVWSFcKVCtzeWUxMGw2azVXdHNWNXBzaDN4YmlCU29laFJBTkNBQVNHS1JyamhoOThrN2xTTjExYlhjYW9QVEY0NVFuWgppWk1UUjdBQXRLMU9mSnJKSXA2aEJ3eVNESkpwb3dTKzYwakY1RmpGVFBRU3VSZXA4Mk1UMHdDMQotLS0tLUVORCBQUklWQVRFIEtFWS0tLS0tCg==",
    ),
    (
        "APPLE_REFRESH_TOKEN_ENCRYPTION_KEY",
        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
    ),
    ("S3_BUCKET", "test-bucket"),
    ("CLOUDFRONT_BASE_URL", "https://cdn.test.chalkak"),
    (
        "IMAGE_PROCESSOR_CALLBACK_SECRET",
        "test-callback-secret-with-enough-length",
    ),
    ("CALLBACK_MAX_BODY_BYTES", "65536"),
    ("SIGNATURE_PROCESSING_TIMEOUT", "7m"),
    ("CHALKAK_IMAGE_ANONYMOUSACCESS", "false"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn env(extra: &[(&str, &str)]) -> Env {
        let mut pairs: Vec<(&str, &str)> = TEST_ENV.to_vec();
        pairs.extend_from_slice(extra);
        Env::from_pairs(pairs)
    }

    fn parse(env: &Env, profile: &str) -> Result<AppSettings, ConfigError> {
        AppSettings::from_env(env, profile, vec![0], Duration::from_secs(900))
    }

    // application.yml / application-local.yml
    #[test]
    fn 필수_값을_읽고_고정_값을_채운다() {
        let s = parse(&env(&[]), "local").unwrap();
        assert_eq!(s.auth.google.issuer, "https://accounts.google.com");
        assert_eq!(
            s.auth.google.jwks_uri,
            "https://www.googleapis.com/oauth2/v3/certs"
        );
        assert_eq!(
            s.auth.apple.token_uri,
            "https://appleid.apple.com/auth/token"
        );
        assert_eq!(s.auth.refresh_inactivity, Duration::from_secs(30 * 86400));
        assert_eq!(s.image.bucket, "test-bucket");
        assert_eq!(s.image.root_prefix, "chalkak");
        assert_eq!(s.image.signature_max_bytes, 1_048_576);
        assert_eq!(s.signature_processing_timeout, Duration::from_secs(420));
        assert_eq!(s.callback.max_body_bytes, 65536);
    }

    #[test]
    fn local만_버킷_기본값과_익명_접근_기본값이_있다() {
        let mut pairs: Vec<(&str, &str)> = TEST_ENV
            .iter()
            .copied()
            .filter(|(k, _)| {
                !matches!(
                    *k,
                    "S3_BUCKET" | "CLOUDFRONT_BASE_URL" | "CHALKAK_IMAGE_ANONYMOUSACCESS"
                )
            })
            .collect();
        let env_local = Env::from_pairs(pairs.clone());
        let s = parse(&env_local, "local").unwrap();
        assert_eq!(s.image.bucket, "chalkak-local-images");
        assert_eq!(s.image.base_url, "http://localhost:4566");
        assert!(s.image.anonymous_access);
        assert!(parse(&env_local, "dev").is_err(), "dev는 S3_PREFIX가 필수");
        pairs.push(("S3_PREFIX", "chalkak"));
        assert!(
            parse(&Env::from_pairs(pairs), "prod").is_err(),
            "prod는 S3_BUCKET이 필수"
        );
    }

    // AppleAuthPropertiesTest, AesGcmAppleAuthorizationCipherTest#constructor_invalidEncryptionKey
    #[test]
    fn apple_properties_invalid_encryption_key_rejects_startup() {
        for bad in [
            "too-short",
            "",
            "00112233",
            "zz112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        ] {
            assert!(
                parse(
                    &env(&[("APPLE_REFRESH_TOKEN_ENCRYPTION_KEY", bad)]),
                    "local"
                )
                .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn 잘못된_값은_기동을_막는다() {
        for (key, value) in [
            ("SOCIAL_SIGNUP_TOKEN_SECRET", "abc"),
            ("SOCIAL_IDENTITY_HMAC_SECRET", "zz"),
            ("APPLE_REFRESH_TOKEN_ENCRYPTION_KEY", "00"),
            ("IMAGE_PROCESSOR_CALLBACK_SECRET", "short"),
            ("CALLBACK_MAX_BODY_BYTES", "0"),
            ("SIGNATURE_PROCESSING_TIMEOUT", "0s"),
            ("REFRESH_TOKEN_INACTIVITY_EXPIRATION", "100d"),
            ("S3_PREFIX", "/chalkak"),
        ] {
            assert!(parse(&env(&[(key, value)]), "local").is_err(), "{key}");
        }
    }
}
