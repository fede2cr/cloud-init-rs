//! Port of `cloudinit/config/modules.py`: reading a module section out of the
//! merged config, canonicalising the names, resolving them against the module
//! set, and deciding which ones apply to this instance.
//!
//! What this crate does *not* do is decide when to run anything. Upstream's
//! `Modules._run_modules` is three concerns welded together — the per-module
//! `ReportEventStack`, the `Runners.run` semaphore, and the call into
//! `mod.handle`. The stage driver owns the first two; this crate answers the
//! question that is pure — given the config, which modules, in what order, at
//! what frequency, with what arguments? — and, in [`cc`], supplies the bodies
//! for the modules that have been ported.
//!
//! The engine is worth porting carefully because it is where a typo in
//! `/etc/cloud/cloud.cfg` — or, more interestingly, in tenant user-data, which
//! can replace `cloud_config_modules` wholesale — decides whether an instance
//! gets configured at all.

pub mod cc;
pub mod registry;

use ci_config::{repr, repr_str, type_name, Object, Value};
use ci_core::semaphore::Frequency;
use ci_log::Logger;

pub use cc::{handler, Args, Datasource, Handler};
pub use registry::{find, Module, MODULES};

/// Log source for this port, matching `logging.getLogger(__name__)` upstream.
const SOURCE: &str = "modules.py";

/// `modules.MOD_PREFIX`.
const MOD_PREFIX: &str = "cc_";

/// `modules.REMOVED_MODULES`. Named so that dropping them is quiet advice
/// rather than a warning; note the stray `.py` on the second entry, which is
/// upstream's and means that name can never actually match.
const REMOVED_MODULES: &[&str] = &[
    "cc_emit_upstart",
    "cc_refresh_rmc_and_interface.py",
    "cc_migrator",
    "cc_rightscale_userdata",
];

/// `modules.RENAMED_MODULES`.
const RENAMED_MODULES: &[(&str, &str)] = &[("cc_ubuntu_advantage", "cc_ubuntu_pro")];

/// `distros.ALL_DISTROS`.
const ALL_DISTROS: &str = "all";

/// One entry of a module section, as written, before the name is resolved.
///
/// Upstream's `_read_modules` returns dicts with `mod`, and optionally `freq`
/// and `args`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raw {
    /// `mod`: the name as configured, stripped but not canonicalised.
    pub name: String,
    /// `freq`: absent means "use the module's own default".
    pub frequency: Option<String>,
    /// `args`: absent when the entry did not carry any. Upstream does not
    /// require a list here, so neither does this.
    pub args: Option<Value>,
}

/// A resolved module: upstream's `ModuleDetails`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Details {
    /// The registry entry the name resolved to.
    pub module: &'static Module,
    /// The name *as configured*. Upstream keeps the raw, pre-canonicalisation,
    /// pre-rename spelling here, and it is what the semaphore is named after,
    /// so `- [bootcmd]` and `- [cc_bootcmd]` are two different semaphores.
    pub name: String,
    /// The resolved frequency: the configured one, or the module's default.
    pub frequency: Frequency,
    /// The arguments to hand the module.
    pub args: Value,
}

/// `modules.form_module_name`.
///
/// Note what it does not do: it does not lowercase. `Bootcmd` becomes
/// `cc_Bootcmd`, which then fails to resolve and is reported as a missing
/// module.
#[must_use]
pub fn form_module_name(name: &str) -> Option<String> {
    let canon = name.replace('-', "_");
    let canon = match canon.len().checked_sub(3) {
        Some(cut)
            if canon
                .get(cut..)
                .is_some_and(|end| end.eq_ignore_ascii_case(".py")) =>
        {
            canon
                .get(..cut)
                .map_or_else(|| canon.clone(), std::borrow::ToOwned::to_owned)
        }
        _ => canon,
    };
    let canon = canon.trim();
    if canon.is_empty() {
        return None;
    }
    if canon.starts_with(MOD_PREFIX) {
        Some(canon.to_owned())
    } else {
        Some(format!("{MOD_PREFIX}{canon}"))
    }
}

/// Python truthiness, which is what upstream's two `if not ...` guards test.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `for item in cfg_mods`, with Python's idea of what is iterable.
///
/// A string iterates per character and a mapping iterates its keys. Both are
/// almost certainly a mistake by whoever wrote the config, but both are what
/// upstream does, and the resulting run — seven one-letter module names, each
/// warned about and dropped — is reproduced rather than second-guessed.
fn iterate(value: &Value) -> Option<Vec<Value>> {
    match value {
        Value::Array(items) => Some(items.clone()),
        Value::String(text) => {
            Some(text.chars().map(|c| Value::String(c.to_string())).collect())
        }
        Value::Object(map) => {
            Some(map.keys().map(|k| Value::String(k.clone())).collect())
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

/// The `.strip()` upstream applies to a name or a frequency, which raises
/// `AttributeError` on anything that is not a string.
fn strip(value: &Value) -> Option<String> {
    value.as_str().map(|text| text.trim().to_owned())
}

/// `Modules._read_modules`.
///
/// Diverges from upstream in one respect: entries upstream raises on are
/// warned about and dropped. See `docs/COMPAT.md`.
#[must_use]
pub fn read_modules(cfg: &Object, section: &str, logger: &mut Logger) -> Vec<Raw> {
    let Some(value) = cfg.get(section) else {
        return Vec::new();
    };
    if !truthy(value) {
        return Vec::new();
    }
    let Some(items) = iterate(value) else {
        logger.warning(
            SOURCE,
            &format!(
                "Config option '{section}' is a {} and cannot be read as a \
                 module list; no modules will run from it.",
                type_name(value)
            ),
        );
        return Vec::new();
    };

    let mut list = Vec::new();
    for item in items {
        if !truthy(&item) {
            continue;
        }
        match &item {
            Value::String(text) => list.push(Raw {
                name: text.trim().to_owned(),
                frequency: None,
                args: None,
            }),
            Value::Array(parts) => {
                // Meant to fall through, as upstream puts it: a one-element
                // list is a bare name, two adds a frequency, the rest is args.
                let Some(name) = parts.first().and_then(strip) else {
                    warn_not_a_string(logger, &item, "module name");
                    continue;
                };
                let frequency = match parts.get(1) {
                    None => None,
                    Some(value) => {
                        let Some(freq) = strip(value) else {
                            warn_not_a_string(logger, &item, "frequency");
                            continue;
                        };
                        Some(freq)
                    }
                };
                let args = parts
                    .get(2..)
                    .filter(|rest| !rest.is_empty())
                    .map(|rest| Value::Array(rest.to_vec()));
                list.push(Raw {
                    name,
                    frequency,
                    args,
                });
            }
            Value::Object(map) => {
                // No `name` and the entry is silently dropped, even if it
                // carries a frequency and args.
                let Some(name) = map.get("name") else {
                    continue;
                };
                let Some(name) = strip(name) else {
                    warn_not_a_string(logger, &item, "module name");
                    continue;
                };
                let frequency = match map.get("frequency") {
                    None => None,
                    Some(value) => {
                        let Some(freq) = strip(value) else {
                            warn_not_a_string(logger, &item, "frequency");
                            continue;
                        };
                        Some(freq)
                    }
                };
                list.push(Raw {
                    name,
                    frequency,
                    // `item["args"] or []`: a falsy value is stored as an
                    // empty list, but the key itself is still set.
                    args: map.get("args").map(|args| {
                        if truthy(args) {
                            args.clone()
                        } else {
                            Value::Array(Vec::new())
                        }
                    }),
                });
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {
                logger.warning(
                    SOURCE,
                    &format!(
                        "Failed to read '{}' item in config, unknown type {} \
                         (entry ignored).",
                        display(&item),
                        type_name(&item)
                    ),
                );
            }
        }
    }
    list
}

/// `str()` rather than `repr()`, which is what upstream's `%s` interpolates.
fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => repr(other),
    }
}

fn warn_not_a_string(logger: &mut Logger, item: &Value, field: &str) {
    logger.warning(
        SOURCE,
        &format!(
            "Config specified module {} has a {field} that is not a string; \
             entry ignored.",
            repr(item)
        ),
    );
}

/// `Modules._fixup_modules`: canonicalise, resolve, and fill in the default
/// frequency. Entries that do not resolve are dropped, with a log line.
#[must_use]
pub fn fixup(raw: &[Raw], logger: &mut Logger) -> Vec<Details> {
    let mut resolved = Vec::new();
    for entry in raw {
        let Some(mut name) = form_module_name(&entry.name) else {
            continue;
        };
        let mut frequency = entry.frequency.as_deref().filter(|f| !f.is_empty());
        if let Some(text) = frequency {
            if Frequency::parse(text).is_none() {
                logger.log(
                    ci_log::Level::Deprecated,
                    "lifecycle.py",
                    &format!(
                        "Config specified module {} has an unknown frequency \
                         {text} is deprecated in 22.1 and scheduled to be \
                         removed in 27.1.",
                        entry.name
                    ),
                );
                // Misconfigured in /etc/cloud/cloud.cfg: fall back to the
                // module's own default rather than refusing to run it.
                frequency = None;
            }
        }
        if let Some((_, renamed)) =
            RENAMED_MODULES.iter().find(|(from, _)| *from == name)
        {
            logger.log(
                ci_log::Level::Deprecated,
                "lifecycle.py",
                &format!(
                    "Module has been renamed from {name} to {renamed}. Update \
                     any references in /etc/cloud/cloud.cfg is deprecated in \
                     24.1 and scheduled to be removed in 29.1."
                ),
            );
            name = (*renamed).to_owned();
        }
        let Some(module) = find(&name) else {
            if REMOVED_MODULES.contains(&name.as_str()) {
                logger.info(
                    SOURCE,
                    &format!(
                        "Module `{}` has been removed from cloud-init. It may \
                         be removed from `/etc/cloud/cloud.cfg`.",
                        name.get(MOD_PREFIX.len()..).unwrap_or(&name)
                    ),
                );
            } else {
                logger.warning(
                    SOURCE,
                    &format!(
                        "Could not find module named {name} (searched \
                         ['{name}', 'cloudinit.config.{name}'])"
                    ),
                );
            }
            continue;
        };
        resolved.push(Details {
            module,
            // Upstream keeps the raw name here, not the canonical one.
            name: entry.name.clone(),
            frequency: frequency
                .and_then(Frequency::parse)
                .unwrap_or(module.frequency),
            args: match &entry.args {
                Some(args) if truthy(args) => args.clone(),
                _ => Value::Array(Vec::new()),
            },
        });
    }
    resolved
}

/// `modules._is_active`: a module with `activate_by_schema_keys` runs only if
/// at least one of those keys is present at the top level of the config.
fn is_active(module: &Module, cfg: &Object) -> bool {
    module.activate_by_schema_keys.is_empty()
        || module
            .activate_by_schema_keys
            .iter()
            .any(|key| cfg.contains_key(*key))
}

/// The selection half of `Modules.run_section`: drop the modules that have no
/// config to act on and the ones that are not verified on this distro.
#[must_use]
pub fn select(
    mods: Vec<Details>,
    cfg: &Object,
    distro: &str,
    logger: &mut Logger,
) -> Vec<Details> {
    let overridden: Vec<&str> = cfg
        .get("unverified_modules")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut inapplicable = Vec::new();
    let mut skipped = Vec::new();
    let mut forced = Vec::new();
    let mut active = Vec::new();
    for details in mods {
        if !is_active(details.module, cfg) {
            inapplicable.push(details.name);
            continue;
        }
        let worked = details.module.distros;
        if !worked.is_empty() && worked != [ALL_DISTROS] && !worked.contains(&distro) {
            if !overridden.contains(&details.name.as_str()) {
                skipped.push(details.name);
                continue;
            }
            forced.push(details.name.clone());
        }
        active.push(details);
    }

    if !inapplicable.is_empty() {
        logger.info(
            SOURCE,
            &format!(
                "Skipping modules '{}' because no applicable config is \
                 provided.",
                inapplicable.join(",")
            ),
        );
    }
    if !skipped.is_empty() {
        logger.info(
            SOURCE,
            &format!(
                "Skipping modules '{}' because they are not verified on \
                 distro '{distro}'.  To run anyway, add them to \
                 'unverified_modules' in config.",
                skipped.join(",")
            ),
        );
    }
    if !forced.is_empty() {
        logger.info(
            SOURCE,
            &format!("running unverified_modules: '{}'", forced.join(", ")),
        );
    }
    active
}

/// `Modules.run_section`, minus the running: read, resolve, select.
#[must_use]
pub fn section(
    cfg: &Object,
    section_name: &str,
    distro: &str,
    logger: &mut Logger,
) -> Vec<Details> {
    let raw = read_modules(cfg, section_name, logger);
    let mods = fixup(&raw, logger);
    select(mods, cfg, distro, logger)
}

/// `Modules.run_single`, minus the running.
///
/// Note the asymmetry with `section`: a single module is resolved but never
/// filtered, so `cloud-init single --name apk_configure` runs on Ubuntu.
#[must_use]
pub fn single(
    name: &str,
    args: &[String],
    frequency: Option<&str>,
    logger: &mut Logger,
) -> Vec<Details> {
    let raw = Raw {
        name: name.to_owned(),
        frequency: frequency.map(str::to_owned),
        args: Some(Value::Array(
            args.iter().map(|a| Value::String(a.clone())).collect(),
        )),
    };
    fixup(&[raw], logger)
}

/// `f"config-{name}"`: the semaphore and reporting name for a module run.
#[must_use]
pub fn run_name(name: &str) -> String {
    format!("config-{name}")
}

/// `"running %s with frequency %s"`: the reporting description.
#[must_use]
pub fn description(run_name: &str, frequency: Frequency) -> String {
    format!("running {run_name} with frequency {}", frequency.as_str())
}

/// The `LOG.debug` upstream emits before each module runs. The module object
/// interpolates as its `repr`, which names the file it was imported from.
#[must_use]
pub fn running_message(details: &Details) -> String {
    format!(
        "Running module {} (<module 'cloudinit.config.{}' from \
         '/usr/lib/python3/dist-packages/cloudinit/config/{}.py'>) with \
         frequency {}",
        details.name,
        details.module.name,
        details.module.name,
        details.frequency.as_str()
    )
}

/// `repr` of a list of strings, for the "no modules to run" diagnostics.
#[must_use]
pub fn repr_names(names: &[String]) -> String {
    let rendered: Vec<String> = names.iter().map(|n| repr_str(n)).collect();
    format!("[{}]", rendered.join(", "))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn logger() -> Logger {
        Logger::silent()
    }

    fn cfg(text: &str) -> Object {
        match serde_json::from_str(text).unwrap() {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn names(mods: &[Details]) -> Vec<(&str, &str, &Value)> {
        mods.iter()
            .map(|m| (m.name.as_str(), m.frequency.as_str(), &m.args))
            .collect()
    }

    #[test]
    fn canonicalises_names_like_upstream() {
        assert_eq!(form_module_name("bootcmd").as_deref(), Some("cc_bootcmd"));
        assert_eq!(form_module_name("Bootcmd").as_deref(), Some("cc_Bootcmd"));
        assert_eq!(form_module_name("foo-bar").as_deref(), Some("cc_foo_bar"));
        assert_eq!(form_module_name("x.py").as_deref(), Some("cc_x"));
        assert_eq!(form_module_name("x.PY").as_deref(), Some("cc_x"));
        assert_eq!(form_module_name("cc_x").as_deref(), Some("cc_x"));
        assert_eq!(form_module_name("cc_").as_deref(), Some("cc_"));
        assert_eq!(form_module_name("a b").as_deref(), Some("cc_a b"));
        assert_eq!(form_module_name("  "), None);
        assert_eq!(form_module_name(""), None);
        assert_eq!(form_module_name(".py"), None);
    }

    #[test]
    fn reads_every_entry_shape() {
        let mut log = logger();
        let cfg = cfg(r#"{"m": ["bootcmd", ["runcmd", "always"],
                     ["ntp", "always", "x"],
                     {"name": "ansible", "frequency": "once", "args": ["a"]},
                     {"name": "keyboard"}, {"nothing": 1}, [], {}, null, ""]}"#);
        let raw = read_modules(&cfg, "m", &mut log);
        let got: Vec<(&str, Option<&str>, String)> = raw
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.frequency.as_deref(),
                    r.args.as_ref().map_or_else(
                        || "absent".to_owned(),
                        std::string::ToString::to_string,
                    ),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("bootcmd", None, "absent".to_owned()),
                ("runcmd", Some("always"), "absent".to_owned()),
                ("ntp", Some("always"), r#"["x"]"#.to_owned()),
                ("ansible", Some("once"), r#"["a"]"#.to_owned()),
                ("keyboard", None, "absent".to_owned()),
            ]
        );
    }

    #[test]
    fn missing_or_empty_section_is_empty() {
        let mut log = logger();
        assert!(read_modules(&cfg("{}"), "m", &mut log).is_empty());
        assert!(read_modules(&cfg(r#"{"m": []}"#), "m", &mut log).is_empty());
        assert!(read_modules(&cfg(r#"{"m": null}"#), "m", &mut log).is_empty());
    }

    #[test]
    fn a_string_section_iterates_per_character() {
        let mut log = logger();
        let raw = read_modules(&cfg(r#"{"m": "abc"}"#), "m", &mut log);
        let got: Vec<&str> = raw.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn fixup_resolves_renames_and_defaults() {
        let mut log = logger();
        let raw = read_modules(
            &cfg(r#"{"m": ["bootcmd", ["runcmd", "always"],
                          {"name": "ansible", "frequency": "once", "args": ["a", "b"]},
                          "nope", "migrator", "ubuntu_advantage",
                          ["keyboard", "bogusfreq"]]}"#),
            "m",
            &mut log,
        );
        let mods = fixup(&raw, &mut log);
        let got: Vec<(&str, &str, &str)> = mods
            .iter()
            .map(|m| (m.name.as_str(), m.module.name, m.frequency.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("bootcmd", "cc_bootcmd", "always"),
                ("runcmd", "cc_runcmd", "always"),
                ("ansible", "cc_ansible", "once"),
                // The raw name survives the rename, so the semaphore does too.
                ("ubuntu_advantage", "cc_ubuntu_pro", "once-per-instance"),
                ("keyboard", "cc_keyboard", "once-per-instance"),
            ]
        );
    }

    #[test]
    fn selects_by_schema_keys_and_distro() {
        let mut log = logger();
        let base = r#"{"bootcmd": [], "runcmd": [], "apk_repos": {}, "keyboard": {},
                       "m": ["bootcmd", "runcmd", "apk_configure", "keyboard",
                             "apt_configure", "ca_certs"]"#;
        let cfg_ubuntu = cfg(&format!("{base}}}"));
        let mods = section(&cfg_ubuntu, "m", "ubuntu", &mut log);
        assert_eq!(
            mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["bootcmd", "runcmd", "keyboard", "apt_configure"]
        );

        let mods = section(&cfg_ubuntu, "m", "alpine", &mut log);
        assert_eq!(
            mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["bootcmd", "runcmd", "apk_configure", "keyboard"]
        );

        let forced = cfg(&format!(
            r#"{base}, "unverified_modules": ["apk_configure"]}}"#
        ));
        let mods = section(&forced, "m", "ubuntu", &mut log);
        assert_eq!(
            mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec![
                "bootcmd",
                "runcmd",
                "apk_configure",
                "keyboard",
                "apt_configure"
            ]
        );
    }

    #[test]
    fn malformed_entries_are_dropped_not_fatal() {
        let mut log = logger();
        let raw = read_modules(
            &cfg(
                r#"{"m": [5, 5.5, true, {"name": 5}, {"name": "x", "frequency": 7},
                           [7], ["x", 7], "bootcmd"]}"#,
            ),
            "m",
            &mut log,
        );
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].name, "bootcmd");
        assert_eq!(
            names(&fixup(&raw, &mut log)),
            vec![("bootcmd", "always", &Value::Array(Vec::new()))]
        );
    }

    #[test]
    fn single_is_not_filtered() {
        let mut log = logger();
        let mods = single("apk_configure", &["a".to_owned()], None, &mut log);
        assert_eq!(mods.len(), 1);
        assert_eq!(mods[0].module.name, "cc_apk_configure");
        assert_eq!(mods[0].args, serde_json::json!(["a"]));
        assert!(single("nope", &[], None, &mut log).is_empty());
    }

    #[test]
    fn names_the_semaphore_after_the_configured_spelling() {
        assert_eq!(run_name("bootcmd"), "config-bootcmd");
        assert_eq!(run_name("cc_bootcmd"), "config-cc_bootcmd");
        assert_eq!(
            description("config-bootcmd", Frequency::Always),
            "running config-bootcmd with frequency always"
        );
    }
}
