//! Dump the klibc initramfs network config source, for differential testing.
//!
//! Usage: `dump-klibc <run-dir> <cmdline>`
//!
//! `<run-dir>` stands in for `/run`, which upstream hardcodes; the Python side
//! is handed the same file list explicitly. The mac addresses come from this
//! host's real `/sys/class/net` on both sides, because that is the lookup
//! upstream does and a fixture cannot stand in for it.

use ci_config::{Object, Value};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(run_dir), Some(cmdline)) = (args.next(), args.next()) else {
        eprintln!("usage: dump-klibc <run-dir> <cmdline>");
        std::process::exit(2);
    };

    let sys = ci_net::sysfs::Sys::real();
    let source = ci_net::cmdline::Klibc::new(&run_dir, &cmdline, &sys);

    let mut out = Object::new();
    out.insert(
        "files".to_owned(),
        Value::Array(
            ci_net::cmdline::Klibc::net_cfg_files(std::path::Path::new(&run_dir))
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| Value::String(name.to_string_lossy().into_owned()))
                .collect(),
        ),
    );
    out.insert(
        "is_applicable".to_owned(),
        Value::Bool(source.is_applicable()),
    );
    out.insert(
        "config".to_owned(),
        match source.render_config() {
            Ok(config) => Value::Object(config),
            Err(err) => Value::String(format!("ValueError: {err}")),
        },
    );

    println!("{}", ci_core::jsonfmt::dumps_indent(&Value::Object(out), 1));
}
