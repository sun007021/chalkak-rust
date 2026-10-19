//! SigV4 서명, S3 presigned PUT·HeadObject, 자격증명 공급자(환경변수 → IMDSv2). AWS SDK 없이 직접 구현한다.
#![forbid(unsafe_code)]

pub mod credentials;
pub mod error;
pub mod logs;
pub mod s3;
pub mod sigv4;
pub mod sqs;

pub use credentials::{
    ChainCredentialsProvider, Credentials, CredentialsProvider, EnvCredentialsProvider,
    HttpRequest, HttpResponse, HttpTransport, ImdsCredentialsProvider,
};
pub use error::AwsError;
pub use logs::{LogsEndpoint, sign_json_request};
pub use s3::{PutObject, S3Endpoint, SignedRequest, presign_put, sign_head_object};
pub use sqs::{
    ReceiveMessageInput, ReceivedMessage, SendMessageOutput, SqsClient, SqsEndpoint, SqsError,
    SqsHttpResponse, SqsServiceError, SqsTransport, is_standard_queue_url,
};
