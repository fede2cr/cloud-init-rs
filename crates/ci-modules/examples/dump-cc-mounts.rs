//! `cc_mounts`' decisions, for the differential harness. Paired with
//! `tests/differential/ccmounts.py`.
//!
//! Usage: `dump-cc-mounts <root> <cfg-json> <transformer-json> <systemd> [env]`
//!        `dump-cc-mounts --batch <cases-file>`
//!
//! `<root>` is a fixture tree holding `dev/...`, `sys/block/...` and
//! `etc/fstab`, so both sides can be asked about devices this machine does not
//! have. The Python side reaches the same tree by prefixing `os.path.exists`
//! and `os.path.realpath`.
//!
//! `<transformer-json>` stands in for `cloud.device_name_to_device`, which is
//! a datasource override no ported datasource supplies yet.
//!
//! `[env]` is the optional fifth field: the host facts the swap planner cannot
//! read through a rooted path (`fstype`, `kernel_version`, `memtotal`,
//! `available`). Supplying it extends the record past the four `mounts` passes
//! to the swap plan and the exact fstab bytes; the Python side injects the
//! same facts by stubbing `get_mount_info`, `kernel_version`, `read_meminfo`
//! and `os.statvfs`.
//!
//! Commands are compared as *planned* rather than as run: the port's `run`
//! half refuses to shell out unless the root is `/`, and the Python side
//! records the argv its stubs were handed instead of executing it.
//!
//! Batch mode takes one tab-separated argument list per line and emits a
//! `## <line>` marker before each record.

use std::path::Path;

use ci_config::{Object, Value};
use ci_modules::cc::mounts::{
    parse_fstab, plan_fstab, plan_mounts, plan_swapcfg, Fstab, Step, SwapEnv,
    SwapMethod,
};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-mounts: cannot read {:?}", arg(2));
            return std::process::ExitCode::from(2);
        };
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            println!("## {line}");
            println!("{}", ci_core::jsonfmt::dumps_indent(&one(&fields), 1));
        }
        return std::process::ExitCode::SUCCESS;
    }

    if argv.len() < 5 {
        eprintln!(
            "usage: dump-cc-mounts <root> <cfg-json> <transformer-json> <systemd> [env]"
        );
        return std::process::ExitCode::from(2);
    }
    let fields: Vec<&str> = (1..argv.len().min(6)).map(arg).collect();
    println!("{}", ci_core::jsonfmt::dumps_indent(&one(&fields), 1));
    std::process::ExitCode::SUCCESS
}

fn one(fields: &[&str]) -> Value {
    let at = |index: usize| fields.get(index).copied().unwrap_or("");
    let mut out = Object::new();

    let root = Path::new(at(0));
    let Ok(Value::Object(cfg)) = serde_json::from_str::<Value>(at(1)) else {
        out.insert(
            "error".to_owned(),
            Value::String("<cfg-json> must be an object".to_owned()),
        );
        return Value::Object(out);
    };
    let transformer_map = match serde_json::from_str::<Value>(at(2)) {
        Ok(Value::Object(map)) => map,
        _ => Object::new(),
    };
    let uses_systemd = at(3) == "1";

    let default_mount_options = if uses_systemd {
        "defaults,nofail,x-systemd.after=cloud-init-network.service,_netdev"
    } else {
        "defaults,nobootwait"
    };
    let hardcoded = vec![
        Value::Null,
        Value::Null,
        Value::String("auto".to_owned()),
        Value::String(default_mount_options.to_owned()),
        Value::String("0".to_owned()),
        Value::String("2".to_owned()),
    ];
    let default_fields = cfg
        .get("mount_default_fields")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or(hardcoded);
    let mounts = cfg
        .get("mounts")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let aliases = cfg
        .get("device_aliases")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let transformer = |name: &str| {
        transformer_map
            .get(name)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };

    let fstab = parse_fstab(root);
    // `%s` is `str()`, so a string config prints unquoted while a list prints
    // as its repr.
    let shown = mounts
        .as_str()
        .map_or_else(|| ci_config::repr(&mounts), ToOwned::to_owned);
    let mut steps = vec![Step::Debug(format!("mounts configuration is {shown}"))];

    let planned = plan_mounts(
        root,
        &mounts,
        &fstab.devs,
        &aliases,
        &default_fields,
        default_mount_options,
        &transformer,
        &mut steps,
    );
    let mut updated = match planned {
        Ok(updated) => updated,
        Err(error) => {
            out.insert("error".to_owned(), Value::String(error));
            out.insert("log".to_owned(), log(&steps));
            return Value::Object(out);
        }
    };

    let Some(env) = swap_env(at(4)) else {
        out.insert(
            "mounts".to_owned(),
            Value::Array(updated.into_iter().map(Value::Array).collect()),
        );
        out.insert("log".to_owned(), log(&steps));
        return Value::Object(out);
    };

    let swapcfg = cfg
        .get("swap")
        .cloned()
        .unwrap_or_else(|| Value::Object(Object::new()));
    swap_and_fstab(
        root,
        &swapcfg,
        &env,
        &mut updated,
        &fstab,
        uses_systemd,
        &mut steps,
        &mut out,
    );
    Value::Object(out)
}

/// The half of `handle` past the four passes: the swap file, then the fstab
/// rewrite and the commands that follow it.
#[expect(
    clippy::too_many_arguments,
    reason = "one argument per thing `handle` has already resolved"
)]
fn swap_and_fstab(
    root: &Path,
    swapcfg: &Value,
    env: &SwapEnv,
    updated: &mut Vec<Vec<Value>>,
    fstab: &Fstab,
    uses_systemd: bool,
    steps: &mut Vec<Step>,
    out: &mut Object,
) {
    let (swap_steps, swapfile) = plan_swapcfg(root, swapcfg, env);
    steps.extend(swap_steps);
    if let Some(path) = swapfile {
        let mut entry = vec![Value::String(path)];
        entry.extend(
            ["none", "swap", "sw", "0", "0"]
                .into_iter()
                .map(|token| Value::String(token.to_owned())),
        );
        updated.push(entry);
    }
    match plan_fstab(updated, fstab, uses_systemd) {
        Ok(tail) => steps.extend(tail),
        Err(error) => {
            out.insert("error".to_owned(), Value::String(error));
        }
    }
    out.insert("steps".to_owned(), commands(root, steps));
    let fstab_bytes = steps.iter().find_map(|step| match step {
        Step::WriteFstab(contents) => Some(contents.clone()),
        _ => None,
    });
    out.insert(
        "fstab".to_owned(),
        fstab_bytes.map_or(Value::Null, Value::String),
    );
    out.insert("log".to_owned(), log(steps));
}

/// The fifth field: host facts the swap planner cannot read through a rooted
/// path. An absent or unparsable field means the record stops at the four
/// `mounts` passes.
fn swap_env(text: &str) -> Option<SwapEnv> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
        return None;
    };
    let number = |key: &str| map.get(key).and_then(Value::as_u64);
    Some(SwapEnv {
        fstype: map
            .get("fstype")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        kernel_version: map
            .get("kernel_version")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|part| u32::try_from(part).unwrap_or(u32::MAX))
                    .collect()
            })
            .unwrap_or_default(),
        memtotal: number("memtotal"),
        available: number("available"),
    })
}

/// Everything the run half would carry out, as the argv it would run, so the
/// Python side's recorded stub calls line up token for token.
///
/// One label covers both `ensure_dir` steps: they differ only in what happens
/// when the call fails, which a recording stub never does. `mount_if_needed`
/// is rendered as its inputs rather than as `mount -a`, because whether it
/// fires depends on the live mount table, the one fact neither side injects.
fn commands(root: &Path, steps: &[Step]) -> Value {
    let mut out: Vec<String> = Vec::new();
    for step in steps {
        match step {
            Step::Debug(_) | Step::Info(_) | Step::Warning(_) => {}
            Step::EnsureDir(dir) | Step::EnsureConfigDir(dir) => {
                out.push(format!("ensure-dir {dir}"));
            }
            Step::BtrfsPrepare(path) => {
                out.push(format!("truncate -s 0 {path}"));
                out.push(format!("chattr +C {path}"));
            }
            Step::CreateSwap {
                path, mib, method, ..
            } => out.push(match method {
                SwapMethod::Fallocate => format!("fallocate -l {mib}M {path}"),
                SwapMethod::Dd => {
                    format!("dd if=/dev/zero of={path} bs=1M count={mib}")
                }
            }),
            // The run half only chmods a swap file that exists, and with the
            // creation command stubbed on both sides it never does.
            Step::ChmodSwap(path) => {
                if root.join(path.trim_start_matches('/')).exists() {
                    out.push(format!("chmod 600 {path}"));
                }
            }
            Step::Mkswap(path) => out.push(format!("mkswap {path}")),
            Step::WriteFstab(_) => out.push("write-fstab".to_owned()),
            Step::SwapOn => out.push("swapon -a".to_owned()),
            Step::MountAll {
                daemon_reload,
                changes_made,
                dirs,
            } => out.push(format!(
                "mount-if-needed reload={} changes={} dirs={}",
                u8::from(*daemon_reload),
                u8::from(*changes_made),
                dirs.join(",")
            )),
        }
    }
    Value::Array(out.into_iter().map(Value::String).collect())
}

/// The log lines in order, tagged with their level so a warning cannot pass
/// for a debug line.
fn log(steps: &[Step]) -> Value {
    Value::Array(
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Debug(message) => Some(format!("DEBUG {message}")),
                Step::Info(message) => Some(format!("INFO {message}")),
                Step::Warning(message) => Some(format!("WARNING {message}")),
                _ => None,
            })
            .map(Value::String)
            .collect(),
    )
}
