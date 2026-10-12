//! SigV4 서명, S3 presigned PUT·HeadObject, 자격증명 공급자(환경변수 → IMDSv2). AWS SDK 없이 직접 구현한다.
#![forbid(unsafe_code)]

pub mod sigv4;
