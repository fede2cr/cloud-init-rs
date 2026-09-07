//! Port of `cloudinit/analyze/dump.py` — turning cloud-init log lines into events.
//!
//! The log is an untrusted, attacker-influenced input (user-data ends up in it), so
//! every step here is total: a malformed line is skipped, never panicked on.

use serde_json::{Map, Value};

/// One parsed log record, kept as a JSON object because `analyze dump` emits it
/// verbatim and `analyze show`/`blame` may instead be fed a JSON file.
pub type Event = Map<String, Value>;

/// The cases upstream signals with `ValueError`, reported as "Skipping invalid entry".
#[derive(Debug, Clone, Copy)]
pub struct InvalidEntry;

const AMAZON_LINUX_2_SEP: &str = " cloud-init[";
const SEPARATORS: [&str; 3] = [" - ", " [CLOUDINIT] ", AMAZON_LINUX_2_SEP];
const CI_EVENT_MATCHES: [&str; 3] = ["start:", "finish:", "Cloud-init v."];

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn stage_to_description(name: &str) -> Option<&'static str> {
    Some(match name {
        "finished" => "finished running cloud-init",
        "init-local" => "starting search for local datasources",
        "init-network" | "init" => "searching for network datasources",
        "modules-config" => "running config modules",
        "modules-final" => "finalizing modules",
        "modules" => "running modules for",
        "single" => "running single module ",
        _ => return None,
    })
}

/// Parse every cloud-init event out of a raw log.
///
/// Upstream keeps the previously parsed event in scope across iterations, so an
/// unparsable line re-emits its predecessor and a line matching two of the event
/// markers is emitted twice. Both are reproduced deliberately: the differential
/// harness compares against Python, not against what Python meant to do.
/// See docs/COMPAT.md B2 and B4.
pub fn dump_events(rawdata: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut event: Option<Event> = None;
    for line in rawdata.lines() {
        for needle in CI_EVENT_MATCHES {
            if !line.contains(needle) {
                continue;
            }
            match parse_ci_logline(line) {
                Ok(parsed) => event = parsed,
                Err(InvalidEntry) => eprintln!("Skipping invalid entry"),
            }
            if let Some(parsed) = &event {
                events.push(Value::Object(parsed.clone()));
            }
        }
    }
    events
}

/// `parse_ci_logline()`. Kept as one function so it can be diffed line-by-line
/// against the upstream implementation it mirrors.
#[allow(clippy::too_many_lines)]
fn parse_ci_logline(line: &str) -> Result<Option<Event>, InvalidEntry> {
    let Some(sep) = SEPARATORS.iter().copied().find(|s| line.contains(s)) else {
        return Ok(None);
    };
    let split: Vec<&str> = line.split(sep).collect();
    let [timehost, eventstr] = split.as_slice() else {
        return Err(InvalidEntry);
    };
    let mut timehost = (*timehost).to_owned();
    let mut eventstr = (*eventstr).to_owned();

    // `journalctl -o short-precise` appends "<host> <unit>:"; drop the trailing field.
    if timehost.ends_with(':') {
        let fields: Vec<&str> = timehost.split_whitespace().collect();
        timehost = fields
            .split_last()
            .map(|(_, head)| head.join(" "))
            .unwrap_or_default();
    }

    let timestampstr = if timehost.contains(',') {
        let parts: Vec<&str> = timehost.split(',').collect();
        let [head, extra] = parts.as_slice() else {
            return Err(InvalidEntry);
        };
        let millis = extra.split_whitespace().next().ok_or(InvalidEntry)?;
        format!("{head},{millis}")
    } else {
        let hostname = timehost
            .split_whitespace()
            .next_back()
            .ok_or(InvalidEntry)?
            .to_owned();
        if sep == AMAZON_LINUX_2_SEP {
            // No hostname on these lines, and the PID leads the event text.
            eventstr = split_once_whitespace(&eventstr)
                .ok_or(InvalidEntry)?
                .to_owned();
            timehost.trim().to_owned()
        } else {
            timehost
                .split(hostname.as_str())
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        }
    };

    let (event_type, event_name, description) = if eventstr.contains("Cloud-init v.") {
        let Some(after) = eventstr.split("running").nth(1) else {
            // The "finished at" banner is not the start of anything.
            return Ok(None);
        };
        let parts: Vec<&str> = after.trim_start().split(" at ").collect();
        let [stage, _] = parts.as_slice() else {
            return Err(InvalidEntry);
        };
        let name = stage.replace('\'', "").replace(':', "-");
        let name = if name == "init" {
            "init-network".to_owned()
        } else {
            name
        };
        let description = stage_to_description(&name).ok_or(InvalidEntry)?.to_owned();
        ("start".to_owned(), name, description)
    } else {
        let fields: Vec<&str> = eventstr.split_whitespace().collect();
        let [_level, event_type, event_name, ..] = fields.as_slice() else {
            return Err(InvalidEntry);
        };
        let description = eventstr
            .split(event_name)
            .nth(1)
            .ok_or(InvalidEntry)?
            .trim()
            .to_owned();
        (
            (*event_type).to_owned(),
            (*event_name).to_owned(),
            description,
        )
    };

    let timestamp = parse_timestamp(&timestampstr)?;
    let event_type = event_type.trim_end_matches(':').to_owned();

    let mut event = Event::new();
    event.insert(
        "name".to_owned(),
        Value::String(event_name.trim_end_matches(':').to_owned()),
    );
    event.insert("description".to_owned(), Value::String(description.clone()));
    event.insert(
        "timestamp".to_owned(),
        serde_json::Number::from_f64(timestamp).map_or(Value::Null, Value::Number),
    );
    event.insert("origin".to_owned(), Value::String("cloudinit".to_owned()));
    event.insert("event_type".to_owned(), Value::String(event_type.clone()));

    if event_type == "finish" {
        let result = description.split(':').next().unwrap_or_default().to_owned();
        if result.is_empty() {
            // `str.split("")` is a ValueError in Python; upstream skips the line.
            return Err(InvalidEntry);
        }
        let tail = description
            .split(result.as_str())
            .nth(1)
            .ok_or(InvalidEntry)?
            .trim_start_matches(':')
            .trim()
            .to_owned();
        event.insert("result".to_owned(), Value::String(result));
        event.insert("description".to_owned(), Value::String(tail));
    }

    Ok(Some(event))
}

/// `text.split(maxsplit=1)[1]` — everything after the first whitespace-delimited token.
fn split_once_whitespace(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let idx = trimmed.find(char::is_whitespace)?;
    Some(trimmed.get(idx..)?.trim_start())
}

/// `dump.parse_timestamp()` — every stamp is interpreted as UTC, as upstream does.
fn parse_timestamp(text: &str) -> Result<f64, InvalidEntry> {
    let first = text.split_whitespace().next().ok_or(InvalidEntry)?;
    if MONTHS.contains(&first) {
        return parse_syslog(text);
    }
    if text.contains(',') {
        return parse_asctime(text);
    }
    parse_iso8601(text)
}

/// `%b %d %H:%M:%S[.%f]` plus the current year, as upstream grafts on.
fn parse_syslog(text: &str) -> Result<f64, InvalidEntry> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [month, day, clock] = fields.as_slice() else {
        return Err(InvalidEntry);
    };
    let month = MONTHS
        .iter()
        .position(|m| m == month)
        .ok_or(InvalidEntry)?
        .saturating_add(1);
    let day: i64 = day.parse().map_err(|_| InvalidEntry)?;
    let (hour, minute, second, micros) = parse_clock(clock)?;
    let secs = ci_core::time::epoch_from_civil(
        ci_core::time::current_year(),
        i64::try_from(month).map_err(|_| InvalidEntry)?,
        day,
        hour,
        minute,
        second,
    );
    Ok(compose(secs, micros))
}

/// `%Y-%m-%d %H:%M:%S,%f` — the Python `logging` asctime format cloud-init writes.
fn parse_asctime(text: &str) -> Result<f64, InvalidEntry> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [date, clock] = fields.as_slice() else {
        return Err(InvalidEntry);
    };
    let (whole, frac) = clock.split_once(',').ok_or(InvalidEntry)?;
    let (year, month, day) = parse_date(date)?;
    let (hour, minute, second, _) = parse_clock(whole)?;
    let micros = parse_fraction(frac)?;
    let secs = ci_core::time::epoch_from_civil(year, month, day, hour, minute, second);
    Ok(compose(secs, micros))
}

/// Upstream shells out to GNU `date(1)` for anything else (docs/COMPAT.md 15).
/// Parsing ISO 8601 in process covers the formats journald actually produces
/// without forking twice per line or depending on the ambient locale.
fn parse_iso8601(text: &str) -> Result<f64, InvalidEntry> {
    let text = text.trim();
    let (date, rest) = text
        .split_once('T')
        .or_else(|| text.split_once(' '))
        .ok_or(InvalidEntry)?;
    let (year, month, day) = parse_date(date)?;

    let (clock, offset) = split_offset(rest)?;
    let (hour, minute, second, micros) = parse_clock(clock)?;
    let secs = ci_core::time::epoch_from_civil(year, month, day, hour, minute, second);
    Ok(compose(secs.saturating_sub(offset), micros))
}

/// Split a time-of-day from a trailing `Z`, `+HH:MM` or `-HHMM` zone designator,
/// returning the offset in seconds east of UTC.
fn split_offset(rest: &str) -> Result<(&str, i64), InvalidEntry> {
    if let Some(clock) = rest.strip_suffix('Z') {
        return Ok((clock, 0));
    }
    let Some(idx) = rest.rfind(['+', '-']) else {
        return Ok((rest, 0));
    };
    let clock = rest.get(..idx).ok_or(InvalidEntry)?;
    let zone = rest.get(idx..).ok_or(InvalidEntry)?;
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let digits: String = zone.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 4 {
        return Err(InvalidEntry);
    }
    let hours: i64 = digits
        .get(..2)
        .ok_or(InvalidEntry)?
        .parse()
        .map_err(|_| InvalidEntry)?;
    let minutes: i64 = digits
        .get(2..)
        .ok_or(InvalidEntry)?
        .parse()
        .map_err(|_| InvalidEntry)?;
    Ok((clock, sign * (hours * 3600 + minutes * 60)))
}

fn parse_date(text: &str) -> Result<(i64, i64, i64), InvalidEntry> {
    let fields: Vec<&str> = text.split('-').collect();
    let [year, month, day] = fields.as_slice() else {
        return Err(InvalidEntry);
    };
    Ok((
        year.parse().map_err(|_| InvalidEntry)?,
        month.parse().map_err(|_| InvalidEntry)?,
        day.parse().map_err(|_| InvalidEntry)?,
    ))
}

/// `HH:MM:SS` with an optional `.ffffff`, returning whole units plus microseconds.
fn parse_clock(text: &str) -> Result<(i64, i64, i64, i64), InvalidEntry> {
    let fields: Vec<&str> = text.split(':').collect();
    let [hour, minute, second] = fields.as_slice() else {
        return Err(InvalidEntry);
    };
    let (second, micros) = match second.split_once('.') {
        Some((whole, frac)) => (whole, parse_fraction(frac)?),
        None => (*second, 0),
    };
    Ok((
        hour.parse().map_err(|_| InvalidEntry)?,
        minute.parse().map_err(|_| InvalidEntry)?,
        second.parse().map_err(|_| InvalidEntry)?,
        micros,
    ))
}

/// Python's `%f` accepts one to six digits and pads on the right to microseconds.
fn parse_fraction(text: &str) -> Result<i64, InvalidEntry> {
    if text.is_empty() || text.len() > 6 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(InvalidEntry);
    }
    let mut padded = text.to_owned();
    while padded.len() < 6 {
        padded.push('0');
    }
    padded.parse().map_err(|_| InvalidEntry)
}

#[allow(clippy::cast_precision_loss)]
fn compose(secs: i64, micros: i64) -> f64 {
    secs as f64 + micros as f64 / 1e6
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Event {
        parse_ci_logline(line).unwrap().unwrap()
    }

    #[test]
    fn parses_a_start_banner() {
        let event = parse(
            "2017-05-22 18:02:01,088 - util.py[DEBUG]: Cloud-init v. 0.7.9 running \
             'init-local' at Mon, 22 May 2017 18:02:01 +0000. Up 2.0 seconds.",
        );
        assert_eq!(event["name"], "init-local");
        assert_eq!(event["event_type"], "start");
        assert_eq!(
            event["description"],
            "starting search for local datasources"
        );
        assert_eq!(event["origin"], "cloudinit");
        assert_eq!(event["timestamp"].as_f64().unwrap(), 1_495_476_121.088);
    }

    #[test]
    fn maps_the_init_stage_onto_init_network() {
        let event = parse(
            "2017-05-22 18:02:01,088 - util.py[DEBUG]: Cloud-init v. 0.7.9 running \
             'init' at Mon, 22 May 2017 18:02:01 +0000. Up 2.0 seconds.",
        );
        assert_eq!(event["name"], "init-network");
        assert_eq!(event["description"], "searching for network datasources");
    }

    #[test]
    fn skips_the_finished_banner() {
        assert!(parse_ci_logline(
            "2017-05-22 18:02:01,088 - util.py[DEBUG]: Cloud-init v. 0.7.9 finished \
             at Mon, 22 May 2017 18:02:01 +0000."
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn splits_result_out_of_a_finish_record() {
        let event = parse(
            "Aug 29 22:55:26 test1 [CLOUDINIT] handlers.py[DEBUG]: finish: \
             modules-final: SUCCESS: running modules for final",
        );
        assert_eq!(event["name"], "modules-final");
        assert_eq!(event["event_type"], "finish");
        assert_eq!(event["result"], "SUCCESS");
        assert_eq!(event["description"], "running modules for final");
    }

    #[test]
    fn reads_amazon_linux_lines_without_a_hostname() {
        let event = parse(
            "Apr 30 19:39:11 cloud-init[2673]: handlers.py[DEBUG]: start: \
             init-local/check-cache: attempting to read from cache [check]",
        );
        assert_eq!(event["name"], "init-local/check-cache");
        assert_eq!(event["event_type"], "start");
        assert_eq!(
            event["description"],
            "attempting to read from cache [check]"
        );
    }

    #[test]
    fn ignores_lines_without_a_separator() {
        assert!(parse_ci_logline("nothing to see here").unwrap().is_none());
    }

    #[test]
    fn rejects_lines_that_split_into_more_than_two_parts() {
        assert!(parse_ci_logline("a - b - c").is_err());
    }

    #[test]
    fn pads_sub_second_fractions_on_the_right() {
        assert_eq!(parse_fraction("839").unwrap(), 839_000);
        assert_eq!(parse_fraction("000001").unwrap(), 1);
        assert!(parse_fraction("1234567").is_err());
        assert!(parse_fraction("").is_err());
    }

    #[test]
    fn parses_iso8601_with_an_offset() {
        // 2016-08-30T21:53:25.972325+00:00
        let utc = parse_timestamp("2016-08-30T21:53:25.972325+00:00").unwrap();
        assert_eq!(utc, 1_472_594_005.972_325);
        let plus_one = parse_timestamp("2016-08-30T22:53:25.972325+01:00").unwrap();
        assert_eq!(plus_one, utc);
    }

    #[test]
    fn repeats_the_previous_event_for_an_unparsable_line() {
        // Upstream leaves `event` bound across loop iterations; the second line is
        // rejected by the two-way split and re-emits the first event.
        let raw = "Aug 29 22:55:26 h [CLOUDINIT] handlers.py[DEBUG]: start: \
                   init-local: searching\nfinish: - a - b\n";
        let events = dump_events(raw);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], events[1]);
    }
}
