//! Print what the `LXD` crawler makes of a `/dev/lxd/sock`-style API.

use ci_datasource::lxd;

fn main() {
    let Some(socket) = std::env::args().nth(1) else {
        eprintln!("usage: dump-lxd SOCKET");
        std::process::exit(2);
    };

    let mut logger = ci_log::Logger::silent();
    match lxd::read_metadata(
        std::path::Path::new(&socket),
        lxd::Keys::all(),
        &mut logger,
    ) {
        Ok(md) => println!("{}", ci_core::json_dumps(&ci_config::Value::Object(md))),
        Err(reason) => {
            eprintln!("{reason}");
            std::process::exit(1);
        }
    }
}
