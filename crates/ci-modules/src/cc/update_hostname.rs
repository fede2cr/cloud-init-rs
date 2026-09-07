//! Port of `cloudinit/config/cc_update_hostname.py`.
//!
//! The `PER_ALWAYS` counterpart to `cc_set_hostname`. That one names the
//! machine once per instance; this one runs on every boot and reconciles what
//! is on disk with what is running — but only while the two still agree with
//! the record cloud-init keeps. Once an operator has renamed the machine by
//! hand, the plan notices and comes back with nothing to do, which is what
//! lets a manual rename survive a reboot.

use ci_config::option;
use ci_core::paths::Lookup;
use ci_distro::hostname::Step;

use super::Args;

const SOURCE: &str = "cc_update_hostname.py";

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let Some(steps) = plan(args)? else {
        return Ok(());
    };
    ci_distro::hostname::run_update(
        args.distro,
        args.cfg,
        args.root,
        &steps,
        args.logger,
    );
    Ok(())
}

/// The module's decision, separated from carrying it out.
///
/// `None` is the two early returns: `preserve_hostname`, and a `localhost`
/// nobody asked for.
///
/// # Errors
/// Whatever `Distro.update_hostname` raised. Upstream logs it and re-raises,
/// which fails the module.
pub fn plan(args: &mut Args<'_>) -> Result<Option<Vec<Step>>, String> {
    if option::get_bool(args.cfg, "preserve_hostname", false) {
        let message = format!(
            "Configuration option 'preserve_hostname' is set, not updating the \
             hostname in module {}",
            args.name
        );
        args.debug(SOURCE, &message);
        return Ok(None);
    }

    // Upstream copies `prefer_fqdn_over_hostname` and `create_hostname_file`
    // onto `distro._cfg` here so the distro methods can read them back. The
    // table this port carries is static, so both keys are read straight out
    // of the config at the point of use instead — see `cc_set_hostname`.

    let metadata = args.datasource.map(|ds| ds.metadata);
    let resolved = ci_core::hostname::get_hostname_fqdn(args.cfg, metadata, args.root);

    if resolved.is_default && resolved.hostname == "localhost" {
        args.debug(
            SOURCE,
            "Hostname is localhost. Let other services handle this.",
        );
        return Ok(None);
    }

    let previous = args.paths.cpath(Lookup::Data).join("previous-hostname");
    let message = format!(
        "Updating hostname to {} ({})",
        resolved.fqdn, resolved.hostname
    );
    args.debug(SOURCE, &message);

    ci_distro::hostname::plan_update(
        args.distro,
        args.cfg,
        args.root,
        Some(&resolved.hostname),
        Some(&resolved.fqdn),
        &previous,
        args.logger,
    )
    .map(Some)
    .map_err(|error| {
        format!(
            "Failed to update the hostname to {} ({}): {error}",
            resolved.fqdn, resolved.hostname
        )
    })
}
