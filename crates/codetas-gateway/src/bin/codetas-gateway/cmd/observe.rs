use super::*;
use codetas_gateway::read_observability_breakdown;
use codetas_gateway::read_observability_events_after_sequence;
use codetas_gateway::read_observability_summary;
use codetas_gateway::read_recent_observability_events;

use std::io::{self, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;
pub(crate) fn observe(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{OBSERVE_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "summary".into());
    let json_output = take_flag(&mut args, "--json")?;
    let directory = config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("observability");
    match action.as_str() {
        "summary" | "usage" => {
            finish_args(&args, OBSERVE_USAGE)?;
            let value = read_observability_summary(directory);
            if json_output {
                print_json(&value)
            } else {
                print_json(&value)
            }
        }
        "events" | "logs" => {
            let since = optional_u64(&mut args, "--since-ms")?.unwrap_or(0);
            let limit = optional_usize(&mut args, "--limit")?
                .unwrap_or(200)
                .clamp(1, 500);
            let provider = take_option(&mut args, "--provider")?;
            let model = take_option(&mut args, "--model")?;
            let status = take_option(&mut args, "--status")?
                .map(|value| {
                    value
                        .parse::<u16>()
                        .map_err(|_| "--status must be an HTTP status code".to_string())
                })
                .transpose()?;
            let jsonl = take_flag(&mut args, "--jsonl")?;
            if json_output && jsonl {
                return Err("--json and --jsonl cannot be combined".into());
            }
            finish_args(&args, OBSERVE_USAGE)?;
            let events = read_recent_observability_events(directory, since, 500)
                .into_iter()
                .filter(|item| {
                    provider
                        .as_deref()
                        .is_none_or(|value| item.provider_id.as_deref() == Some(value))
                })
                .filter(|item| {
                    model.as_deref().is_none_or(|value| {
                        item.exposed_model == value || item.upstream_model.as_deref() == Some(value)
                    })
                })
                .filter(|item| status.is_none_or(|value| item.status_code == value))
                .take(limit)
                .collect::<Vec<_>>();
            if jsonl {
                for item in events {
                    println!(
                        "{}",
                        serde_json::to_string(&item).map_err(|error| error.to_string())?
                    );
                }
                Ok(())
            } else if json_output {
                print_json(&events)
            } else {
                for item in events {
                    println!(
                        "{}  {}  {}  {}ms  {}",
                        item.timestamp_ms,
                        item.status_code,
                        item.exposed_model,
                        item.latency_ms,
                        item.outcome
                    );
                }
                Ok(())
            }
        }
        "breakdown" => {
            let since = optional_u64(&mut args, "--since-ms")?.unwrap_or(0);
            let limit = optional_usize(&mut args, "--limit")?
                .unwrap_or(50_000)
                .clamp(1, 50_000);
            finish_args(&args, OBSERVE_USAGE)?;
            let value = read_observability_breakdown(directory, since, limit);
            if json_output {
                print_json(&value)
            } else {
                print_json(&value)
            }
        }
        "follow" => {
            if json_output {
                return Err(
                    "observe follow is an unbounded stream; use --jsonl instead of --json".into(),
                );
            }
            let since = optional_u64(&mut args, "--since-ms")?.unwrap_or_else(now_millis);
            let interval = optional_u64(&mut args, "--interval-ms")?
                .unwrap_or(1_000)
                .clamp(250, 60_000);
            let provider = take_option(&mut args, "--provider")?;
            let model = take_option(&mut args, "--model")?;
            let status = take_option(&mut args, "--status")?
                .map(|value| {
                    value
                        .parse::<u16>()
                        .map_err(|_| "--status must be an HTTP status code".to_string())
                })
                .transpose()?;
            let jsonl = take_flag(&mut args, "--jsonl")?;
            finish_args(&args, OBSERVE_USAGE)?;
            follow_observability(
                &directory,
                since,
                interval,
                provider.as_deref(),
                model.as_deref(),
                status,
                jsonl,
            )
        }
        _ => Err(format!("unknown observe action: {action}\n{OBSERVE_USAGE}")),
    }
}

pub(crate) fn follow_observability(
    directory: &Path,
    cursor_ms: u64,
    interval_ms: u64,
    provider: Option<&str>,
    model: Option<&str>,
    status: Option<u16>,
    jsonl: bool,
) -> Result<(), String> {
    let mut cursor = (cursor_ms, 0_u64);
    loop {
        let events = read_observability_events_after_sequence(directory, cursor_ms, cursor.1, 500);
        for item in events {
            let event_cursor = (item.timestamp_ms, item.ledger_sequence);
            if item.ledger_sequence <= cursor.1 {
                continue;
            }
            cursor = (cursor.0.max(event_cursor.0), event_cursor.1);
            if provider.is_some_and(|value| item.provider_id.as_deref() != Some(value))
                || model.is_some_and(|value| {
                    item.exposed_model != value && item.upstream_model.as_deref() != Some(value)
                })
                || status.is_some_and(|value| item.status_code != value)
            {
                continue;
            }
            if jsonl {
                println!(
                    "{}",
                    serde_json::to_string(&item).map_err(|error| error.to_string())?
                );
            } else {
                println!(
                    "{}  {}  {}  {}ms  {}",
                    item.timestamp_ms,
                    item.status_code,
                    item.exposed_model,
                    item.latency_ms,
                    item.outcome
                );
            }
        }
        io::stdout()
            .flush()
            .map_err(|error| format!("failed to flush observation stream: {error}"))?;
        thread::sleep(Duration::from_millis(interval_ms));
    }
}

pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn follow_cursor_keeps_distinct_events_from_the_same_millisecond() {
        let mut cursor = (1_000_u64, 0_u64);
        let events = [(1_000, 41), (1_000, 42), (1_000, 43)];
        let accepted = events
            .into_iter()
            .filter(|event| {
                if *event <= cursor {
                    return false;
                }
                cursor = *event;
                true
            })
            .collect::<Vec<_>>();

        assert_eq!(accepted, events.to_vec());
        assert_eq!(cursor, (1_000, 43));
    }
}
