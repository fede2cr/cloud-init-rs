//! `cc_apt_configure`'s decisions, for the differential harness. Paired with
//! `tests/differential/ccaptconfigure.py`.
//!
//! The module is far too large for one comparison, and most of it cannot be
//! run for real on the machine doing the comparing — it imports gpg keys,
//! rewrites `/etc/apt` and shells out to `add-apt-repository`. So the program
//! is split into subcommands, one per decision the module makes, each of them
//! driving the planning half with its inputs supplied as JSON:
//!
//! ```text
//! dump-cc-apt-configure convert <cfg-json>
//! dump-cc-apt-configure aptconf <cfg-json> <exists-json>
//! dump-cc-apt-configure sources <cfg-json> <env-json>
//! dump-cc-apt-configure entries <cfg-json> <env-json>
//! dump-cc-apt-configure mirrors <cfg-json> <env-json>
//! ```
//!
//! `util.rand_dict_key` is replaced on both sides by a counter, because the
//! real one is random and the key it picks is part of the converted config.
//!
//! The gpg half is a stub on both sides for the same reason `subp` is: asking
//! keyserver.ubuntu.com for a key during a test run makes the result depend on
//! the network. `<env-json>.gpg` says what the stub answers.

use ci_config::{Object, Value};
use ci_gpg::Gpg;
use ci_log::Logger;
use ci_modules::cc::apt_configure as apt;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let Some(what) = argv.get(1) else {
        usage();
    };
    let Value::Object(cfg) = parse(argv.get(2).map_or("{}", String::as_str)) else {
        fail("<cfg-json> must be an object")
    };
    let env = parse(argv.get(3).map_or("{}", String::as_str));
    let mut log = Logger::silent();

    let printed = match what.as_str() {
        "convert" => convert(cfg, &mut log),
        "aptconf" => aptconf(&cfg, &env, &mut log),
        "sources" => sources(&cfg, &env, &mut log),
        "entries" => entries(&cfg, &env, &mut log),
        "mirrors" => mirrors(&cfg, &env, &mut log),
        _ => usage(),
    };
    println!("{}", ci_core::jsonfmt::dumps_indent(&printed, 1));
}

fn usage() -> ! {
    eprintln!(
        "usage: dump-cc-apt-configure \
         convert|aptconf|sources|entries|mirrors <cfg-json> [<env-json>]"
    );
    std::process::exit(2);
}

fn fail(message: &str) -> ! {
    eprintln!("dump-cc-apt-configure: {message}");
    std::process::exit(2);
}

fn parse(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => value,
        Err(err) => fail(&err.to_string()),
    }
}

/// An error is an outcome, not a crash: upstream raises out of `handle` and
/// the harness compares the message.
fn raised(error: &str) -> Value {
    let mut out = Object::new();
    out.insert("error".to_owned(), Value::String(error.to_owned()));
    Value::Object(out)
}

fn string_of(env: &Value, key: &str, fallback: &str) -> String {
    env.get(key)
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned()
}

/// `util.rand_dict_key`, made repeatable: the real one is random, and the key
/// it returns ends up in the converted config.
fn counter() -> impl FnMut(&Object, &str) -> String {
    let mut next = 0u32;
    move |_, postfix| {
        next = next.saturating_add(1);
        format!("key{next}_{postfix}")
    }
}

fn convert(mut cfg: Object, log: &mut Logger) -> Value {
    match apt::convert_to_v3_apt_format(&mut cfg, log, counter()) {
        Ok(()) => Value::Object(cfg),
        Err(error) => raised(&error),
    }
}

fn aptconf(cfg: &Object, env: &Value, log: &mut Logger) -> Value {
    // `<env-json>` is the list of drop-in paths that already exist.
    let present: Vec<String> = env
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let actions = apt::plan_apt_config(
        cfg,
        apt::APT_PROXY_FN,
        apt::APT_CONFIG_FN,
        &mut |path| present.iter().any(|known| known == path),
        log,
    );
    Value::Array(actions.iter().map(file_action).collect())
}

fn sources(cfg: &Object, env: &Value, log: &mut Logger) -> Value {
    let release = string_of(env, "release", "noble");
    let distro = string_of(env, "distro", "ubuntu");
    let deb822 = env.get("deb822").and_then(Value::as_bool).unwrap_or(true);
    let empty = Object::new();
    let mirrors = env
        .get("mirrors")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let keys = env.get("keys").and_then(Value::as_object).unwrap_or(&empty);
    let templates = env
        .get("templates")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let files = env
        .get("files")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut load_template = |name: &str, _: &mut Logger| {
        templates
            .get(name)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    let mut read_file = |path: &str| {
        files
            .get(path)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    match apt::plan_sources_list(
        cfg,
        &release,
        mirrors,
        &distro,
        keys,
        deb822,
        &apt::AptPaths::default(),
        &mut load_template,
        &mut read_file,
        log,
    ) {
        Ok(actions) => Value::Array(actions.iter().map(file_action).collect()),
        Err(error) => raised(&error),
    }
}

fn entries(cfg: &Object, env: &Value, log: &mut Logger) -> Value {
    let mut gpg = StubGpg {
        keys: env
            .get("gpg")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
    };
    let mut params = env
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let pattern = match regex::Regex::new(&string_of(
        env,
        "aa_repo_match",
        apt::ADD_APT_REPO_MATCH,
    )) {
        Ok(pattern) => pattern,
        Err(error) => return raised(&error.to_string()),
    };
    let sources = cfg.get("sources").cloned().unwrap_or(Value::Null);
    match apt::plan_apt_sources(
        &sources,
        &mut gpg,
        &mut params,
        &mut |source| pattern.is_match(source),
        log,
    ) {
        Ok(steps) => Value::Array(steps.iter().map(step).collect()),
        // Upstream records the steps it took before the failure and then the
        // failure; the planner keeps no partial list, so the fixtures only
        // raise on the first entry.
        Err(error) => Value::Array(vec![raised(&error)]),
    }
}

fn mirrors(cfg: &Object, env: &Value, log: &mut Logger) -> Value {
    let arch = string_of(env, "arch", "amd64");
    let distro = string_of(env, "distro", "ubuntu");
    let fqdn = string_of(env, "fqdn", "host.example.com");
    // Which candidate URLs the resolver is to accept. `null` accepts none,
    // which is what an instance with no DNS at all sees.
    let resolvable: Vec<String> = env
        .get("resolvable")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    // Three call sites need this, and each wants its own `&mut`, so the
    // shared part is a plain immutable closure they all read.
    let find = |candidates: &[String]| {
        candidates
            .iter()
            .find(|candidate| resolvable.contains(candidate))
            .cloned()
    };
    let mut search = |candidates: &[String], _: &mut Logger| find(candidates);
    let mut search_dns = |configured: &Value, mirrortype: &str, _: &mut Logger| {
        if !ci_config::option::py_truthy(configured) {
            return None;
        }
        let candidates = apt::mirror_dns_candidates(mirrortype, &fqdn, &distro).ok()?;
        find(&candidates)
    };
    let package_mirrors = env
        .get("package_mirrors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut from_datasource = |log: &mut Logger| {
        let arch_info =
            ci_distro::mirrors::arch_package_mirror_info(&package_mirrors, &arch);
        ci_distro::mirrors::package_mirror_info(
            arch_info,
            env.get("availability_zone").and_then(Value::as_str),
            env.get("region").and_then(Value::as_str),
            &string_of(env, "platform_type", "azure"),
            &mut |candidates: &[String], _: &mut Logger| find(candidates),
            log,
        )
    };
    match apt::find_apt_mirror_info(
        cfg,
        &arch,
        &mut from_datasource,
        &mut search,
        &mut search_dns,
        log,
    ) {
        Ok(found) => Value::Object(found),
        Err(error) => raised(&error),
    }
}

fn file_action(action: &apt::FileAction) -> Value {
    let mut out = Object::new();
    match action {
        apt::FileAction::Write {
            path,
            content,
            mode,
        } => {
            out.insert("op".to_owned(), Value::String("write".to_owned()));
            out.insert("path".to_owned(), Value::String(path.clone()));
            out.insert("content".to_owned(), Value::String(content.clone()));
            // `None` means "whatever `util.write_file` defaults to", which is
            // 0o644; the Python side reports the effective mode either way.
            out.insert(
                "mode".to_owned(),
                Value::Number(u64::from(mode.unwrap_or(0o644)).into()),
            );
        }
        apt::FileAction::Remove(path) => {
            out.insert("op".to_owned(), Value::String("remove".to_owned()));
            out.insert("path".to_owned(), Value::String(path.clone()));
        }
    }
    Value::Object(out)
}

fn step(step: &apt::Step) -> Value {
    let mut out = Object::new();
    let mut set = |key: &str, value: Value| {
        out.insert(key.to_owned(), value);
    };
    match step {
        apt::Step::WriteKey { path, content } => {
            set("op", Value::String("write_key".to_owned()));
            set("path", Value::String(path.clone()));
            set(
                "content",
                Value::String(String::from_utf8_lossy(content).into_owned()),
            );
        }
        apt::Step::WriteSource {
            path,
            content,
            append,
        } => {
            set("op", Value::String("write_source".to_owned()));
            set("path", Value::String(path.clone()));
            set("content", Value::String(content.clone()));
            set("append", Value::Bool(*append));
        }
        apt::Step::AddAptRepository { source } => {
            set("op", Value::String("add_apt_repository".to_owned()));
            set("source", Value::String(source.clone()));
        }
        apt::Step::UpdatePackageSources => {
            set("op", Value::String("update_package_sources".to_owned()));
        }
    }
    Value::Object(out)
}

/// A `Gpg` that answers out of the fixture instead of out of the network.
///
/// `dearmor` is the identity with a marker wrapped round it, so the harness
/// can see *that* it happened without either side having to agree on what
/// real binary `OpenPGP` bytes look like.
#[derive(Debug)]
struct StubGpg {
    keys: Object,
}

impl Gpg for StubGpg {
    fn export_armour(&mut self, key: &str, _: &mut Logger) -> Option<String> {
        self.keys
            .get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }

    fn dearmor(&mut self, key: &str) -> Result<Vec<u8>, String> {
        if key.contains("BAD") {
            return Err("Failed to dearmor key".to_owned());
        }
        Ok(format!("<dearmored>{key}</dearmored>").into_bytes())
    }

    fn list_keys(
        &mut self,
        _: &str,
        _: bool,
        _: &mut Logger,
    ) -> Result<String, String> {
        Ok(String::new())
    }

    fn recv_key(&mut self, key: &str, _: &str, _: &mut Logger) -> Result<(), String> {
        if self.keys.contains_key(key) {
            return Ok(());
        }
        Err(format!("Failed to import key '{key}' from keyserver"))
    }

    fn delete_key(&mut self, _: &str, _: &mut Logger) {}
}
