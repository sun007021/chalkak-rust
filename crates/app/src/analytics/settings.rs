//! 분석 설정. 원본 `AnalyticsProperties`(`chalkak.analytics.*`)와 `application*.yml`의 값.
//!
//! 환경변수 이름은 Spring 느슨한 바인딩을 따라 하이픈을 없앤다.
//!
//! | 속성 | 환경변수 | 기본값 |
//! | --- | --- | --- |
//! | `chalkak.analytics.aggregation.enabled` | `CHALKAK_ANALYTICS_AGGREGATION_ENABLED` | prod true, 그 밖 false (`jobs` 설정이 읽는다) |
//! | `chalkak.analytics.excluded-user-ids` | `CHALKAK_ANALYTICS_EXCLUDEDUSERIDS`(쉼표로 구분) | prod 팀 계정 21개, 그 밖 비어 있음 |
//! | `chalkak.analytics.log-group` | `CHALKAK_ANALYTICS_LOGGROUP` | prod `/chalkak/prod/application`, 그 밖 `/chalkak/dev/application` |
//! | `chalkak.analytics.log-metrics-start-date` | `CHALKAK_ANALYTICS_LOGMETRICSSTARTDATE` | 2026-10-02 |
//! | `chalkak.analytics.log-retention-days` | `CHALKAK_ANALYTICS_LOGRETENTIONDAYS` | prod 60, 그 밖 7 |
//!
//! 리전은 S3와 같은 `chalkak.image.region`(`AWS_REGION`)을 쓴다(`CloudWatchLogsConfig`).

use chalkak_core::config::{ConfigError, Env};
use jiff::civil::Date;
use uuid::Uuid;

use crate::common::java_uuid_from_string;

/// 프로필이 정하는 기본값(`application.yml`, `application-prod.yml`).
const DEV_LOG_GROUP: &str = "/chalkak/dev/application";
const PROD_LOG_GROUP: &str = "/chalkak/prod/application";
const DEV_RETENTION_DAYS: i64 = 7;
const PROD_RETENTION_DAYS: i64 = 60;

/// prod에서 지표에서 뺄 팀원·테스트 계정의 회원 ID(`application-prod.yml`). 비밀 값이 아니다.
/// 대시보드의 실시간 DAU·당일 접속 사용자 위젯도 같은 목록을 쓴다.
pub const PROD_EXCLUDED_USER_IDS: [&str; 21] = [
    "01a0945f-6d5c-7b62-8096-79fd5dbd187d",
    "01a09463-1b97-723c-86b1-8596005cfa37",
    "01a09464-847a-7d9d-b44f-53eb71710637",
    "01a0946a-05af-744e-b15a-3175707c8721",
    "01a0946a-aa90-7a3f-9956-c26d313eafea",
    "01a0946c-7ff3-71a9-ae92-b4b1a0a79641",
    "01a09492-e61e-7172-bb6f-2ad0b8f1be43",
    "01a09822-2461-7649-be66-18d1203e2871",
    "01a09de9-e8af-7034-95f8-20ec7a795745",
    "01a09dee-5ce9-726e-b49e-c4816553f681",
    "01a09e37-d7e8-77c6-8023-0fe770644536",
    "01a09e9f-b243-7a78-b790-51feeab58a15",
    "01a0a366-73e4-76a7-9acc-6d73bbf85913",
    "01a0a36c-f580-78f9-a1fd-dd29c9f47da9",
    "01a0a36e-5721-79f7-95c9-7a3126d464e9",
    "01a0a519-6cbf-7ae1-bcc4-a3710eab5ea8",
    "01a0adcf-06f2-7090-a9ca-6c21c2251e6b",
    "01a0b299-a04b-72cf-bb83-8a7da6b96b97",
    "01a0c381-9856-70a8-a598-e228cb99b028",
    "01a0c386-f89a-7db3-9701-1aea22bc9caa",
    "01a11503-09cf-7366-8d03-81e0c6e6f47b",
];

/// 원본 `AnalyticsProperties`: 지표에서 뺄 회원, 접근 로그 그룹, 로그 지표 관측 시작일, 로그 보존 기간.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalyticsSettings {
    pub excluded_user_ids: Vec<Uuid>,
    pub log_group: String,
    /// 이 날짜보다 이른 날짜는 로그 지표를 집계하지 않는다.
    pub log_metrics_start_date: Date,
    /// 로그 그룹의 보존 기간(일). 하루 전체가 보존 기간 안에 들지 않는 날짜는 원본 로그가 없어 로그 지표를 집계하지 않는다.
    pub log_retention_days: i64,
    /// CloudWatch Logs 리전(`chalkak.image.region`).
    pub region: String,
}

impl AnalyticsSettings {
    /// `deployed_prod`는 프로필이 prod인지다. `region`은 이미지 설정의 리전이다.
    pub fn from_env(
        env: &Env,
        deployed_prod: bool,
        region: &str,
    ) -> Result<AnalyticsSettings, ConfigError> {
        let excluded_user_ids = match env.get("CHALKAK_ANALYTICS_EXCLUDEDUSERIDS") {
            Some(raw) => parse_user_ids("CHALKAK_ANALYTICS_EXCLUDEDUSERIDS", raw)?,
            None if deployed_prod => PROD_EXCLUDED_USER_IDS
                .iter()
                .map(|id| java_uuid_from_string(id).expect("원본 prod 제외 목록은 UUID다"))
                .collect(),
            None => Vec::new(),
        };
        let default_group = if deployed_prod {
            PROD_LOG_GROUP
        } else {
            DEV_LOG_GROUP
        };
        let default_retention = if deployed_prod {
            PROD_RETENTION_DAYS
        } else {
            DEV_RETENTION_DAYS
        };
        let log_retention_days: i64 =
            env.parsed_or("CHALKAK_ANALYTICS_LOGRETENTIONDAYS", default_retention)?;
        if log_retention_days <= 0 {
            return Err(ConfigError::Invalid {
                key: "CHALKAK_ANALYTICS_LOGRETENTIONDAYS".to_owned(),
                reason: "0보다 커야 합니다".to_owned(),
            });
        }
        let log_metrics_start_date: Date = env.parsed_or(
            "CHALKAK_ANALYTICS_LOGMETRICSSTARTDATE",
            default_start_date(),
        )?;
        Ok(AnalyticsSettings {
            excluded_user_ids,
            log_group: env
                .string_or("CHALKAK_ANALYTICS_LOGGROUP", default_group)
                .to_owned(),
            log_metrics_start_date,
            log_retention_days,
            region: region.to_owned(),
        })
    }
}

/// 원본 `application.yml`의 `log-metrics-start-date`(prod 접근 로그에 userId가 남기 시작한 다음 날).
fn default_start_date() -> Date {
    Date::constant(2026, 10, 2)
}

fn parse_user_ids(key: &str, raw: &str) -> Result<Vec<Uuid>, ConfigError> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    raw.split(',')
        .map(|part| {
            java_uuid_from_string(part.trim()).ok_or_else(|| ConfigError::Invalid {
                key: key.to_owned(),
                reason: "쉼표로 구분한 회원 ID(UUID) 목록이어야 합니다".to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)], prod: bool) -> Result<AnalyticsSettings, ConfigError> {
        AnalyticsSettings::from_env(
            &Env::from_pairs(pairs.iter().copied()),
            prod,
            "ap-northeast-2",
        )
    }

    // application.yml / application-prod.yml 기본값
    #[test]
    fn 프로필별_기본값은_원본_yml과_같다() {
        let dev = settings(&[], false).unwrap();
        assert!(dev.excluded_user_ids.is_empty());
        assert_eq!(dev.log_group, "/chalkak/dev/application");
        assert_eq!(dev.log_metrics_start_date, Date::constant(2026, 10, 2));
        assert_eq!(dev.log_retention_days, 7);

        let prod = settings(&[], true).unwrap();
        assert_eq!(prod.excluded_user_ids.len(), 21);
        assert_eq!(
            prod.excluded_user_ids[0].to_string(),
            "01a0945f-6d5c-7b62-8096-79fd5dbd187d"
        );
        assert_eq!(
            prod.excluded_user_ids[20].to_string(),
            "01a11503-09cf-7366-8d03-81e0c6e6f47b"
        );
        assert_eq!(prod.log_group, "/chalkak/prod/application");
        assert_eq!(prod.log_retention_days, 60);
    }

    #[test]
    fn 환경변수가_프로필_기본값을_덮는다() {
        let s = settings(
            &[
                (
                    "CHALKAK_ANALYTICS_EXCLUDEDUSERIDS",
                    "00000000-0000-0000-0000-000000000001, 00000000-0000-0000-0000-000000000002",
                ),
                ("CHALKAK_ANALYTICS_LOGGROUP", "/chalkak/test/application"),
                ("CHALKAK_ANALYTICS_LOGMETRICSSTARTDATE", "2099-01-01"),
                ("CHALKAK_ANALYTICS_LOGRETENTIONDAYS", "30"),
            ],
            true,
        )
        .unwrap();
        assert_eq!(s.excluded_user_ids.len(), 2);
        assert_eq!(s.log_group, "/chalkak/test/application");
        assert_eq!(s.log_metrics_start_date, Date::constant(2099, 1, 1));
        assert_eq!(s.log_retention_days, 30);
    }

    #[test]
    fn 빈_제외_목록은_prod_기본_목록도_비운다() {
        let s = settings(&[("CHALKAK_ANALYTICS_EXCLUDEDUSERIDS", "")], true).unwrap();
        assert!(s.excluded_user_ids.is_empty());
    }

    // AnalyticsProperties: logRetentionDays는 양수여야 한다.
    #[test]
    fn 보존_기간이_양수가_아니면_기동을_막는다() {
        for value in ["0", "-1"] {
            assert!(matches!(
                settings(&[("CHALKAK_ANALYTICS_LOGRETENTIONDAYS", value)], false),
                Err(ConfigError::Invalid { .. })
            ));
        }
    }

    #[test]
    fn 잘못된_회원_id_나_날짜는_기동을_막는다() {
        assert!(
            settings(
                &[("CHALKAK_ANALYTICS_EXCLUDEDUSERIDS", "not-a-uuid")],
                false
            )
            .is_err()
        );
        assert!(
            settings(
                &[("CHALKAK_ANALYTICS_LOGMETRICSSTARTDATE", "2026/10/02")],
                false
            )
            .is_err()
        );
    }
}
