//! `ta watch` - block until tasks matching a filter change past a cursor.
//!
//! Polls the mutation log for events newer than `--since`; on the first match it
//! waits a short `--holdout` to batch a burst, then prints a per-task diff (via
//! the shared `format::render_state_diff`) and exits 0. If nothing matches before
//! `--timeout`, it prints `No updates yet` to stderr and exits 1 - so a caller
//! loop is `while :; do ta watch --since "$s" ... && break; done`.

use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::action::list::validate_criteria;
use crate::action::watch::poll;
use crate::error::DynError;
use crate::format::{emit, render_state_diff, state_diff, want_color, OutputArgs};
use crate::storage::EventStore;

/// How often to re-check the log while blocking.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[allow(clippy::too_many_arguments)] // a watch is genuinely this many knobs
pub fn cmd_watch(
    store: &impl EventStore,
    criteria: &[String],
    open: bool,
    ready: bool,
    since: u64,
    timeout: Duration,
    holdout: Duration,
    output: &OutputArgs,
) -> Result<(), DynError> {
    // Reject a bad filter before blocking.
    validate_criteria(criteria)?;

    let color = want_color(output.no_color);
    let deadline = Instant::now() + timeout;

    loop {
        if collect(store, criteria, open, ready, since, color)?.is_some() {
            // A match arrived: hold out briefly (bounded by the time left) to batch
            // a burst, then emit the accumulated set.
            let wait = holdout.min(deadline.saturating_duration_since(Instant::now()));
            if !wait.is_zero() {
                sleep(wait);
            }
            if let Some((human, value)) = collect(store, criteria, open, ready, since, color)? {
                emit(output, &human, &value);
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            eprintln!("No updates yet");
            std::process::exit(1);
        }
        sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// One snapshot: the joined human diff blocks and the JSON array for the tasks
/// that changed since `since` and match the filter. `None` when nothing renderable
/// changed (so the caller keeps waiting).
fn collect(
    store: &impl EventStore,
    criteria: &[String],
    open: bool,
    ready: bool,
    since: u64,
    color: bool,
) -> Result<Option<(String, Value)>, DynError> {
    let updates = poll(store, criteria, open, ready, since)?;
    let mut blocks = Vec::new();
    let mut items = Vec::new();
    for u in &updates {
        let body = render_state_diff(u.before.as_ref(), u.after.as_ref(), color);
        if body.is_empty() {
            continue; // touched but net-zero change (e.g. a merge artifact)
        }
        blocks.push(format!("{}:\n{body}", u.id));
        let (removed, added) = state_diff(u.before.as_ref(), u.after.as_ref());
        items.push(json!({ "id": u.id.clone(), "removed": removed, "added": added }));
    }
    if blocks.is_empty() {
        return Ok(None);
    }
    Ok(Some((blocks.join("\n\n"), Value::Array(items))))
}

/// Parse a human duration like `1m55s`, `30s`, `500ms`, or `2h` into a
/// [`Duration`]: a sequence of `<integer><unit>` groups (units `h`, `m`, `s`,
/// `ms`), summed. Used by clap for `--timeout`/`--holdout` (it parses the string
/// defaults too, so a bad default would fail loudly at startup).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty duration (e.g. `1m55s`, `30s`, `500ms`)".to_string());
    }
    let bytes = trimmed.as_bytes();
    let mut millis: u64 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let num_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == num_start {
            return Err(format!(
                "invalid duration `{trimmed}`: expected `<number><unit>` (units h, m, s, ms)"
            ));
        }
        let num: u64 = trimmed[num_start..i]
            .parse()
            .map_err(|_| format!("invalid number in duration `{trimmed}`"))?;
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let scale = match &trimmed[unit_start..i] {
            "h" => 3_600_000,
            "m" => 60_000,
            "s" => 1_000,
            "ms" => 1,
            "" => {
                return Err(format!(
                    "invalid duration `{trimmed}`: missing unit after {num} (use h, m, s, ms)"
                ))
            }
            other => {
                return Err(format!(
                    "invalid duration `{trimmed}`: unknown unit `{other}` (use h, m, s, ms)"
                ))
            }
        };
        millis = num
            .checked_mul(scale)
            .and_then(|ms| millis.checked_add(ms))
            .ok_or_else(|| format!("duration `{trimmed}` is too large"))?;
    }
    Ok(Duration::from_millis(millis))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;

    #[test]
    fn parse_duration_sums_unit_groups() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("1m55s").unwrap(), Duration::from_secs(115));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_hours(2));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_mins(90));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err(), "empty");
        assert!(parse_duration("abc").is_err(), "no number");
        assert!(parse_duration("10").is_err(), "missing unit");
        assert!(parse_duration("10x").is_err(), "unknown unit");
    }
}
