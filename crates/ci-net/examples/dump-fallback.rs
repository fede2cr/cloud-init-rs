//! Dump fallback network config generation, for differential testing.
//!
//! Usage: `dump-fallback`
//!
//! Like `dump-renderers` this one has no fixture: every function here reads
//! `/sys/class/net` on the machine it runs on. That is also the comparison
//! that matters, because this config is what a boot with no datasource
//! actually renders — if the two implementations disagree about it, one of
//! them writes the wrong netplan.

use ci_config::{Object, Value};

fn strings(names: Vec<String>) -> Value {
    Value::Array(names.into_iter().map(Value::String).collect())
}

fn main() {
    let sys = ci_net::sysfs::Sys::real();

    let mut out = Object::new();
    out.insert("candidates".to_owned(), strings(sys.candidate_nics()));
    out.insert(
        "fallback_nic".to_owned(),
        sys.fallback_nic().map_or(Value::Null, Value::String),
    );
    out.insert(
        "config".to_owned(),
        sys.generate_fallback_config(false)
            .map_or(Value::Null, Value::Object),
    );
    out.insert(
        "config_driver".to_owned(),
        sys.generate_fallback_config(true)
            .map_or(Value::Null, Value::Object),
    );

    println!("{}", ci_core::jsonfmt::dumps_indent(&Value::Object(out), 1));
}
