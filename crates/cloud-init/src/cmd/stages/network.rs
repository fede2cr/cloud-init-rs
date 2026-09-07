//! `Init.apply_network_config`: choosing a network config and putting it on disk.
//!
//! Upstream splits this between `stages.Init` and `distros.Distro`; the split
//! buys nothing here, because no ported distro overrides either half, so the
//! whole path lives in one place.

use std::path::Path;

use ci_config::{Object, Value};
use ci_core::semaphore::{FileSemaphores, Frequency};
use ci_core::{Lookup, Paths};
use ci_datasource::event::{Scope, Type};
use ci_datasource::Datasource;
use ci_distro::Distro;
use ci_log::Logger;

/// `sources.DataSource.network_config_sources`.
///
/// Upstream lets a datasource reorder this; only MAAS and Oracle do, and
/// neither is ported, so every datasource here gets the base order.
const SOURCE_ORDER: [&str; 4] = ["cmdline", "initramfs", "system_cfg", "ds"];

/// `("apply_network_config", PER_ONCE)` — the per-boot semaphore that stops the
/// network stage redoing what the local stage already did.
const SEMAPHORE: &str = "apply_network_config";

/// What `Init.apply_network_config` was asked to do and what it found.
pub struct Request<'a> {
    /// `bring_up`: `_should_bring_up_interfaces`, i.e. the network stage
    /// unless `disable_network_activation` says otherwise.
    pub bring_up: bool,
    pub cfg: &'a Object,
    /// `Distro._cfg`, i.e. `cfg["system_info"]`. The renderer and activator
    /// priority lists live under here, *not* at the top level.
    pub system_info: &'a Object,
    pub paths: &'a Paths,
    pub distro: &'a Distro,
    pub datasource: Option<&'a Datasource>,
    /// `Init.is_new_instance()`. Meaningless without a datasource.
    pub is_new_instance: bool,
    pub cmdline: &'a str,
    /// `/run`, where klibc leaves its files. Not cloud-init's own run dir,
    /// which lives one level below it.
    pub run_root: &'a Path,
}

/// `Init._find_networking_config`.
///
/// The second half of the pair is the source name upstream logs: either a
/// `NetworkConfigSource` value or, for the upgrade marker, a path.
///
/// The error is the `ValueError` a malformed klibc file raises, which upstream
/// lets escape all the way out of `main_init`.
fn find_networking_config(
    request: &Request<'_>,
    log: &mut Logger,
) -> Result<(Option<Object>, String), String> {
    let disable_file = request.paths.cpath(Lookup::Data).join("upgraded-network");
    if disable_file.exists() {
        return Ok((None, disable_file.display().to_string()));
    }

    for source in SOURCE_ORDER {
        let candidate = match source {
            "cmdline" => {
                ci_net::cmdline::read_kernel_cmdline_config(request.cmdline, log)
            }
            "initramfs" => ci_net::cmdline::read_initramfs_config(
                request.run_root,
                request.cmdline,
                &ci_net::sysfs::Sys::real(),
            )
            .map_err(|err| err.to_string())?,
            "system_cfg" => network_key(request.cfg.get("network")),
            "ds" => request
                .datasource
                .and_then(|ds| ds.network_config.as_ref())
                .and_then(network_key_of),
            _ => None,
        };
        if ci_net::cmdline::is_disabled_cfg(candidate.as_ref()) {
            log.debug("stages.py", &format!("network config disabled by {source}"));
            return Ok((None, source.to_owned()));
        }
        if candidate.as_ref().is_some_and(|cfg| !cfg.is_empty()) {
            return Ok((candidate, source.to_owned()));
        }
    }

    if !request
        .cfg
        .get("network")
        .is_none_or(ci_config::option::py_truthy)
    {
        log.warning("stages.py", "Empty network config found");
    }
    Ok((
        ci_net::sysfs::Sys::real().generate_fallback_config(false),
        "fallback".to_owned(),
    ))
}

/// `Init._get_network_key_contents`: unwrap one `network:` level if it is there.
fn network_key(value: Option<&Value>) -> Option<Object> {
    value.and_then(network_key_of)
}

fn network_key_of(value: &Value) -> Option<Object> {
    let object = value.as_object()?;
    match object.get("network").and_then(Value::as_object) {
        Some(inner) => Some(inner.clone()),
        None => Some(object.clone()),
    }
}

/// `Init.apply_network_config`.
///
/// The error is the text upstream would have let escape as an exception, which
/// the caller records in `result.json` exactly as upstream does.
pub fn apply_network_config(
    request: &Request<'_>,
    log: &mut Logger,
) -> Result<(), String> {
    let (netcfg, source) = find_networking_config(request, log)?;
    let Some(netcfg) = netcfg else {
        log.info(
            "stages.py",
            &format!("network config is disabled by {source}"),
        );
        return Ok(());
    };

    if request.datasource.is_some()
        && !request.is_new_instance
        && !should_run_on_boot_event(request, log)
        && !event_enabled_and_metadata_updated(request, Type::BootLegacy, log)
    {
        log.debug(
            "stages.py",
            "No network config applied. Neither a new instance nor datasource \
             network update allowed",
        );
        // Upstream still applies renames here. `apply_network_config_names`
        // is not ported (COMPAT.md deviation 125), so there is nothing to do.
        return Ok(());
    }

    write_network_config_json(&netcfg, request.paths, log);

    log.info(
        "stages.py",
        &format!(
            "Applying network configuration from {source} bringup={}: {}",
            if request.bring_up { "True" } else { "False" },
            ci_config::repr::repr(&Value::Object(netcfg.clone())),
        ),
    );

    // `_acquire` logs a write failure and hands back no lock; the render runs
    // either way, so a semaphore that cannot be written costs a redundant
    // re-render next boot rather than the network.
    let semaphores = FileSemaphores::new(request.paths.run_path(Lookup::Sem));
    if let Err(err) = semaphores.acquire(SEMAPHORE, Frequency::Once) {
        log.warning(
            "helpers.py",
            &format!(
                "Failed writing semaphore file {}: {err}",
                semaphores.path(SEMAPHORE, Frequency::Once).display()
            ),
        );
    }

    render(&netcfg, request, log)
}

/// `Distro.apply_network_config`.
fn render(
    netcfg: &Object,
    request: &Request<'_>,
    log: &mut Logger,
) -> Result<(), String> {
    let priority = priority_list(request.system_info, "renderers");

    let name = ci_net::renderers::select(priority.as_deref())
        .map_err(|err| err.to_string())?;
    log.debug(
        "__init__.py",
        &format!(
            "Selected renderer '{name}' from priority list: {}",
            match &priority {
                Some(list) => ci_config::repr::repr(&Value::from(list.clone())),
                None => "None".to_owned(),
            }
        ),
    );
    if name != "netplan" {
        // Only the netplan renderer has a body here. Upstream would have
        // rendered one of the other seven; writing nothing and carrying on
        // would leave the machine off the network with no explanation, so the
        // stage fails instead. See COMPAT.md deviation 125.
        return Err(format!(
            "cloud-init-rs: the '{name}' renderer is not implemented; \
             refusing to leave the network unconfigured"
        ));
    }

    let mut warnings = ci_net::state::Warnings::default();
    let state = ci_net::state::parse_net_config_data(
        &Value::Object(netcfg.clone()),
        ci_net::state::Target::Netplan,
        &mut warnings,
    )
    .map_err(|err| err.to_string())?;
    for warning in &warnings.0 {
        log.warning("network_state.py", warning);
    }

    let renderer =
        ci_net::renderer::Netplan::from_config(&request.distro.renderer_config(name));
    let (content, netplan_warnings) = renderer.content(&state);
    for warning in &netplan_warnings.0 {
        log.warning("netplan.py", warning);
    }

    write_netplan(Path::new(&renderer.path), &content)?;
    bring_up(&state, request, log)
}

/// `util.get_cfg_by_path(self._cfg, ("network", key), None)`.
fn priority_list(system_info: &Object, key: &str) -> Option<Vec<String>> {
    system_info
        .get("network")
        .and_then(|network| network.get(key))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
                .collect()
        })
}

/// The bring-up half of `Distro.apply_network_config`.
///
/// Only `NoActivatorException` is caught upstream: an unknown name in the
/// priority list is a `ValueError` that fails the stage, and so is a failure of
/// the one activator command that runs outside `_alter_interface`.
fn bring_up(
    state: &ci_net::state::NetworkState,
    request: &Request<'_>,
    log: &mut Logger,
) -> Result<(), String> {
    if !request.bring_up {
        log.debug(
            "__init__.py",
            "Not bringing up newly configured network interfaces",
        );
        return Ok(());
    }
    log.debug(
        "__init__.py",
        "Bringing up newly configured network interfaces",
    );

    let priority = priority_list(request.system_info, "activators");
    let activator = match ci_net::activators::select(priority.as_deref(), log) {
        Ok(activator) => activator,
        Err(ci_net::activators::Error::NoActivator(_)) => {
            log.warning(
                "__init__.py",
                "No network activator found, not bringing up network interfaces",
            );
            return Ok(());
        }
        Err(err) => return Err(err.to_string()),
    };
    // The return value says whether every interface came up; upstream discards
    // it, and each failure has already been logged by the activator.
    activator
        .bring_up_all_interfaces(state, log)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// `Distro.wait_for_network`, called from `main_init` stage 4 when the local
/// stage did not leave a `.skip-network` marker.
///
/// The base method is a documented no-op, so only Ubuntu does anything, and it
/// swallows every failure: a machine that cannot wait still boots. That is why
/// nothing here returns a `Result`.
pub fn wait_for_network(distro: &Distro, system_info: &Object, log: &mut Logger) {
    if !distro.waits_for_network {
        return;
    }
    // `self.network_activator`, which is the same lookup `bring_up` does.
    let priority = priority_list(system_info, "activators");
    let activator = match ci_net::activators::select(priority.as_deref(), log) {
        Ok(activator) => activator,
        Err(ci_net::activators::Error::NoActivator(_)) => {
            log.error(
                "ubuntu.py",
                "Failed to wait for network. No network activator found",
            );
            return;
        }
        // An unknown name in the priority list is a `ValueError` upstream,
        // which the bare `except Exception` below it catches.
        Err(err) => {
            log.error("ubuntu.py", &format!("Failed to wait for network: {err}"));
            return;
        }
    };
    if let Err(err) = activator.wait_for_network(log) {
        // `WaitError::NotImplemented` prints as nothing, because
        // `str(NotImplementedError())` is empty and upstream uses `%s`.
        log.error("ubuntu.py", &format!("Failed to wait for network: {err}"));
    }
}

/// `fallback_write_netplan_yaml`.
///
/// Upstream tries netplan's own python bindings first and only lands here when
/// they are missing or raise; the bindings pick the same path and the same
/// mode, so this is the one branch worth having.
fn write_netplan(path: &Path, content: &str) -> Result<(), String> {
    // 0600 because `features.NETPLAN_CONFIG_ROOT_READ_ONLY` is on: a netplan
    // config can carry wifi passwords.
    let mut mode = 0o600;
    if let Ok(existing) = std::fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        let current = existing.permissions().mode() & 0o777;
        // Preserve an existing mode that is already at least as strict.
        if current & mode == current {
            mode = current;
        }
    }
    if let Some(parent) = path.parent() {
        let _ = ci_sys::path::ensure_dir(parent, 0o755);
    }
    ci_sys::atomic::write_file(
        path,
        content.as_bytes(),
        ci_sys::atomic::WriteOptions {
            mode,
            durable: true,
        },
    )
    .map_err(|err| ci_core::pyerr::oserror(&err, path))
}

/// `Init._write_network_config_json`.
///
/// Silent before `instancify` has run: without the instance link there is
/// nowhere instance-scoped to put it.
fn write_network_config_json(netcfg: &Object, paths: &Paths, log: &mut Logger) {
    let link = paths.instance_link();
    if !link.is_symlink() {
        return;
    }
    let instance_path = paths.instance_path(Lookup::NetworkConfig);
    let json = ci_core::jsonfmt::dumps_indent(&Value::Object(netcfg.clone()), 1);

    let unchanged = std::fs::read_to_string(&instance_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|current| current == Value::Object(netcfg.clone()));
    if !unchanged {
        if let Err(err) = ci_sys::atomic::write_file(
            &instance_path,
            json.as_bytes(),
            ci_sys::atomic::WriteOptions::SECRET,
        ) {
            log.warning(
                "stages.py",
                &format!("Failed to write {}: {err}", instance_path.display()),
            );
            return;
        }
    }

    let run_link = paths.run_path(Lookup::NetworkConfig);
    if !run_link.is_symlink() {
        let _ = std::os::unix::fs::symlink(&instance_path, &run_link);
    }
}

/// `should_run_on_boot_event` from inside `apply_network_config`.
fn should_run_on_boot_event(request: &Request<'_>, log: &mut Logger) -> bool {
    let sem = FileSemaphores::new(request.paths.run_path(Lookup::Sem));
    !sem.has_run(SEMAPHORE, Frequency::Once)
        && event_enabled_and_metadata_updated(request, Type::Boot, log)
}

/// `event_enabled_and_metadata_updated`.
///
/// Both halves are lazy, as upstream's `and` is: the re-crawl only happens for
/// a tenant who asked for it, which by default nobody has.
fn event_enabled_and_metadata_updated(
    request: &Request<'_>,
    event: Type,
    log: &mut Logger,
) -> bool {
    let Some(datasource) = request.datasource else {
        return false;
    };
    let Some(probe) = ci_datasource::probe_for_class(datasource.class_name) else {
        return false;
    };
    if !ci_datasource::event::update_event_enabled(
        probe.as_ref(),
        request.cfg,
        event,
        Scope::Network,
        request.paths,
        log,
    ) {
        return false;
    }
    update_metadata_if_supported(probe.as_ref(), datasource, event, request, log)
}

/// `DataSource.update_metadata_if_supported`, for the single event upstream
/// ever passes it from here.
fn update_metadata_if_supported(
    probe: &dyn ci_datasource::Probe,
    datasource: &Datasource,
    event: Type,
    request: &Request<'_>,
    log: &mut Logger,
) -> bool {
    let scopes: Vec<Scope> = probe
        .supported_update_events()
        .iter()
        .filter(|(_, events)| events.contains(&event))
        .map(|(scope, _)| *scope)
        .collect();
    for scope in &scopes {
        log.debug(
            "__init__.py",
            &format!(
                "Update datasource metadata and {scope} config due to events: {event}"
            ),
        );
    }
    if scopes.is_empty() {
        log.debug(
            "__init__.py",
            &format!("Datasource {datasource} not updated for events: {event}"),
        );
        return false;
    }

    let mut restricted = request.cfg.clone();
    restricted.remove("system_info");
    // `update_metadata_if_supported` re-runs the datasource outside any
    // reporting scope, so nothing here has handlers to publish to.
    let mut reporter = ci_report::Reporter::silent();
    let mut ctx = ci_datasource::Context {
        sys_cfg: &restricted,
        paths: request.paths,
        cmdline: request.cmdline,
        limits: ci_config::Limits::default(),
        logger: log,
        reporter: &mut reporter,
    };
    if probe.get_data(&mut ctx).is_some() {
        return true;
    }
    log.debug(
        "__init__.py",
        &format!("Datasource {datasource} not updated for events: {event}"),
    );
    false
}

/// `_should_bring_up_interfaces`.
#[must_use]
pub fn should_bring_up_interfaces(cfg: &Object, local: bool) -> bool {
    if cfg
        .get("disable_network_activation")
        .is_some_and(ci_config::option::py_truthy)
    {
        return false;
    }
    !local
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn system_info(yaml: &str) -> Object {
        ci_config::yaml::load_mapping(yaml, ci_config::Limits::default()).unwrap()
    }

    #[test]
    fn the_priority_lists_come_from_system_info_not_the_top_level() {
        let cfg = system_info(
            "network:\n  renderers: [netplan]\n  activators: [netplan, networkd]\n",
        );
        assert_eq!(
            priority_list(&cfg, "renderers"),
            Some(vec!["netplan".to_owned()])
        );
        assert_eq!(
            priority_list(&cfg, "activators"),
            Some(vec!["netplan".to_owned(), "networkd".to_owned()])
        );
        assert_eq!(priority_list(&cfg, "nosuch"), None);
        assert_eq!(priority_list(&Object::new(), "renderers"), None);
    }

    #[test]
    fn only_the_network_stage_brings_interfaces_up() {
        let empty = Object::new();
        assert!(!should_bring_up_interfaces(&empty, true));
        assert!(should_bring_up_interfaces(&empty, false));

        let disabled = system_info("disable_network_activation: true\n");
        assert!(!should_bring_up_interfaces(&disabled, false));
        // Any truthy value, not just `true`: upstream uses plain truthiness.
        let disabled = system_info("disable_network_activation: 1\n");
        assert!(!should_bring_up_interfaces(&disabled, false));
        let enabled = system_info("disable_network_activation: 0\n");
        assert!(should_bring_up_interfaces(&enabled, false));
    }
}
