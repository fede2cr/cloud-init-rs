//! `ds-identify` entry point.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let di_main = std::env::var("DI_MAIN").unwrap_or_else(|_| "main".to_owned());
    let code = match di_main.as_str() {
        "main" => ds_identify::run(&args),
        "print_info" => ds_identify::run_print_info(),
        "noop" => 0,
        // Upstream `exec`s whatever DI_MAIN names, as root, from a systemd
        // generator. Anything able to set one environment variable in that
        // context would get arbitrary code execution, so the port refuses.
        other => {
            eprintln!(
                "ERROR: refusing to side-load alternate implementation: [{other}]"
            );
            3
        }
    };
    std::process::exit(code);
}
