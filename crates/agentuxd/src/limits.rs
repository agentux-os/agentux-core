//! Usage limits from what harnesses say when they run out: Claude Code's
//! session and weekly limits, Codex's usage limit, Gemini and Antigravity
//! quotas, HTTP 429s, credit balances. [`parse`] finds a limit in an error or
//! a turn's reply and when it resets, so the engine can take the harness (or
//! the model) out of use until then instead of failing the run or letting it
//! hang.

use std::sync::LazyLock;

use agentux_api::{HarnessLimit, LimitKind};
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, Zoned};
use regex::Regex;

/// A limit found in a harness's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub kind: LimitKind,
    /// The model the message names, when the limit is for one model.
    pub model: Option<String>,
    /// When the harness said it resets.
    pub resets: Option<Timestamp>,
}

/// A turn's reply this long is the agent's work, not a limit notice.
const REPLY_MAX: usize = 600;

/// Words that only a limit notice uses.
static NOTICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)
        usage[\s_-]limit | limit\s+reached | hit\s+your\s+(?:usage\s+)?limit
        | reached\s+your\s+(?:\w+\s+){0,2}limit | quota\s+exceeded | exceeded\s+your\s+(?:current\s+)?quota
        | resource[_\s]exhausted | exhausted\s+your | rate[\s_-]limit(?:ed|\s+exceeded|_error)
        | too\s+many\s+requests | credit\s+balance\s+is\s+too\s+low | insufficient[_\s](?:quota|credits?|funds)
        | out\s+of\s+credits | quota\s+will\s+reset",
    )
    .expect("valid regex")
});

/// What an error may also say (a turn's reply may say it about code).
static ERROR_ONLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b429\b|rate[\s_-]?limit").expect("valid regex"));

static CREDITS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)credit\s+balance|insufficient[_\s](?:quota|credits?|funds)|out\s+of\s+credits|add\s+(?:more\s+)?(?:funds|credits)|billing",
    )
    .expect("valid regex")
});
static WEEKLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bweekly\b|\bper\s+week\b|7-day").expect("valid regex"));
static SESSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)5[\s-]hour|session\s+limit|usage[\s_-]limit|hit\s+your\s+(?:usage\s+)?limit|limit\s+reached")
        .expect("valid regex")
});

static MODEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(opus|sonnet|haiku|gemini-[\w.-]*\w|gpt-[\w.-]*\w|o\d-[\w.-]*\w)\b")
        .expect("valid regex")
});

/// Claude Code's `Claude AI usage limit reached|1761000000`.
static EPOCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"limit reached\|(\d{10})\b").expect("valid regex"));

/// `try again in 2 days 3 hours`, `retry in 41.5s`, `reset after 3h12m`,
/// `"retryDelay": "41s"`, `retry-after: 120`.
static IN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:(?:try\s+again|retry|resets?|available\s+again)\s+(?:in|after)|retry[-_ ]?(?:after|delay)"?\s*[:=]\s*"?)\s*((?:\d+(?:\.\d+)?\s*(?:days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)?[\s,]*(?:and\s+)?)+)"#,
    )
    .expect("valid regex")
});
static PART: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)?")
        .expect("valid regex")
});

/// `resets 3pm`, `resets at 2:30am (America/Sao_Paulo)`, `resets Oct 27,
/// 9am`, `try again at 3:05 PM`, `try again at Oct 27th, 2026 3:05 PM`.
static AT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)(?:resets?|try\s+again|available\s+again)\s+(?:at\s+|on\s+)?
        (?:(?P<mon>jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+(?P<day>\d{1,2})(?:st|nd|rd|th)?,?\s*(?:(?P<year>\d{4}),?\s*)?(?:at\s+)?)?
        (?P<hour>\d{1,2})(?::(?P<min>\d{2}))?\s*(?P<ampm>[ap]\.?m\.?)?
        (?:\s*\((?P<tz>[A-Za-z_]+(?:/[A-Za-z_+-]+)+|UTC)\))?",
    )
    .expect("valid regex")
});

/// Finds a limit in `text`. `reply`: the text is a turn's reply rather than
/// an error, so it must be short and use a limit notice's own words.
pub fn parse(text: &str, reply: bool, now: &Zoned) -> Option<Found> {
    if reply && text.trim().chars().count() > REPLY_MAX {
        return None;
    }
    if !NOTICE.is_match(text) && (reply || !ERROR_ONLY.is_match(text)) {
        return None;
    }
    let kind = if CREDITS.is_match(text) {
        LimitKind::Credits
    } else if WEEKLY.is_match(text) {
        LimitKind::Weekly
    } else if SESSION.is_match(text) {
        LimitKind::Session
    } else {
        LimitKind::Rate
    };
    let model = MODEL.captures(text).map(|c| c[1].to_ascii_lowercase());
    Some(Found {
        kind,
        model,
        resets: resets(text, now),
    })
}

fn resets(text: &str, now: &Zoned) -> Option<Timestamp> {
    if let Some(c) = EPOCH.captures(text) {
        return Timestamp::from_second(c[1].parse().ok()?).ok();
    }
    if let Some(c) = IN.captures(text) {
        let mut seconds = 0.0;
        for part in PART.captures_iter(&c[1]) {
            let n: f64 = part[1].parse().ok()?;
            let unit = part.get(2).map_or("s", |m| m.as_str()).to_ascii_lowercase();
            seconds += n * match unit.chars().next() {
                Some('d') => 86_400.0,
                Some('h') => 3_600.0,
                Some('m') => 60.0,
                _ => 1.0,
            };
        }
        if seconds > 0.0 {
            return now
                .timestamp()
                .checked_add(Span::new().seconds(seconds.ceil() as i64))
                .ok();
        }
    }
    let c = AT
        .captures_iter(text)
        .find(|c| c.name("ampm").is_some() || c.name("min").is_some())?;
    let tz = match c.name("tz") {
        Some(name) => TimeZone::get(name.as_str()).ok()?,
        None => now.time_zone().clone(),
    };
    let local = now.with_time_zone(tz.clone());
    let mut hour: i8 = c["hour"].parse().ok()?;
    let minute: i8 = c.name("min").map_or(Ok(0), |m| m.as_str().parse()).ok()?;
    if let Some(ampm) = c.name("ampm") {
        let pm = ampm.as_str().to_ascii_lowercase().starts_with('p');
        hour = match (hour, pm) {
            (12, false) => 0,
            (12, true) => 12,
            (h, true) => h + 12,
            (h, false) => h,
        };
    }
    let time = Time::new(hour, minute, 0, 0).ok()?;
    match c.name("mon") {
        Some(mon) => {
            let month = MONTHS
                .iter()
                .position(|m| mon.as_str().eq_ignore_ascii_case(m))? as i8
                + 1;
            let day: i8 = c["day"].parse().ok()?;
            let year: i16 = c
                .name("year")
                .map_or(Ok(local.year()), |y| y.as_str().parse())
                .ok()?;
            let at = Date::new(year, month, day)
                .ok()?
                .to_datetime(time)
                .to_zoned(tz.clone())
                .ok()?;
            // `Jan 2` read in late December is next year's.
            if c.name("year").is_none() && at.timestamp() < now.timestamp() - Span::new().hours(24)
            {
                let next = Date::new(year + 1, month, day).ok()?;
                return Some(next.to_datetime(time).to_zoned(tz).ok()?.timestamp());
            }
            Some(at.timestamp())
        }
        None => {
            let today = local.date().to_datetime(time).to_zoned(tz.clone()).ok()?;
            if today.timestamp() > now.timestamp() {
                Some(today.timestamp())
            } else {
                let tomorrow = local.date().tomorrow().ok()?;
                Some(tomorrow.to_datetime(time).to_zoned(tz).ok()?.timestamp())
            }
        }
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// How long a harness stays out of use when it did not say when it resets:
/// then the daemon tries it again (and learns the time if it says so). Credits
/// come back only when the human says so.
pub fn guess(kind: LimitKind) -> Option<Span> {
    match kind {
        LimitKind::Rate => Some(Span::new().minutes(2)),
        LimitKind::Session => Some(Span::new().hours(1)),
        LimitKind::Weekly => Some(Span::new().hours(24)),
        LimitKind::Credits => None,
    }
}

/// The limit to record for `harness` from what was found at `now`.
pub fn limit(harness: &str, message: &str, found: Found, now: &Zoned) -> HarnessLimit {
    let guessed = guess(found.kind).and_then(|span| now.timestamp().checked_add(span).ok());
    // At least a minute: a reset time already past (a stale message, a
    // clock off) would retry the harness in a tight loop.
    let soonest = now.timestamp().as_millisecond() + 60_000;
    HarnessLimit {
        harness: harness.to_string(),
        model: found.model,
        kind: found.kind,
        until: found
            .resets
            .or(guessed)
            .map(|t| t.as_millisecond().max(soonest)),
        until_known: found.resets.is_some(),
        message: message.trim().chars().take(2_000).collect(),
        at: now.timestamp().as_millisecond(),
    }
}

/// The limit that keeps `harness` (playing with `model`) out of use, among
/// `limits` in force: one for the whole harness, or for that model (a
/// model limit matches a role's model by name: `opus` matches
/// `claude-opus-4-1`).
pub fn blocking<'a>(
    limits: &'a [HarnessLimit],
    harness: &str,
    model: Option<&str>,
) -> Option<&'a HarnessLimit> {
    limits.iter().find(|l| {
        l.harness == harness
            && match (&l.model, model) {
                (None, _) => true,
                (Some(limited), Some(model)) => {
                    let model = model.to_ascii_lowercase();
                    model == *limited || model.contains(limited.as_str())
                }
                (Some(_), None) => false,
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tuesday 2026-10-20 10:00 in São Paulo (UTC-3).
    fn now() -> Zoned {
        "2026-10-20T10:00:00-03:00[America/Sao_Paulo]"
            .parse()
            .unwrap()
    }

    fn at(text: &str) -> String {
        let found = parse(text, false, &now()).unwrap_or_else(|| panic!("no limit in {text:?}"));
        found
            .resets
            .map(|t| {
                t.to_zoned(TimeZone::get("America/Sao_Paulo").unwrap())
                    .to_string()
            })
            .unwrap_or_default()
    }

    #[test]
    fn claude_code_limits() {
        let found = parse("Claude AI usage limit reached|1792512000", true, &now()).unwrap();
        assert_eq!(found.kind, LimitKind::Session);
        assert_eq!(
            found.resets,
            Some(Timestamp::from_second(1_792_512_000).unwrap())
        );

        assert_eq!(
            at("5-hour limit reached ∙ resets 3pm"),
            "2026-10-20T15:00:00-03:00[America/Sao_Paulo]"
        );
        // The time already passed today: tomorrow's.
        assert_eq!(
            at("You've hit your limit · resets 2am (America/Sao_Paulo)"),
            "2026-10-21T02:00:00-03:00[America/Sao_Paulo]"
        );
        assert_eq!(
            at("You've hit your limit · resets 9:30am (America/New_York)"),
            "2026-10-20T10:30:00-03:00[America/Sao_Paulo]"
        );
        let weekly = parse("Weekly limit reached ∙ resets Oct 27, 9am", false, &now()).unwrap();
        assert_eq!(weekly.kind, LimitKind::Weekly);
        assert_eq!(
            at("Weekly limit reached ∙ resets Oct 27, 9am"),
            "2026-10-27T09:00:00-03:00[America/Sao_Paulo]"
        );

        let opus = parse(
            "Opus weekly limit reached ∙ resets Oct 27, 9am",
            false,
            &now(),
        )
        .unwrap();
        assert_eq!(opus.model.as_deref(), Some("opus"));

        let credits = parse("Credit balance is too low", false, &now()).unwrap();
        assert_eq!((credits.kind, credits.resets), (LimitKind::Credits, None));
    }

    #[test]
    fn codex_limits() {
        let text =
            "You've hit your usage limit. Upgrade to Pro or try again in 2 days 3 hours 5 minutes.";
        assert_eq!(at(text), "2026-10-22T13:05:00-03:00[America/Sao_Paulo]");
        assert_eq!(parse(text, false, &now()).unwrap().kind, LimitKind::Session);
        assert_eq!(
            at("You've hit your usage limit. Try again at 3:05 PM."),
            "2026-10-20T15:05:00-03:00[America/Sao_Paulo]"
        );
        assert_eq!(
            at("You've hit your usage limit. Try again at Nov 3rd, 2026 8:00 AM."),
            "2026-11-03T08:00:00-03:00[America/Sao_Paulo]"
        );
    }

    #[test]
    fn gemini_and_http_limits() {
        assert_eq!(
            at(
                "Quota exceeded for quota metric 'Generate Content' of model gemini-2.5-pro. Please retry in 41.5s."
            ),
            "2026-10-20T10:00:42-03:00[America/Sao_Paulo]"
        );
        let found = parse(r#"RESOURCE_EXHAUSTED {"retryDelay": "30s"}"#, false, &now()).unwrap();
        assert_eq!(found.kind, LimitKind::Rate);
        assert_eq!(
            at(
                "You have exhausted your capacity on this model. Your quota will reset after 3h12m."
            ),
            "2026-10-20T13:12:00-03:00[America/Sao_Paulo]"
        );
        assert_eq!(
            parse("Quota exceeded for model gemini-2.5-pro", false, &now())
                .unwrap()
                .model
                .as_deref(),
            Some("gemini-2.5-pro")
        );
        let http = parse("API error 429: retry-after: 120", false, &now()).unwrap();
        assert_eq!(http.kind, LimitKind::Rate);
        assert_eq!(
            at("API error 429: retry-after: 120"),
            "2026-10-20T10:02:00-03:00[America/Sao_Paulo]"
        );
    }

    #[test]
    fn work_is_not_a_limit() {
        assert_eq!(
            parse(
                "Added a rate limiter that returns 429 when a client sends too much.",
                true,
                &now()
            ),
            None
        );
        assert_eq!(parse("Implemented the login page.", true, &now()), None);
        assert_eq!(parse("the agent crashed", false, &now()), None);
        let long = format!(
            "Quota exceeded is now handled. {}",
            "More details. ".repeat(60)
        );
        assert_eq!(parse(&long, true, &now()), None);
        // An error may be terser.
        assert!(parse("HTTP 429", false, &now()).is_some());
    }

    #[test]
    fn unknown_reset_times_are_guessed_and_credits_wait_for_the_human() {
        let session = limit(
            "codex",
            "You've hit your usage limit.",
            parse("You've hit your usage limit.", false, &now()).unwrap(),
            &now(),
        );
        assert!(!session.until_known);
        assert_eq!(
            session.until,
            Some(now().timestamp().as_millisecond() + 3_600_000)
        );
        let credits = limit(
            "claude-code",
            "Credit balance is too low",
            parse("Credit balance is too low", false, &now()).unwrap(),
            &now(),
        );
        assert_eq!(credits.until, None);
    }

    #[test]
    fn blocking_limits() {
        let mk = |harness: &str, model: Option<&str>| HarnessLimit {
            harness: harness.into(),
            model: model.map(str::to_string),
            kind: LimitKind::Weekly,
            until: None,
            until_known: false,
            message: String::new(),
            at: 0,
        };
        let limits = [mk("claude-code", Some("opus")), mk("antigravity", None)];
        assert!(blocking(&limits, "claude-code", Some("claude-opus-4-1")).is_some());
        assert!(blocking(&limits, "claude-code", Some("sonnet")).is_none());
        assert!(blocking(&limits, "claude-code", None).is_none());
        assert!(blocking(&limits, "antigravity", Some("x")).is_some());
        assert!(blocking(&limits, "codex", None).is_none());
    }
}
