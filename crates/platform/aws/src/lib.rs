//! SigV4 서명, S3 presigned PUT·HeadObject, 자격증명 공급자(환경변수 → IMDSv2). AWS SDK 없이 직접 구현한다.
#![forbid(unsafe_code)]

pub mod credentials;
pub mod error;
pub mod s3;
pub mod sigv4;

pub use credentials::{Credentials, CredentialsProvider};
pub use error::AwsError;
pub use s3::{PutObject, S3Endpoint, SignedRequest, presign_put, sign_head_object};
