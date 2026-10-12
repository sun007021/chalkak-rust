//! HS256 발급·검증, RS256 검증, ES256 서명, JWKS 캐시.
//!
//! JWT 라이브러리 없이 RustCrypto(`hmac`, `sha2`, `p256`, `rsa`)로 직접 구현한다. 원본 동작의 근거는
//! Spring Security `NimbusJwtDecoder`/`NimbusJwtEncoder`와 Nimbus JOSE 라이브러리다. 각 모듈 머리말과 테스트 이름에
//! 근거 Java 클래스를 적었다.

mod b64;
mod claims;
mod error;
mod hs256;
mod json;
mod jws;
pub mod original;
mod validator;

pub use b64::decode_hex;
pub use claims::Claims;
pub use error::{JwksError, JwtError};
pub use hs256::{hs256_decode, hs256_encode};
pub use validator::{ClaimError, Validator};
