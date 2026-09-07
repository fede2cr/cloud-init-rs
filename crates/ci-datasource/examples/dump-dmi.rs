//! Dump what this host's DMI reports, for the `dmi.py` differential.

fn main() {
    let mut logger = ci_log::Logger::silent();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--syspath") {
        let root = std::path::PathBuf::from(args.get(1).map_or("", String::as_str));
        for key in args.iter().skip(2) {
            let value = ci_datasource::dmi::read_syspath_at(&root, key, &mut logger);
            println!("syspath {key}={}", value.as_deref().unwrap_or("<none>"));
        }
        return;
    }
    println!(
        "container={}",
        if ci_core::container::is_container() {
            "True"
        } else {
            "False"
        }
    );
    for key in ci_datasource::dmi::keys() {
        let value = ci_datasource::dmi::read_dmi_data(key, &mut logger);
        println!("{key}={}", value.as_deref().unwrap_or("<none>"));
    }
    for src in args {
        println!(
            "sub {src} -> {}",
            ci_datasource::dmi::sub_dmi_vars(&src, &mut logger)
        );
    }
}
