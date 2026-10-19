//! 관리자 주제 푸시 시간표 관리(원본 `AdminTopicPushScheduleController`, `AdminTopicPushScheduleService`, DTO):
//! `/api/v1/admin/topic-push-schedules`.
//!
//! 발송 시각(KST, 분 단위)을 하나씩 등록한다. 같은 시각은 하나만 둔다. 삭제는 행을 지우고 감사 로그에 남긴다. 푸시를 실제로
//! 발행하는 일은 이 모듈의 범위가 아니다.

use chalkak_http::{Method, Request};
use jiff::Timestamp;
use jiff::civil::Time;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::admin::audit::domain::{AdminAction, AdminTargetType, AuditSnapshot, text_value};
use crate::admin::audit::service::{AuditLogCommand, create_audit_log};
use crate::admin::binding::{MUST_NOT_BE_NULL, java_trim, validation};
use crate::admin::support::{
    AdminDeps, in_transaction, no_content, not_found, path_uuid, resolve_current_admin,
};
use crate::notification::topic_push_schedule::{
    SaveError, TopicPushSchedule, TopicPushScheduleRepository, duplicate_send_time_error,
    validate_send_time,
};
use crate::web::binding::read_json_object;
use crate::web::json::serialize_instant;
use crate::web::{ApiError, Routes, created_json, ok_json};

const NOT_FOUND_MESSAGE: &str = "주제 푸시 설정을 찾을 수 없습니다.";

fn save_error(e: SaveError) -> ApiError {
    match e {
        SaveError::Business(e) => ApiError::App(e),
        SaveError::Db(e) => ApiError::from(e),
    }
}

/// `HH:mm:ss`(`DateTimeFormatter.ofPattern("HH:mm:ss")`, 응답의 `LocalTime`도 같은 모양이다).
fn format_send_time(time: Time) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        time.hour(),
        time.minute(),
        time.second()
    )
}

#[derive(Clone)]
pub struct AdminTopicPushScheduleService {
    deps: AdminDeps,
}

impl AdminTopicPushScheduleService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminTopicPushScheduleService { deps }
    }

    /// `getSchedules`: 발송 시각 오름차순.
    pub fn get_schedules(&self) -> Result<Vec<TopicPushSchedule>, ApiError> {
        let mut conn = self.deps.pool.get()?;
        Ok(TopicPushScheduleRepository.find_all_order_by_send_time(&mut conn)?)
    }

    pub fn create_schedule(
        &self,
        admin_id: Uuid,
        send_time: Time,
    ) -> Result<TopicPushSchedule, ApiError> {
        // 시각이 잘못됐으면 DB를 쓰지 않는다.
        validate_send_time(send_time)?;
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let repo = TopicPushScheduleRepository;
            if repo.exists_by_send_time(tx, send_time)? {
                return Err(ApiError::App(duplicate_send_time_error()));
            }
            let mut schedule = TopicPushSchedule::new(send_time)?;
            repo.save_and_flush(tx, &mut schedule, now)
                .map_err(save_error)?;
            let schedule_id = schedule.id.ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::TopicPushScheduleCreated,
                    target_type: AdminTargetType::TopicPushSchedule,
                    target_id: schedule_id,
                    reason: None,
                    before_state: AuditSnapshot::empty(),
                    after_state: schedule_state(&schedule)?,
                },
                now,
            )?;
            Ok(schedule)
        })
    }

    pub fn update_schedule(
        &self,
        schedule_id: Uuid,
        admin_id: Uuid,
        send_time: Time,
    ) -> Result<TopicPushSchedule, ApiError> {
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let repo = TopicPushScheduleRepository;
            let mut schedule = repo
                .find_by_id_for_update(tx, schedule_id)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            validate_send_time(send_time)?;
            if schedule.send_time == send_time {
                return Ok(schedule);
            }
            if repo.exists_by_send_time(tx, send_time)? {
                return Err(ApiError::App(duplicate_send_time_error()));
            }
            let before = schedule_state(&schedule)?;
            schedule.update_send_time(send_time)?;
            repo.save_and_flush(tx, &mut schedule, now)
                .map_err(save_error)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::TopicPushScheduleUpdated,
                    target_type: AdminTargetType::TopicPushSchedule,
                    target_id: schedule_id,
                    reason: None,
                    before_state: before,
                    after_state: schedule_state(&schedule)?,
                },
                now,
            )?;
            Ok(schedule)
        })
    }

    pub fn delete_schedule(&self, schedule_id: Uuid, admin_id: Uuid) -> Result<(), ApiError> {
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let repo = TopicPushScheduleRepository;
            let schedule = repo
                .find_by_id_for_update(tx, schedule_id)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            let before = schedule_state(&schedule)?;
            repo.delete(tx, schedule_id)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::TopicPushScheduleDeleted,
                    target_type: AdminTargetType::TopicPushSchedule,
                    target_id: schedule_id,
                    reason: None,
                    before_state: before,
                    after_state: AuditSnapshot::empty(),
                },
                now,
            )?;
            Ok(())
        })
    }
}

fn schedule_state(schedule: &TopicPushSchedule) -> Result<AuditSnapshot, ApiError> {
    Ok(AuditSnapshot::of(vec![(
        "sendTime",
        text_value(&format_send_time(schedule.send_time)),
    )])?)
}

// ---------------------------------------------------------------------------------------------
// 컨트롤러
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let service = AdminTopicPushScheduleService::new(deps.clone());
    let (list_deps, create_deps, update_deps, delete_deps) =
        (deps.clone(), deps.clone(), deps.clone(), deps.clone());
    let (list_service, create_service, update_service) =
        (service.clone(), service.clone(), service.clone());
    routes
        .get("/api/v1/admin/topic-push-schedules", move |_req| {
            resolve_current_admin(&list_deps)?;
            let schedules = list_service.get_schedules()?;
            Ok(ok_json(&ScheduleListResponse {
                schedules: schedules.into_iter().map(ScheduleResponse::from).collect(),
            }))
        })
        .post("/api/v1/admin/topic-push-schedules", move |req| {
            let admin_id = resolve_current_admin(&create_deps)?;
            let send_time = read_send_time(req)?;
            let schedule = create_service.create_schedule(admin_id, send_time)?;
            Ok(created_json(&ScheduleResponse::from(schedule)))
        })
        .route(
            Method::Put,
            "/api/v1/admin/topic-push-schedules/{scheduleId}",
            move |req| {
                let admin_id = resolve_current_admin(&update_deps)?;
                let send_time = read_send_time(req)?;
                let schedule_id = path_uuid(req, "scheduleId")?;
                let schedule = update_service.update_schedule(schedule_id, admin_id, send_time)?;
                Ok(ok_json(&ScheduleResponse::from(schedule)))
            },
        )
        .route(
            Method::Delete,
            "/api/v1/admin/topic-push-schedules/{scheduleId}",
            move |req| {
                let admin_id = resolve_current_admin(&delete_deps)?;
                let schedule_id = path_uuid(req, "scheduleId")?;
                service.delete_schedule(schedule_id, admin_id)?;
                Ok(no_content())
            },
        )
}

/// `TopicPushScheduleRequest`: `@NotNull LocalTime sendTime`.
fn read_send_time(req: &Request) -> Result<Time, ApiError> {
    let json = read_json_object(req)?;
    match json_local_time(json.value("sendTime"))? {
        Some(time) => Ok(time),
        None => Err(validation(MUST_NOT_BE_NULL)),
    }
}

/// Jackson `LocalTime`: 문자열(`ISO_LOCAL_TIME`, `T`가 붙은 날짜시각 포함, 앞뒤 공백 허용, 빈 문자열은 `null`)과
/// `[시, 분(, 초(, 나노초))]` 배열을 받는다. 그 밖의 값과 범위를 벗어난 시각은 읽을 수 없는 본문(400)이다.
fn json_local_time(value: Option<&Value>) -> Result<Option<Time>, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let trimmed = java_trim(text);
            if trimmed.is_empty() {
                return Ok(None);
            }
            parse_local_time_text(trimmed)
                .map(Some)
                .ok_or(ApiError::UnreadableBody)
        }
        Some(Value::Array(items)) => {
            let numbers: Option<Vec<i64>> = items.iter().map(Value::as_i64).collect();
            let unreadable = || ApiError::UnreadableBody;
            match numbers.as_deref() {
                Some([h, m]) => time_of(*h, *m, 0, 0),
                Some([h, m, s]) => time_of(*h, *m, *s, 0),
                Some([h, m, s, n]) => time_of(*h, *m, *s, *n),
                _ => None,
            }
            .map(Some)
            .ok_or_else(unreadable)
        }
        Some(_) => Err(ApiError::UnreadableBody),
    }
}

fn time_of(hour: i64, minute: i64, second: i64, nanos: i64) -> Option<Time> {
    Time::new(
        i8::try_from(hour).ok()?,
        i8::try_from(minute).ok()?,
        i8::try_from(second).ok()?,
        i32::try_from(nanos).ok()?,
    )
    .ok()
}

fn parse_local_time_text(text: &str) -> Option<Time> {
    // 자바스크립트가 보내는 날짜시각(`...T...`): `Z`로 끝나면 UTC 시각, 아니면 `ISO_LOCAL_DATE_TIME`의 시간 부분.
    if text.len() > 10 && text.as_bytes()[10] == b'T' {
        return if let Some(instant) = text.strip_suffix('Z') {
            let timestamp: Timestamp = format!("{instant}Z").parse().ok()?;
            Some(timestamp.to_zoned(jiff::tz::TimeZone::UTC).time())
        } else {
            parse_iso_local_time(&text[11..])
                .filter(|_| text[..10].parse::<jiff::civil::Date>().is_ok())
        };
    }
    parse_iso_local_time(text)
}

/// `ISO_LOCAL_TIME`: `HH:mm[:ss[.fraction]]`(분 이하는 선택, 소수는 0~9자리). 엄격한 해석이라 24시와 60초는 거절한다.
fn parse_iso_local_time(text: &str) -> Option<Time> {
    let b = text.as_bytes();
    let two = |at: usize| -> Option<i64> {
        let (x, y) = (*b.get(at)?, *b.get(at + 1)?);
        (x.is_ascii_digit() && y.is_ascii_digit())
            .then(|| i64::from(x - b'0') * 10 + i64::from(y - b'0'))
    };
    let hour = two(0)?;
    if b.get(2) != Some(&b':') {
        return None;
    }
    let minute = two(3)?;
    let (mut second, mut nanos) = (0, 0);
    if b.len() > 5 {
        if b[5] != b':' {
            return None;
        }
        second = two(6)?;
        if b.len() > 8 {
            if b[8] != b'.' {
                return None;
            }
            let fraction = &b[9..];
            if fraction.len() > 9 || !fraction.iter().all(u8::is_ascii_digit) {
                return None;
            }
            for (i, digit) in fraction.iter().enumerate() {
                nanos += i64::from(digit - b'0') * 10_i64.pow(u32::try_from(8 - i).ok()?);
            }
        }
    }
    time_of(hour, minute, second, nanos)
}

#[derive(Serialize)]
struct ScheduleListResponse {
    schedules: Vec<ScheduleResponse>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleResponse {
    schedule_id: String,
    send_time: String,
    #[serde(serialize_with = "serialize_instant")]
    created_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    updated_at: Timestamp,
}

impl From<TopicPushSchedule> for ScheduleResponse {
    fn from(schedule: TopicPushSchedule) -> Self {
        ScheduleResponse {
            schedule_id: schedule.id.unwrap_or_default().hyphenated().to_string(),
            send_time: format_send_time(schedule.send_time),
            created_at: schedule.created_at.unwrap_or(Timestamp::UNIX_EPOCH),
            updated_at: schedule.updated_at.unwrap_or(Timestamp::UNIX_EPOCH),
        }
    }
}
