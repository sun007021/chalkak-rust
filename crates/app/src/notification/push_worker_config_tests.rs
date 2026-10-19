//! 푸시 Worker 설정 테스트. 원본 `PushWorkerConfigurationTest` 중 설정 검증 3개에 대응한다.
//! (폴링 작업 등록·종료 동작은 `tests/push_worker_jobs_db.rs`, 숨김 연장 실행기 종료는 `sqs_worker_tests.rs`에서 본다.)

use chalkak_core::config::{ConfigError, Env};

use super::push_worker_config::FcmSettings;
use super::sqs_push::SqsPushSettings;

const MISSING: &str = "PushWorker를 켜려면 Firebase 프로젝트와 서버 인증 파일 경로가 필요합니다.";
const INVALID_QUEUE: &str = "알림 푸시를 사용하려면 Standard SQS 큐 URL이 필요합니다.";

fn reason(error: ConfigError) -> String {
    match error {
        ConfigError::Invalid { reason, .. } => reason,
        other => panic!("예상하지 못한 오류: {other:?}"),
    }
}

// PushWorkerConfigurationTest#configuration_workerDisabled_doesNotInitializeFirebase
#[test]
fn worker가_꺼져_있으면_firebase_설정_없이도_설정을_만들_수_있다() {
    let env = Env::from_pairs([("NOTIFICATION_WORKER_ENABLED", "false")]);
    let sqs = SqsPushSettings::from_env(&env).unwrap();
    assert!(!sqs.worker_enabled);
    assert!(!sqs.queue_required());
    let fcm = FcmSettings::from_env(&env, sqs.worker_enabled).unwrap();
    assert_eq!(fcm.project_id, "");
    assert_eq!(fcm.credentials_path, "");
    // 아무 설정이 없어도 마찬가지다(기본 꺼짐).
    let empty = Env::from_pairs::<&str, &str>([]);
    assert!(!SqsPushSettings::from_env(&empty).unwrap().worker_enabled);
}

// PushWorkerConfigurationTest#configuration_workerEnabledWithoutCredentials_rejectsConfiguration
#[test]
fn worker를_켤_때_프로젝트나_서버_키_경로가_없으면_설정을_거절한다() {
    for (project, path) in [
        ("", "test.json"),
        ("test", ""),
        ("", ""),
        (" ", "test.json"),
        ("test", "\t"),
    ] {
        let env = Env::from_pairs([
            ("NOTIFICATION_FCM_PROJECT_ID", project),
            ("NOTIFICATION_FCM_CREDENTIALS_PATH", path),
        ]);
        let error = FcmSettings::from_env(&env, true).unwrap_err();
        assert_eq!(reason(error), MISSING, "{project:?} {path:?}");
        // 꺼져 있으면 같은 값도 통과한다.
        assert!(FcmSettings::from_env(&env, false).is_ok());
    }
    let env = Env::from_pairs([
        ("NOTIFICATION_FCM_PROJECT_ID", "test"),
        ("NOTIFICATION_FCM_CREDENTIALS_PATH", "test.json"),
    ]);
    let fcm = FcmSettings::from_env(&env, true).unwrap();
    assert_eq!(fcm.project_id, "test");
    assert_eq!(fcm.endpoint_url, "https://fcm.googleapis.com");
}

// PushWorkerConfigurationTest#configuration_invalidQueueUrl_rejectsRelayAndWorker
#[test]
fn relay와_worker는_같은_조건으로_잘못된_standard_큐_url을_거절한다() {
    let urls: [Option<&str>; 7] = [
        None,
        Some(""),
        Some(" "),
        Some("http://sqs.ap-northeast-2.amazonaws.com/000000000000/test"),
        Some("https://example.com/000000000000/test"),
        Some("https://sqs.ap-northeast-2.amazonaws.com/123/test"),
        Some("https://sqs.ap-northeast-2.amazonaws.com/000000000000/test.fifo"),
    ];
    for url in urls {
        for flag in ["NOTIFICATION_RELAY_ENABLED", "NOTIFICATION_WORKER_ENABLED"] {
            let mut pairs = vec![(flag, "true"), ("AWS_REGION", "ap-northeast-2")];
            if let Some(url) = url {
                pairs.push(("NOTIFICATION_SQS_QUEUE_URL", url));
            }
            let error = SqsPushSettings::from_env(&Env::from_pairs(pairs)).unwrap_err();
            assert_eq!(reason(error), INVALID_QUEUE, "{flag} {url:?}");
        }
    }
    // 올바른 Standard 큐 URL은 둘 다 통과한다.
    for flag in ["NOTIFICATION_RELAY_ENABLED", "NOTIFICATION_WORKER_ENABLED"] {
        let env = Env::from_pairs([
            (flag, "true"),
            (
                "NOTIFICATION_SQS_QUEUE_URL",
                "https://sqs.ap-northeast-2.amazonaws.com/000000000000/test",
            ),
        ]);
        assert!(SqsPushSettings::from_env(&env).is_ok(), "{flag}");
    }
}
