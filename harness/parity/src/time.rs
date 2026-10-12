//! 외부 crate 없이 RFC 3339 시각을 다루는 최소 도구. 하네스는 시각을 "시나리오 시계 기준 상대 초"로만
//! 비교하므로 epoch 초와의 변환만 있으면 된다.

/// 1970-01-01 기준 일 수 → (년, 월, 일). Howard Hinnant의 civil_from_days.
#[cfg(test)]
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 파싱한 시각과 원문 형식 정보.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// epoch 초(소수 포함).
    pub epoch: f64,
    /// 소수점 아래 자릿수(0이면 소수부 없음).
    pub fraction_digits: usize,
    /// "Z", "+09:00" 같은 원문 오프셋 표기. 없으면 빈 문자열(로컬 시각).
    pub zone: String,
}

/// `YYYY-MM-DDTHH:MM:SS[.f+][Z|±HH:MM]` 형식만 받는다. 날짜만 있는 값은 시각으로 보지 않는다.
/// 오프셋이 없는 로컬 시각은 Asia/Seoul(+09:00)로 해석한다(원본 서비스의 기준 시간대).
pub fn parse(s: &str) -> Option<Parsed> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
        num(17..19)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut i = 19;
    let mut frac = 0.0;
    let mut digits = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        digits = i - start;
        if digits == 0 {
            return None;
        }
        frac = format!("0.{}", &s[start..i]).parse().ok()?;
    }
    let rest = &s[i..];
    let offset_secs = if rest.is_empty() {
        9 * 3600
    } else if rest == "Z" {
        0
    } else if rest.len() == 6
        && (rest.starts_with('+') || rest.starts_with('-'))
        && &rest[3..4] == ":"
    {
        let sign = if rest.starts_with('-') { -1 } else { 1 };
        sign * (rest[1..3].parse::<i64>().ok()? * 3600 + rest[4..6].parse::<i64>().ok()? * 60)
    } else {
        return None;
    };
    let days = days_from_civil(y, mo as u32, d as u32);
    let secs = days * 86_400 + h * 3600 + mi * 60 + se - offset_secs;
    Some(Parsed {
        epoch: secs as f64 + frac,
        fraction_digits: digits,
        zone: rest.to_string(),
    })
}

pub fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 오프셋별로_같은_순간을_같은_epoch로_읽는다() {
        let a = parse("2026-10-12T00:00:00Z").unwrap();
        let b = parse("2026-10-12T09:00:00+09:00").unwrap();
        let c = parse("2026-10-12T09:00:00").unwrap();
        assert_eq!(a.epoch, b.epoch);
        assert_eq!(a.epoch, c.epoch);
        assert_eq!(a.epoch, 1_791_763_200.0);
    }

    #[test]
    fn 소수부_자릿수를_기록한다() {
        let p = parse("2026-10-12T00:00:00.123400Z").unwrap();
        assert_eq!(p.fraction_digits, 6);
        assert_eq!(p.zone, "Z");
    }

    #[test]
    fn 날짜만_있는_값은_시각이_아니다() {
        assert!(parse("2026-10-12").is_none());
        assert!(parse("hello").is_none());
    }

    #[test]
    fn 일수_변환이_왕복한다() {
        for days in [-1000, 0, 19_000, 20_738] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
