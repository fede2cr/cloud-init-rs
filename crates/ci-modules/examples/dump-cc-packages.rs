//! `cc_package_update_upgrade_install.handle`'s decisions, for the differential
//! harness. Paired with `tests/differential/ccpackages.py`.
//!
//! Only the plan is printed. Carrying it out installs packages on the
//! developer's own machine and may reboot it, so the Python side replaces
//! `subp.subp` with a recorder and the ordered list of commands is the
//! comparison.
//!
//! `dump-cc-packages <cfg-json> <system-info-json> <state-json>`
//!
//! `<state-json>` is what the module reads off the running system:
//!
//! * `apt` / `snap` — whether `subp.which` finds each manager.
//! * `all_packages` — `apt-cache pkgnames`, or `null` to skip the check.
//! * `snap_hold` — `refresh.hold` out of `snap get system -d`.
//! * `reboot_marker` — which of `REBOOT_FILES` exists, or `null` for neither.
//!
//! Package order is not compared: upstream builds the argv out of a `set`, so
//! it differs on every run (bug B72, deviation 130). Both sides sort the
//! operands of an `install` or `dist-upgrade`, and — because `snap install` is
//! one command per package — each run of adjacent snap installs.

use std::collections::HashSet;

use ci_config::{Object, Value};
use ci_distro::packages::{self, Step};
use ci_log::Logger;
use ci_modules::cc::package_update_upgrade_install::{plan, Plan, State};

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let (Some(cfg), Some(system_info), Some(state)) =
        (argv.get(1), argv.get(2), argv.get(3))
    else {
        eprintln!("usage: dump-cc-packages <cfg-json> <system-info-json> <state-json>");
        std::process::exit(2);
    };
    let parse = |text: &str| match serde_json::from_str::<Value>(text) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("dump-cc-packages: {err}");
            std::process::exit(2);
        }
    };
    let Value::Object(cfg) = parse(cfg) else {
        eprintln!("dump-cc-packages: <cfg-json> must be an object");
        std::process::exit(2);
    };
    let system_info = parse(system_info);
    let name = system_info
        .get("distro")
        .and_then(Value::as_str)
        .unwrap_or("ubuntu");
    let Some(distro) = ci_distro::fetch(name) else {
        eprintln!("dump-cc-packages: unknown distro {name}");
        std::process::exit(2);
    };
    let distro_cfg = match system_info.get("distro_cfg") {
        Some(Value::Object(object)) => object.clone(),
        _ => Object::new(),
    };
    let state = state_of(&parse(state));

    // `get_apt_wrapper` raises out of `Distro.__init__`, not out of the
    // module, so it is not one of the exceptions `handle` catches: cloud-init
    // cannot build its distro object and nothing runs at all. It is reported
    // as its own kind of outcome, with a non-zero exit, to keep it apart.
    if let Err(error) =
        ci_distro::packages::AptConfig::from_config(&distro_cfg, &mut |name| {
            ci_sys::subp::which(name).is_some()
        })
    {
        let mut out = Object::new();
        out.insert("error".to_owned(), Value::String(error));
        println!("{}", ci_core::jsonfmt::dumps_indent(&Value::Object(out), 1));
        std::process::exit(1);
    }

    let mut log = Logger::silent();
    let printed =
        match plan(&cfg, &distro_cfg, distro.package_managers, &state, &mut log) {
            Ok(plan) => {
                let mut calls = calls(&plan);
                sort_snap_installs(&mut calls);
                Value::Array(calls)
            }
            Err(error) => {
                let mut out = Object::new();
                out.insert("op".to_owned(), Value::String("raise".to_owned()));
                out.insert("error".to_owned(), Value::String(error));
                Value::Array(vec![Value::Object(out)])
            }
        };
    println!("{}", ci_core::jsonfmt::dumps_indent(&printed, 1));
}

fn state_of(value: &Value) -> State {
    let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
    let all_packages = match value.get("all_packages") {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<HashSet<String>>(),
        ),
        _ => None,
    };
    State {
        packages: packages::State {
            apt_available: flag("apt"),
            snap_available: flag("snap"),
            all_packages,
            snap_refresh_hold: value
                .get("snap_hold")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        },
        reboot_marker: value
            .get("reboot_marker")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }
}

/// `snap install` is one command per package, so B72's set order shows up as
/// the order of the commands rather than of the words inside one. Each run of
/// adjacent snap installs is sorted; nothing else moves.
fn sort_snap_installs(calls: &mut [Value]) {
    let is_snap_install = |call: &Value| {
        call.get("argv")
            .and_then(Value::as_array)
            .is_some_and(|argv| {
                argv.iter()
                    .take(2)
                    .filter_map(Value::as_str)
                    .eq(["snap", "install"])
            })
    };
    let mut start = 0;
    while start < calls.len() {
        if !calls.get(start).is_some_and(is_snap_install) {
            start = start.saturating_add(1);
            continue;
        }
        let mut end = start;
        while calls.get(end).is_some_and(is_snap_install) {
            end = end.saturating_add(1);
        }
        if let Some(run) = calls.get_mut(start..end) {
            run.sort_by_key(|call| {
                call.get("argv")
                    .and_then(Value::as_array)
                    .map(|argv| {
                        argv.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default()
            });
        }
        start = end;
    }
}

/// The whole plan flattened into the order `handle` would run it in, which is
/// the order the Python recorder sees.
///
/// Upstream collects the failure of each of the three phases and re-raises
/// only the last one, after the reboot block — so at most one `raise` is
/// printed, and it comes last even though the phase that produced it did not.
fn calls(plan: &Plan) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut last_error: Option<String> = None;
    for step in &plan.update {
        out.push(call(step));
    }
    if plan.upgrade_unsupported {
        last_error =
            Some("Unable to install packages. Debian family distros only.".to_owned());
    }
    for step in &plan.upgrade {
        out.push(call(step));
    }
    for step in &plan.install.steps {
        out.push(call(step));
    }
    if let Some(failed) = &plan.install.failed {
        last_error = Some(failed.clone());
    }
    if plan.reboot.is_some() {
        let mut reboot = Object::new();
        reboot.insert("op".to_owned(), Value::String("reboot".to_owned()));
        out.push(Value::Object(reboot));
    }
    if let Some(error) = &last_error {
        out.push(raise(error));
    }
    out
}

fn raise(error: &str) -> Value {
    let mut out = Object::new();
    out.insert("op".to_owned(), Value::String("raise".to_owned()));
    out.insert("error".to_owned(), Value::String(error.to_owned()));
    Value::Object(out)
}

fn call(step: &Step) -> Value {
    let mut out = Object::new();
    out.insert("op".to_owned(), Value::String("subp".to_owned()));
    let mut argv: Vec<String> = step.argv.clone();
    if let Some(index) = argv.iter().position(|word| {
        matches!(word.as_str(), "install" | "dist-upgrade" | "upgrade")
    }) {
        argv.get_mut(index.saturating_add(1)..)
            .unwrap_or_default()
            .sort_unstable();
    }
    out.insert(
        "argv".to_owned(),
        Value::Array(argv.into_iter().map(Value::String).collect()),
    );
    let mut env = Object::new();
    for (key, value) in &step.env {
        env.insert(key.clone(), Value::String(value.clone()));
    }
    out.insert("env".to_owned(), Value::Object(env));
    out.insert("capture".to_owned(), Value::Bool(step.capture));
    out.insert(
        "semaphore".to_owned(),
        match &step.semaphore {
            None => Value::Null,
            Some((name, freq)) => Value::Array(vec![
                Value::String(name.clone()),
                Value::String(freq.as_str().to_owned()),
            ]),
        },
    );
    Value::Object(out)
}
