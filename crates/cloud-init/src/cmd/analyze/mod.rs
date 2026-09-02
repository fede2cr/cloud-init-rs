//! `cloud-init analyze` — read cloud-init logs and report per-boot timings.
//!
//! Port of `cloudinit/analyze/__init__.py`.

mod boot;
mod dump;
mod show;

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::Path;

use clap::{Args as ClapArgs, Subcommand};
use serde_json::Value;

const DEFAULT_INFILE: &str = "/var/log/cloud-init.log";
const DEFAULT_FORMAT: &str = "%I%D @%Es +%ds";

/// Ceiling on the log we will read into memory. Upstream slurps the file whole;
/// a bound keeps a rotated-but-enormous log from becoming a memory-exhaustion bug.
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: AnalyzeCommand,
}

#[derive(Debug, Subcommand)]
pub enum AnalyzeCommand {
    /// Print list of executed stages ordered by time to init.
    Blame(IoArgs),
    /// Print list of in-order events during execution.
    Show(ShowArgs),
    /// Dump cloud-init events in JSON format.
    Dump(IoArgs),
    /// Print list of boot times for kernel and cloud-init.
    Boot(IoArgs),
}

#[derive(Debug, ClapArgs)]
pub struct IoArgs {
    /// Specify where to read input.
    #[arg(long, short = 'i', default_value = DEFAULT_INFILE)]
    pub infile: String,

    /// Specify where to write output.
    #[arg(long, short = 'o', default_value = "-")]
    pub outfile: String,
}

#[derive(Debug, ClapArgs)]
pub struct ShowArgs {
    /// Specify formatting of output.
    #[arg(long = "format", short = 'f', default_value = DEFAULT_FORMAT)]
    pub print_format: String,

    #[command(flatten)]
    pub io: IoArgs,
}

pub fn run(args: &Args) -> u8 {
    let io = match &args.command {
        AnalyzeCommand::Blame(io)
        | AnalyzeCommand::Dump(io)
        | AnalyzeCommand::Boot(io) => io,
        AnalyzeCommand::Show(show) => &show.io,
    };

    let Some(rawdata) = read_input(&io.infile) else {
        return 1;
    };
    if rawdata.trim().is_empty() {
        eprintln!("Empty file {}", input_name(&io.infile));
        return 1;
    }
    let events = get_events(&rawdata);

    let (text, code) = match &args.command {
        AnalyzeCommand::Blame(_) => match analyze_blame(&events) {
            Ok(text) => (text, 0),
            Err(reason) => return fail(&reason),
        },
        AnalyzeCommand::Show(show) => match analyze_show(&events, &show.print_format) {
            Ok(text) => (text, 0),
            Err(reason) => return fail(&reason),
        },
        AnalyzeCommand::Dump(_) => {
            (ci_core::json_dumps(&Value::Array(events.clone())) + "\n", 0)
        }
        AnalyzeCommand::Boot(_) => {
            let (text, status_code) = boot::render(&events);
            // Upstream returns the status code from `main()`, and `sys.exit(str)`
            // prints it to stderr and exits 1.
            eprintln!("{status_code}");
            (text, 1)
        }
    };

    if !write_output(&io.outfile, &text) {
        return 1;
    }
    code
}

fn fail(reason: &str) -> u8 {
    eprintln!("{reason}");
    1
}

/// `_get_events()` — a JSON event list if the input is one, log lines otherwise.
fn get_events(rawdata: &str) -> Vec<Value> {
    if let Ok(Value::Array(events)) = serde_json::from_str::<Value>(rawdata) {
        if !events.is_empty() {
            return events;
        }
    }
    dump::dump_events(rawdata)
}

fn analyze_blame(events: &[Value]) -> Result<String, String> {
    let boot_records = show::generate_records(events, "     %ds (%n)")?;
    let mut out = String::new();
    for (idx, record) in boot_records.iter().enumerate() {
        let mut timed: Vec<&String> = record
            .iter()
            .filter(|line| leads_with_delta(line))
            .collect();
        timed.sort_by(|a, b| b.cmp(a));
        let _ = writeln!(out, "-- Boot Record {:02} --", idx.saturating_add(1));
        out.push_str(
            &timed
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        out.push_str("\n\n");
    }
    let _ = writeln!(out, "{} boot records analyzed", boot_records.len());
    Ok(out)
}

fn analyze_show(events: &[Value], print_format: &str) -> Result<String, String> {
    let boot_records = show::generate_records(events, print_format)?;
    let mut out = String::new();
    for (idx, record) in boot_records.iter().enumerate() {
        let _ = writeln!(out, "-- Boot Record {:02} --", idx.saturating_add(1));
        out.push_str(
            "The total time elapsed since completing an event is printed after the \
             \"@\" character.\n",
        );
        out.push_str(
            "The time the event takes is printed after the \"+\" character.\n\n",
        );
        out.push_str(&record.join("\n"));
        out.push('\n');
    }
    let _ = writeln!(out, "{} boot records analyzed", boot_records.len());
    Ok(out)
}

/// `re.compile(r"(^\s+\d+\.\d+)").match` — the filter `blame` uses to keep only
/// the formatted timing lines.
fn leads_with_delta(record: &str) -> bool {
    let mut chars = record.chars().peekable();
    if !chars.peek().is_some_and(|c| c.is_whitespace()) {
        return false;
    }
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
    if !chars.peek().is_some_and(char::is_ascii_digit) {
        return false;
    }
    while chars.peek().is_some_and(char::is_ascii_digit) {
        chars.next();
    }
    chars.next() == Some('.') && chars.next().is_some_and(|c| c.is_ascii_digit())
}

fn input_name(infile: &str) -> &str {
    if infile == "-" {
        "<stdin>"
    } else {
        infile
    }
}

fn read_input(infile: &str) -> Option<String> {
    if infile == "-" {
        let mut buf = String::new();
        return std::io::stdin()
            .take(MAX_LOG_BYTES)
            .read_to_string(&mut buf)
            .ok()
            .map(|_| buf);
    }
    let Ok(text) = ci_sys::path::read_text_capped(Path::new(infile), MAX_LOG_BYTES)
    else {
        eprintln!("Cannot open file {infile}");
        return None;
    };
    Some(text)
}

fn write_output(outfile: &str, text: &str) -> bool {
    if outfile == "-" {
        print!("{text}");
        return std::io::stdout().flush().is_ok();
    }
    if ci_sys::atomic::write_file(outfile, text, ci_sys::atomic::WriteOptions::PUBLIC)
        .is_err()
    {
        eprintln!("Cannot open file {outfile}");
        return false;
    }
    true
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

    fn sample_events() -> Vec<Value> {
        vec![
            json!({"name": "init-local", "event_type": "start", "timestamp": 10.0,
                   "description": "starting search for local datasources",
                   "origin": "cloudinit"}),
            json!({"name": "init-local/check-cache", "event_type": "start",
                   "timestamp": 10.5, "description": "attempting to read from cache",
                   "origin": "cloudinit"}),
            json!({"name": "init-local/check-cache", "event_type": "finish",
                   "timestamp": 10.75, "description": "no cache found",
                   "result": "SUCCESS", "origin": "cloudinit"}),
            json!({"name": "init-local", "event_type": "finish", "timestamp": 11.0,
                   "description": "searching for local datasources",
                   "result": "SUCCESS", "origin": "cloudinit"}),
        ]
    }

    #[test]
    fn blame_keeps_only_timed_lines_sorted_descending() {
        let out = analyze_blame(&sample_events()).unwrap();
        assert_eq!(
            out,
            "-- Boot Record 01 --\n     00.25000s (init-local/check-cache)\n\n\
             1 boot records analyzed\n"
        );
    }

    #[test]
    fn show_uses_the_default_format() {
        let out = analyze_show(&sample_events(), DEFAULT_FORMAT).unwrap();
        assert!(out.starts_with("-- Boot Record 01 --\n"));
        assert!(out.contains("|`->no cache found @00.50000s +00.25000s"));
        assert!(out.ends_with("1 boot records analyzed\n"));
    }

    #[test]
    fn json_input_is_used_verbatim() {
        let raw = serde_json::to_string(&sample_events()).unwrap();
        assert_eq!(get_events(&raw), sample_events());
    }

    #[test]
    fn an_empty_json_list_falls_back_to_log_parsing() {
        assert!(get_events("[]").is_empty());
    }

    #[test]
    fn delta_filter_matches_the_upstream_regex() {
        assert!(leads_with_delta("     00.10520s (init-local)"));
        assert!(!leads_with_delta("Starting stage: init-local"));
        assert!(!leads_with_delta("Total Time: 1.00000 seconds\n"));
        assert!(!leads_with_delta("00.10520s (init-local)"));
    }
}
