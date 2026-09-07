//! Port of `cloudinit/analyze/show.py` — pairing events into per-boot records.

use serde_json::{Map, Value};

use super::dump::Event;

/// `%`-codes accepted by `analyze show --format`, in upstream substitution order.
const FORMAT_KEYS: [(&str, &str); 11] = [
    ("%d", "delta"),
    ("%D", "description"),
    ("%E", "elapsed"),
    ("%e", "event_type"),
    ("%I", "indent"),
    ("%l", "level"),
    ("%n", "name"),
    ("%o", "origin"),
    ("%r", "result"),
    ("%t", "timestamp"),
    ("%T", "total_time"),
];

/// Keys upstream pins to `{:08.5f}` so that the lexical sort in `blame` is numeric.
const FLOAT_KEYS: [&str; 3] = ["delta", "elapsed", "timestamp"];

pub fn event_name(event: &Value) -> Option<&str> {
    event.get("name")?.as_str()
}

pub fn event_type(event: &Value) -> Option<&str> {
    event.get("event_type")?.as_str()
}

/// Timestamp snapped to microseconds, matching `datetime.fromtimestamp()`.
pub fn event_micros(event: &Value) -> Option<i64> {
    let raw = event.get("timestamp")?;
    let seconds = match raw {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.parse().ok()?,
        _ => return None,
    };
    Some(ci_core::time::round_half_even_micros(seconds))
}

/// `show.format_record()` — expand `%`-codes, then apply Python's `str.format`.
pub fn format_record(msg: &str, event: &Event) -> Result<String, String> {
    let mut template = msg.to_owned();
    for (code, key) in FORMAT_KEYS {
        if !template.contains(code) {
            continue;
        }
        let field = if FLOAT_KEYS.contains(&key) {
            format!("{{{key}:08.5f}}")
        } else {
            format!("{{{key}}}")
        };
        template = template.replace(code, &field);
    }
    python_format(&template, event)
}

/// The subset of `str.format` upstream can reach: `{key}`, `{key:08.5f}` and
/// escaped braces. Anything else is an error rather than a silent mis-render.
fn python_format(template: &str, event: &Event) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '{' => {
                let mut field = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    field.push(c);
                }
                if !closed {
                    return Err("Single '{' encountered in format string".to_owned());
                }
                let (key, spec) = field
                    .split_once(':')
                    .map_or((field.as_str(), ""), |(k, s)| (k, s));
                let value =
                    event.get(key).ok_or_else(|| format!("KeyError: '{key}'"))?;
                out.push_str(&render_field(value, spec)?);
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                out.push('}');
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn render_field(value: &Value, spec: &str) -> Result<String, String> {
    if spec == "08.5f" {
        let seconds = value
            .as_f64()
            .ok_or_else(|| format!("Unknown format code 'f' for {value}"))?;
        return Ok(format!("{seconds:08.5}"));
    }
    if !spec.is_empty() {
        return Err(format!("Invalid format specifier '{spec}'"));
    }
    Ok(match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        other => other.to_string(),
    })
}

fn total_time_record(total_time: f64) -> String {
    format!("Total Time: {total_time:3.5} seconds\n")
}

/// `show.event_record()` — the finish event, annotated with its own timing.
fn event_record(start_time: i64, start: &Value, finish: &Value) -> Option<Event> {
    let mut record = finish.as_object()?.clone();
    let indent = match event_name(start) {
        // A top-level stage has no `/`, and `" " * -1` is empty in Python.
        Some(name) => {
            let depth = name.matches('/').count();
            format!("|{}`->", " ".repeat(depth.saturating_sub(1)))
        }
        None => "|".to_owned(),
    };
    let start_us = event_micros(start)?;
    let finish_us = event_micros(finish)?;
    record.insert("delta".to_owned(), seconds(finish_us - start_us));
    record.insert("elapsed".to_owned(), seconds(start_us - start_time));
    record.insert("indent".to_owned(), Value::String(indent));
    Some(record)
}

#[allow(clippy::cast_precision_loss)]
fn seconds(micros: i64) -> Value {
    let value = micros as f64 / 1e6;
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn delta_of(record: &Map<String, Value>) -> f64 {
    record.get("delta").and_then(Value::as_f64).unwrap_or(0.0)
}

/// `show.generate_records()` — split the event stream into per-boot record lists.
///
/// Upstream computes a sorted copy of the events and then indexes the *unsorted*
/// list anyway; the ordering it actually relies on is the log order, so that is
/// what this reproduces. See docs/COMPAT.md B3.
pub fn generate_records(
    events: &[Value],
    print_format: &str,
) -> Result<Vec<Vec<String>>, String> {
    let mut records: Vec<String> = Vec::new();
    let mut boot_records: Vec<Vec<String>> = Vec::new();
    let mut start_time: Option<i64> = None;
    let mut total_time = 0.0_f64;
    let mut unprocessed: Vec<&Value> = Vec::new();

    for (idx, event) in events.iter().enumerate() {
        let next_evt = events.get(idx.saturating_add(1));

        if event_type(event) == Some("start") {
            if !records.is_empty() && event_name(event) == Some("init-local") {
                records.push(total_time_record(total_time));
                boot_records.push(std::mem::take(&mut records));
                start_time = None;
                total_time = 0.0;
            }
            if start_time.is_none() {
                start_time = event_micros(event);
            }

            if event_name(event) == next_evt.and_then(event_name) {
                if next_evt.and_then(event_type) == Some("finish") {
                    if let (Some(begin), Some(finish)) = (start_time, next_evt) {
                        if let Some(record) = event_record(begin, event, finish) {
                            records.push(format_record(print_format, &record)?);
                        }
                    }
                }
            } else {
                let name = event
                    .get("name")
                    .map_or_else(|| "None".to_owned(), render_name);
                records.push(format!("Starting stage: {name}"));
                unprocessed.push(event);
            }
        } else {
            // Upstream raises IndexError on an unmatched finish, which crashes
            // `analyze show` for any childless stage (docs/COMPAT.md B1).
            let Some(prev) = unprocessed.pop() else {
                continue;
            };
            if event_name(event) == event_name(prev) {
                if let Some(begin) = start_time {
                    if let Some(record) = event_record(begin, prev, event) {
                        records.push(
                            format_record("Finished stage: (%n) %d seconds", &record)?
                                + "\n",
                        );
                        total_time += delta_of(&record);
                    }
                }
            } else {
                unprocessed.push(prev);
            }
        }
    }

    records.push(total_time_record(total_time));
    boot_records.push(records);
    Ok(boot_records)
}

fn render_name(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn start(name: &str, ts: f64) -> Value {
        json!({"name": name, "event_type": "start", "timestamp": ts,
               "description": "d", "origin": "cloudinit"})
    }

    fn finish(name: &str, ts: f64) -> Value {
        json!({"name": name, "event_type": "finish", "timestamp": ts,
               "description": "d", "origin": "cloudinit", "result": "SUCCESS"})
    }

    #[test]
    fn zero_pads_time_fields_to_width_eight() {
        let record: Event = json!({"delta": 0.105_195, "name": "init-local"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            format_record("     %ds (%n)", &record).unwrap(),
            "     00.10519s (init-local)"
        );
    }

    #[test]
    fn reports_missing_keys_instead_of_rendering_them_blank() {
        let record: Event = json!({"name": "x"}).as_object().unwrap().clone();
        assert_eq!(
            format_record("%l", &record).unwrap_err(),
            "KeyError: 'level'"
        );
    }

    #[test]
    fn indents_by_path_depth() {
        let record =
            event_record(0, &start("init-local/search", 1.0), &finish("x", 2.0))
                .unwrap();
        assert_eq!(record["indent"], "|`->");
        let record = event_record(0, &start("a/b/c", 1.0), &finish("x", 2.0)).unwrap();
        assert_eq!(record["indent"], "| `->");
    }

    #[test]
    fn pairs_adjacent_start_and_finish_events() {
        let events = vec![
            start("init-local", 10.0),
            start("init-local/check", 10.5),
            finish("init-local/check", 10.75),
            finish("init-local", 11.0),
        ];
        let records = generate_records(&events, "%I%D @%Es +%ds").unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0][0], "Starting stage: init-local");
        assert_eq!(records[0][1], "|`->d @00.50000s +00.25000s");
        assert_eq!(
            records[0][2],
            "Finished stage: (init-local) 01.00000 seconds\n"
        );
        assert_eq!(records[0][3], "Total Time: 1.00000 seconds\n");
    }

    #[test]
    fn starts_a_new_boot_record_at_each_init_local() {
        let mut events = Vec::new();
        for base in [10.0, 20.0] {
            events.push(start("init-local", base));
            events.push(start("init-local/check", base + 0.5));
            events.push(finish("init-local/check", base + 0.75));
            events.push(finish("init-local", base + 1.0));
        }
        let records = generate_records(&events, "%n").unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].last().unwrap(), "Total Time: 1.00000 seconds\n");
        assert_eq!(records[1].last().unwrap(), "Total Time: 1.00000 seconds\n");
    }

    #[test]
    fn tolerates_a_finish_without_a_matching_start() {
        let events = vec![finish("modules-final", 11.0)];
        let records = generate_records(&events, "%n").unwrap();
        assert_eq!(records, vec![vec!["Total Time: 0.00000 seconds\n"]]);
    }
}
