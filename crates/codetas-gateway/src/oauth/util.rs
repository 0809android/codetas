use base64::{engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD}, Engine as _};
use serde_json::Value;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
pub(crate) fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

pub(crate) fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
    })
}

/// Extract the stable Kimi identity using the same claim precedence as
/// OpenCodex: `user_id` across both tokens, then `sub`; email is normalized.
pub(crate) fn kimi_identity_from_tokens(
    access_token: &str,
    refresh_token: Option<&str>,
) -> (Option<String>, Option<String>) {
    fn jwt_payload(token: &str) -> Option<Value> {
        let mut parts = token.split('.');
        let _header = parts.next()?;
        let payload = parts.next()?;
        let _signature = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .or_else(|_| URL_SAFE.decode(payload))
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }
    fn claim(payload: Option<&Value>, name: &str) -> Option<String> {
        payload
            .and_then(|value| value.get(name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    }

    let access = jwt_payload(access_token);
    let refresh = refresh_token.and_then(jwt_payload);
    let account_id = claim(access.as_ref(), "user_id")
        .or_else(|| claim(refresh.as_ref(), "user_id"))
        .or_else(|| claim(access.as_ref(), "sub"))
        .or_else(|| claim(refresh.as_ref(), "sub"));
    let email = claim(access.as_ref(), "email")
        .or_else(|| claim(refresh.as_ref(), "email"))
        .map(|value| value.to_lowercase());
    (account_id, email)
}

pub(crate) fn json_expiry_ms(value: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        if let Some(number) = value.get(*key).and_then(Value::as_i64) {
            return normalize_expiry_ms(number);
        }
        if let Some(number) = value.get(*key).and_then(Value::as_f64) {
            return normalize_expiry_ms(number as i64);
        }
        if let Some(text) = value.get(*key).and_then(Value::as_str) {
            if let Ok(number) = text.parse::<i64>() {
                return normalize_expiry_ms(number);
            }
            if let Ok(parsed) = chrono_like_to_ms(text) {
                return parsed;
            }
        }
    }
    0
}

pub(crate) fn normalize_expiry_ms(value: i64) -> i64 {
    if value <= 0 {
        0
    } else if value < 100_000_000_000 {
        value.saturating_mul(1_000)
    } else {
        value
    }
}

pub(crate) fn chrono_like_to_ms(value: &str) -> Result<i64, ()> {
    let trimmed = value.trim();
    let (date, rest) = trimmed
        .split_once('T')
        .or_else(|| trimmed.split_once(' '))
        .ok_or(())?;
    let mut date_parts = date.split('-');
    let year = date_parts
        .next()
        .ok_or(())?
        .parse::<i32>()
        .map_err(|_| ())?;
    let month = date_parts
        .next()
        .ok_or(())?
        .parse::<u32>()
        .map_err(|_| ())?;
    let day = date_parts
        .next()
        .ok_or(())?
        .parse::<u32>()
        .map_err(|_| ())?;
    let rest = rest.trim();
    let (time, offset_seconds) = if let Some(time) = rest.strip_suffix('Z') {
        (time, 0_i64)
    } else if let Some(index) = rest
        .char_indices()
        .skip(1)
        .find_map(|(index, character)| matches!(character, '+' | '-').then_some(index))
    {
        let sign = if rest.as_bytes()[index] == b'-' {
            -1_i64
        } else {
            1_i64
        };
        let offset = &rest[index + 1..];
        let (hours, minutes) = offset.split_once(':').ok_or(())?;
        let hours = hours.parse::<i64>().map_err(|_| ())?;
        let minutes = minutes.parse::<i64>().map_err(|_| ())?;
        if hours > 23 || minutes > 59 {
            return Err(());
        }
        (&rest[..index], sign * (hours * 3_600 + minutes * 60))
    } else {
        (rest, 0_i64)
    };
    let mut time_parts = time.split(':');
    let hour = time_parts
        .next()
        .ok_or(())?
        .parse::<u32>()
        .map_err(|_| ())?;
    let minute = time_parts
        .next()
        .ok_or(())?
        .parse::<u32>()
        .map_err(|_| ())?;
    let second = time_parts
        .next()
        .unwrap_or("0")
        .split('.')
        .next()
        .unwrap_or("0")
        .parse::<u32>()
        .map_err(|_| ())?;
    Ok(civil_utc_to_ms(year, month, day, hour, minute, second)?
        .saturating_sub(offset_seconds.saturating_mul(1_000)))
}

pub(crate) fn civil_utc_to_ms(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Result<i64, ()> {
    if !(1..=12).contains(&month) || day == 0 || hour > 23 || minute > 59 || second > 60 {
        return Err(());
    }
    let days = days_from_civil(year, month, day)?;
    Ok(days
        .saturating_mul(86_400)
        .saturating_add(i64::from(hour) * 3_600)
        .saturating_add(i64::from(minute) * 60)
        .saturating_add(i64::from(second))
        .saturating_mul(1_000))
}

pub(crate) fn days_from_civil(year: i32, month: u32, day: u32) -> Result<i64, ()> {
    let dim = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = if month == 2 && leap {
        29
    } else {
        dim[month as usize]
    };
    if day > max_day {
        return Err(());
    }
    let mut days = 0_i64;
    for y in 1970..year {
        days += if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
    }
    for m in 1..month {
        days += i64::from(if m == 2 && leap { 29 } else { dim[m as usize] });
    }
    Ok(days + i64::from(day) - 1)
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
