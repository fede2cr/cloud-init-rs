//! Port of `cloudinit/config/cc_update_etc_hosts.py`.
//!
//! Two unrelated behaviours behind one key. `manage_etc_hosts: true` renders
//! the whole of `/etc/hosts` from a per-family template, replacing whatever
//! was there; `manage_etc_hosts: localhost` leaves the file alone apart from
//! the one loopback entry that has to name this machine. Anything else is a
//! no-op.
//!
//! The distinction matters more than it looks: the template path throws away
//! hand-added entries on every boot, which is exactly what the template's own
//! comment header warns about.

use ci_config::Value;

use super::Args;

const SOURCE: &str = "cc_update_etc_hosts.py";

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    // `util.get_cfg_option_str` stringifies whatever it finds, so a YAML
    // `true` arrives here as `"True"` and only ever compares equal to
    // `"template"` when it was spelled that way.
    let manage = args.cfg.get("manage_etc_hosts").map(super::py_str);
    let manage = manage.as_deref();

    if translate_bool_with_template(manage) {
        if manage == Some("template") {
            let message = "Value 'template' for key 'manage_etc_hosts' is \
                 deprecated in 22.2 and scheduled to be removed in 27.2. Use \
                 'true' instead.";
            args.logger
                .log(ci_log::Level::Deprecated, "lifecycle.py", message);
        }
        let Some(resolved) = resolve(args) else {
            return Ok(());
        };
        let name = format!("hosts.{}", args.distro.osfamily);
        let Some(template) = template_filename(args, &name) else {
            return Err(format!(
                "No hosts template could be found for distro {}",
                args.distro.osfamily
            ));
        };
        return render(args, &template, &resolved.0, &resolved.1);
    }

    if manage == Some("localhost") {
        let Some((hostname, fqdn)) = resolve(args) else {
            return Ok(());
        };
        let path = args.root.join(ci_distro::HOSTS_FN.trim_start_matches('/'));
        args.debug(SOURCE, &format!("Managing localhost in {}", path.display()));
        return ci_distro::hosts::update_etc_hosts(
            args.distro,
            args.root,
            &hostname,
            &fqdn,
        );
    }

    let message = format!(
        "Configuration option 'manage_etc_hosts' is not set, not managing {} \
         in module {}",
        ci_distro::HOSTS_FN,
        args.name
    );
    args.debug(SOURCE, &message);
    Ok(())
}

/// `util.translate_bool(manage_hosts, addons=["template"])`.
///
/// `option::translate_bool` cannot express the addon, and the value has
/// already been stringified, so the falsy test is "absent or empty" rather
/// than Python truthiness.
fn translate_bool_with_template(value: Option<&str>) -> bool {
    let Some(text) = value.filter(|text| !text.is_empty()) else {
        return false;
    };
    matches!(
        text.trim().to_lowercase().as_str(),
        "true" | "1" | "on" | "yes" | "template"
    )
}

/// `util.get_hostname_fqdn`, with the "no hostname" warning both arms share.
fn resolve(args: &mut Args<'_>) -> Option<(String, String)> {
    let metadata = args.datasource.map(|ds| ds.metadata);
    let resolved = ci_core::hostname::get_hostname_fqdn(args.cfg, metadata, args.root);
    if resolved.hostname.is_empty() {
        args.logger.warning(
            SOURCE,
            "Option 'manage_etc_hosts' was set, but no hostname was found",
        );
        return None;
    }
    Some((resolved.hostname, resolved.fqdn))
}

/// `cloud.get_template_filename(name)`.
///
/// Not root-prefixed: `Paths.template_tpl` is configurable in its own right
/// (`system_info.paths.templates_dir`), which is how upstream relocates it,
/// and applying the root as well would move it twice.
fn template_filename(args: &mut Args<'_>, name: &str) -> Option<std::path::PathBuf> {
    let path = args.paths.template_tpl(name);
    if path.is_file() {
        return Some(path);
    }
    let message = format!(
        "No template found in {} for template named {name}",
        path.parent().unwrap_or(&path).display()
    );
    args.logger.warning("cloud.py", &message);
    None
}

/// `templater.render_to_file(tpl, hosts_fn, params)`.
fn render(
    args: &mut Args<'_>,
    template: &std::path::Path,
    hostname: &str,
    fqdn: &str,
) -> Result<(), String> {
    let content = std::fs::read_to_string(template)
        .map_err(|error| format!("{}: {error}", template.display()))?;
    let mut params = ci_config::Object::new();
    params.insert("hostname".to_owned(), Value::from(hostname));
    params.insert("fqdn".to_owned(), Value::from(fqdn));
    let rendered = ci_template::render_string(&content, &Value::Object(params))
        .map_err(|error| format!("{}: {error}", template.display()))?;

    let path = args.root.join(ci_distro::HOSTS_FN.trim_start_matches('/'));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    ci_sys::atomic::write_file(
        &path,
        rendered.as_bytes(),
        ci_sys::atomic::WriteOptions {
            mode: 0o644,
            ..ci_sys::atomic::WriteOptions::default()
        },
    )
    .map_err(|error| format!("{}: {error}", path.display()))
}
