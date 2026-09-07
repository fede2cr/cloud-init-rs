//! Dumps the merged system config as JSON, for differential testing.
//!
//! `--file` arguments are the stage driver's repeatable `--file`.

fn main() {
    let files: Vec<std::path::PathBuf> =
        std::env::args_os().skip(1).map(Into::into).collect();
    let cfg = ci_config::merger::read_cfg(&files, ci_config::Limits::default());
    println!(
        "{}",
        ci_core::jsonfmt::json_dumps(&serde_json::Value::Object(cfg))
    );
}
