//! How much of a harness's plan is used, from the harness's own files, so a
//! run can switch agents before hitting a limit rather than after. Only
//! Codex says it: each session log (`$CODEX_HOME/sessions/YYYY/MM/DD/
//! rollout-*.jsonl`) carries `rate_limits` with the used share and reset of
//! its 5-hour and weekly windows. Best effort: a format change only means no
//! quota is shown.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agentux_api::rpc::QuotaWindow;
use agentux_api::{HarnessLimit, LimitKind};
use serde_json::Value;

/// How much of a session log's end is read.
const TAIL: u64 = 512 * 1024;

/// A window this used is spent.
pub const SPENT_PERCENT: f64 = 99.5;

/// Codex's windows from its newest session log under `codex_home`.
pub fn codex(codex_home: &Path) -> Vec<QuotaWindow> {
    let Some(log) = newest_log(&codex_home.join("sessions")) else {
        return Vec::new();
    };
    let Some(text) = tail(&log) else {
        return Vec::new();
    };
    let mtime = fs::metadata(&log)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    for line in text.lines().rev() {
        if !line.contains("rate_limits") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let seen_at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .map_or(mtime, |t| t.as_millisecond());
        let Some(limits) = find(&value, "rate_limits") else {
            continue;
        };
        let windows: Vec<QuotaWindow> = ["primary", "secondary"]
            .iter()
            .filter_map(|name| window(limits.get(*name)?, seen_at))
            .collect();
        if !windows.is_empty() {
            return windows;
        }
    }
    Vec::new()
}

/// The Codex home: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
}

/// A limit for each spent window that has not reset at `now`.
pub fn spent(harness: &str, windows: &[QuotaWindow], now: i64) -> Option<HarnessLimit> {
    windows
        .iter()
        .filter(|w| w.used_percent >= SPENT_PERCENT && w.resets_at.is_some_and(|r| r > now))
        .max_by_key(|w| w.resets_at)
        .map(|w| HarnessLimit {
            harness: harness.to_string(),
            model: None,
            kind: if w.label == "weekly" {
                LimitKind::Weekly
            } else {
                LimitKind::Session
            },
            until: w.resets_at,
            until_known: true,
            message: format!(
                "its {} window is {:.0}% used, as its own session log says",
                w.label, w.used_percent
            ),
            at: now,
        })
}

fn window(value: &Value, seen_at: i64) -> Option<QuotaWindow> {
    let used_percent = value.get("used_percent")?.as_f64()?;
    let minutes = value.get("window_minutes").and_then(Value::as_u64);
    let resets_at = value
        .get("resets_at")
        .and_then(Value::as_i64)
        .map(|secs| secs * 1000)
        .or_else(|| {
            let secs = value.get("resets_in_seconds")?.as_f64()?;
            Some(seen_at + (secs * 1000.0) as i64)
        });
    let label = match minutes {
        Some(300) => "5h".to_string(),
        Some(10080) => "weekly".to_string(),
        Some(m) => format!("{m}m"),
        None => "window".to_string(),
    };
    Some(QuotaWindow {
        label,
        used_percent,
        resets_at,
        seen_at,
    })
}

/// The first object called `key`, anywhere in `value`.
fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .filter(|v| v.is_object())
            .or_else(|| map.values().find_map(|v| find(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find(v, key)),
        _ => None,
    }
}

fn newest_log(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() && depth < 4 {
                stack.push((path, depth + 1));
            } else if meta.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
                && let Ok(modified) = meta.modified()
                && best.as_ref().is_none_or(|(t, _)| modified > *t)
            {
                best = Some((modified, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

fn tail(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_log(home: &Path, day: &str, lines: &[&str]) {
        let dir = home.join("sessions").join(day);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("rollout-test.jsonl"), lines.join("\n")).unwrap();
    }

    #[test]
    fn reads_the_windows_of_the_newest_log() {
        let home = tempfile::tempdir().unwrap();
        write_log(
            home.path(),
            "2026/10/21",
            &[
                r#"{"timestamp":"2026-10-21T12:00:00Z","type":"session_meta","payload":{}}"#,
                r#"{"timestamp":"2026-10-21T12:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":42.0,"window_minutes":300,"resets_in_seconds":3600},"secondary":{"used_percent":100.0,"window_minutes":10080,"resets_at":1793000000}}}}"#,
            ],
        );
        let windows = codex(home.path());
        let noon = "2026-10-21T12:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond();
        assert_eq!(
            windows,
            [
                QuotaWindow {
                    label: "5h".into(),
                    used_percent: 42.0,
                    resets_at: Some(noon + 3_600_000),
                    seen_at: noon,
                },
                QuotaWindow {
                    label: "weekly".into(),
                    used_percent: 100.0,
                    resets_at: Some(1_793_000_000_000),
                    seen_at: noon,
                },
            ]
        );
        let limit = spent("codex", &windows, noon).unwrap();
        assert_eq!(limit.kind, LimitKind::Weekly);
        assert_eq!(limit.until, Some(1_793_000_000_000));
        // Reset already: nothing is spent.
        assert!(spent("codex", &windows, 1_793_000_000_001).is_none());
    }

    #[test]
    fn no_logs_or_other_formats_are_no_quota() {
        let home = tempfile::tempdir().unwrap();
        assert!(codex(home.path()).is_empty());
        write_log(
            home.path(),
            "2026/10/21",
            &["not json", r#"{"rate_limits": 3}"#],
        );
        assert!(codex(home.path()).is_empty());
    }
}
