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
    let time = rest.trim_end_matches('Z');
    let time = time.split(['+', '-']).next().unwrap_or(time);
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
    civil_utc_to_ms(year, month, day, hour, minute, second)
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
