//! 게시물·이미지 설정. 원본 `application*.yml`의 `chalkak.image.*`, `chalkak.callback.*`, `chalkak.post.*`와
//! `CLOUDFRONT_*`, `IMAGE_PROCESSOR_CALLBACK_SECRET` 환경변수를 같은 이름으로 읽는다.
//!
//! local 프로필만 기본값이 있다(`application-local.yml`): 버킷 `chalkak-local-images`, 접두사 `chalkak`, CDN 주소
//! `http://localhost:4566`, 익명 자격증명. dev·prod는 값이 없으면 기동하지 못한다.

use std::time::Duration;

use chalkak_core::config::{ConfigError, Env};

/// `ImageProperties`에서 게시물 흐름이 쓰는 부분.
#[derive(Clone, Debug)]
pub struct ImageSettings {
    pub bucket: String,
    pub region: String,
    /// `S3_PREFIX`. 앞뒤 슬래시와 빈 구간이 없어야 한다.
    pub root_prefix: String,
    /// 프로필 이름(`local`, `dev`, `prod`). 스토리지 키와 콜백 서명 대상에 들어간다.
    pub environment: String,
    pub post_max_bytes: u64,
    pub post_cache_control: String,
    /// 익명 자격증명으로 S3를 부르는지. 익명으로는 presigned URL을 만들 수 없다.
    pub anonymous_access: bool,
}

#[derive(Clone, Debug)]
pub struct PostSettings {
    pub image: ImageSettings,
    pub cloudfront_base_url: String,
    pub cloudfront_origin_path: String,
    /// `IMAGE_PROCESSOR_CALLBACK_SECRET`(32자 이상). 사인 콜백과 같은 비밀키를 쓴다.
    pub callback_secret: String,
    /// `CALLBACK_MAX_BODY_BYTES`
    pub callback_max_body_bytes: u64,
    /// `POST_IMAGE_PROCESSING_TIMEOUT`
    pub image_processing_timeout: Duration,
}

fn invalid(key: &str, reason: &str) -> ConfigError {
    ConfigError::Invalid {
        key: key.to_owned(),
        reason: reason.to_owned(),
    }
}

/// `root-prefix`의 `^[^/]+(/[^/]+)*$`.
fn valid_root_prefix(prefix: &str) -> bool {
    !prefix.is_empty() && prefix.split('/').all(|part| !part.is_empty())
}

impl PostSettings {
    /// `environment`는 프로필 이름이고, `local`이면 로컬 기본값을 쓴다.
    pub fn from_env(env: &Env, environment: &str) -> Result<PostSettings, ConfigError> {
        let local = environment == "local";
        let with_default = |key: &str, local_default: &str| -> Result<String, ConfigError> {
            if local {
                Ok(env.string_or(key, local_default).to_owned())
            } else {
                Ok(env.required(key)?.to_owned())
            }
        };
        let bucket = with_default("S3_BUCKET", "chalkak-local-images")?;
        let root_prefix = with_default("S3_PREFIX", "chalkak")?;
        let cloudfront_base_url = with_default("CLOUDFRONT_BASE_URL", "http://localhost:4566")?;
        if !valid_root_prefix(&root_prefix) {
            return Err(invalid(
                "S3_PREFIX",
                "root-prefix는 앞뒤 슬래시와 빈 구간 없이 작성해야 합니다.",
            ));
        }
        let cloudfront_origin_path = env.required("CLOUDFRONT_ORIGIN_PATH")?.to_owned();

        let callback_secret = env.required("IMAGE_PROCESSOR_CALLBACK_SECRET")?.to_owned();
        if callback_secret.chars().count() < 32 {
            return Err(invalid(
                "IMAGE_PROCESSOR_CALLBACK_SECRET",
                "IMAGE_PROCESSOR_CALLBACK_SECRET은 32자 이상이어야 합니다.",
            ));
        }
        let callback_max_body_bytes: u64 = env.required_parsed("CALLBACK_MAX_BODY_BYTES")?;
        if callback_max_body_bytes == 0 {
            return Err(invalid(
                "CALLBACK_MAX_BODY_BYTES",
                "chalkak.callback.max-body-bytes는 0보다 커야 합니다.",
            ));
        }
        let image_processing_timeout = env.required_duration("POST_IMAGE_PROCESSING_TIMEOUT")?;
        if image_processing_timeout.is_zero() {
            return Err(invalid(
                "POST_IMAGE_PROCESSING_TIMEOUT",
                "chalkak.post.image-processing-timeout은 0보다 커야 합니다.",
            ));
        }

        Ok(PostSettings {
            image: ImageSettings {
                bucket,
                region: env.string_or("AWS_REGION", "ap-northeast-2").to_owned(),
                root_prefix,
                environment: environment.to_owned(),
                post_max_bytes: 5_242_880,
                post_cache_control: "public, max-age=86400".to_owned(),
                anonymous_access: env.bool_or("CHALKAK_IMAGE_ANONYMOUSACCESS", local)?,
            },
            cloudfront_base_url,
            cloudfront_origin_path,
            callback_secret,
            callback_max_body_bytes,
            image_processing_timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-callback-secret-with-enough-length";

    fn env(extra: &[(&str, &str)]) -> Env {
        let mut pairs = vec![
            ("CLOUDFRONT_ORIGIN_PATH", "/chalkak"),
            ("IMAGE_PROCESSOR_CALLBACK_SECRET", SECRET),
            ("CALLBACK_MAX_BODY_BYTES", "65536"),
            ("POST_IMAGE_PROCESSING_TIMEOUT", "7m"),
        ];
        pairs.extend_from_slice(extra);
        Env::from_pairs(pairs)
    }

    // application-local.yml
    #[test]
    fn local_기본값은_원본_local_프로필과_같다() {
        let s = PostSettings::from_env(&env(&[]), "local").unwrap();
        assert_eq!(s.image.bucket, "chalkak-local-images");
        assert_eq!(s.image.root_prefix, "chalkak");
        assert_eq!(s.image.region, "ap-northeast-2");
        assert_eq!(s.cloudfront_base_url, "http://localhost:4566");
        assert!(s.image.anonymous_access);
        assert_eq!(s.image.post_max_bytes, 5_242_880);
        assert_eq!(s.image.post_cache_control, "public, max-age=86400");
        assert_eq!(s.image_processing_timeout, Duration::from_secs(420));
    }

    #[test]
    fn dev_prod는_버킷_접두사_cdn_주소가_필수다() {
        assert!(PostSettings::from_env(&env(&[]), "dev").is_err());
        let full = env(&[
            ("S3_BUCKET", "b"),
            ("S3_PREFIX", "p/q"),
            ("CLOUDFRONT_BASE_URL", "https://cdn"),
        ]);
        let s = PostSettings::from_env(&full, "prod").unwrap();
        assert_eq!(s.image.environment, "prod");
        assert!(!s.image.anonymous_access);
    }

    #[test]
    fn 익명_접근은_환경변수로_끈다() {
        let e = env(&[("CHALKAK_IMAGE_ANONYMOUSACCESS", "false")]);
        assert!(
            !PostSettings::from_env(&e, "local")
                .unwrap()
                .image
                .anonymous_access
        );
    }

    // ImageProperties: root-prefix 패턴, ProcessingCallbackAuthenticator: 비밀키 32자 이상,
    // CallbackBodySizeLimitFilter: 0보다 큰 상한, PostProcessingPolicy: 0보다 큰 기간
    #[test]
    fn 잘못된_설정은_기동을_막는다() {
        for bad in [
            env(&[("S3_PREFIX", "/chalkak")]),
            env(&[("S3_PREFIX", "chalkak/")]),
            env(&[("S3_PREFIX", "a//b")]),
            env(&[("IMAGE_PROCESSOR_CALLBACK_SECRET", "short")]),
            env(&[("CALLBACK_MAX_BODY_BYTES", "0")]),
            env(&[("POST_IMAGE_PROCESSING_TIMEOUT", "0s")]),
        ] {
            assert!(PostSettings::from_env(&bad, "local").is_err());
        }
        let missing = Env::from_pairs([("CLOUDFRONT_ORIGIN_PATH", "/chalkak")]);
        assert!(PostSettings::from_env(&missing, "local").is_err());
    }
}
