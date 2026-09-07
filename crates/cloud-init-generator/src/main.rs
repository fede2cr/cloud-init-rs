fn main() {
    let mut iter = std::env::args();
    let argv0 = iter
        .next()
        .unwrap_or_else(|| "cloud-init-generator".to_owned());
    let args: Vec<String> = iter.collect();
    std::process::exit(cloud_init_generator::run(
        &cloud_init_generator::Config::system(),
        &argv0,
        &args,
    ));
}
