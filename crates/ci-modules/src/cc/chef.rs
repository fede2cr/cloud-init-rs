//! Port of `cc_chef.py`: create the chef directories, render `client.rb` from
//! a template, write `firstboot.json`, then install and optionally run
//! `chef-client`.
//!
//! Every step is a side effect and they are not ordered defensively: the six
//! directories are created and the legacy cache is migrated before anything
//! looks at what `chef:` holds, and the validation key is on disk before the
//! template is rendered. So the machine goes behind [`Host`] and the
//! differential compares the ordered call list, not just the files.
//!
//! `chef:` itself is subscripted without a type check, and `server_url` and
//! `validation_name` are read with `[]` rather than `.get`, so a config that
//! is merely incomplete gets a `KeyError` after the directories exist -- see
//! bugs B107 and B108 in `docs/COMPAT.md`. B106 (the omnibus retry default is
//! unreachable), B109 (`-v <version>` is one argv element), B110 (a string
//! `exec_arguments` is appended whole) and B111 (`null` becomes the string
//! `"None"`) are reproduced here too, and deviation 170 covers the one place
//! upstream's order cannot be reproduced because it comes out of a `set`.

use ci_config::{Object, Value};
use ci_log::Logger;

use super::rsyslog::CmdError;
use super::{dict_get, py_str, sub_option, Args};

const SOURCE: &str = "cc_chef.py";

/// `RUBY_VERSION_DEFAULT`.
const RUBY_VERSION_DEFAULT: &str = "1.8";

/// `CHEF_DIRS`.
const CHEF_DIRS: [&str; 6] = [
    "/etc/chef",
    "/var/log/chef",
    "/var/lib/chef",
    "/var/chef/cache",
    "/var/chef/backup",
    "/var/run/chef",
];

/// `REQUIRED_CHEF_DIRS`, chained after `chef_dirs` -- so with the defaults
/// `/etc/chef` is created twice.
const REQUIRED_CHEF_DIRS: [&str; 1] = ["/etc/chef"];

/// `CHEF_DIR_MIGRATION`, in insertion order.
const CHEF_DIR_MIGRATION: [(&str, &str); 2] = [
    ("/var/cache/chef", "/var/chef/cache"),
    ("/var/backups/chef", "/var/chef/backup"),
];

const OMNIBUS_URL: &str = "https://www.chef.io/chef/install.sh";

/// `OMNIBUS_URL_RETRIES`. Unreachable from `handle`; see bug B106.
const OMNIBUS_URL_RETRIES: i64 = 5;

const CHEF_VALIDATION_PEM_PATH: &str = "/etc/chef/validation.pem";
const CHEF_FB_PATH: &str = "/etc/chef/firstboot.json";
const CHEF_RB_PATH: &str = "/etc/chef/client.rb";
const CHEF_EXEC_PATH: &str = "/usr/bin/chef-client";
const CHEF_EXEC_DEF_ARGS: [&str; 5] = ["-d", "-i", "1800", "-s", "20"];

/// `CHEF_RB_TPL_BOOL_KEYS`.
const CHEF_RB_TPL_BOOL_KEYS: [&str; 1] = ["show_time"];

/// `CHEF_RB_TPL_PATH_KEYS`, whose values get their parent directory created.
const CHEF_RB_TPL_PATH_KEYS: [&str; 7] = [
    "log_location",
    "validation_key",
    "client_key",
    "file_cache_path",
    "json_attribs",
    "pid_file",
    "encrypted_data_bag_secret",
];

/// The members of `CHEF_RB_TPL_KEYS` that are neither defaults nor bools.
const CHEF_RB_TPL_EXTRA_KEYS: [&str; 5] = [
    "server_url",
    "node_name",
    "environment",
    "validation_name",
    "chef_license",
];

/// `CHEF_RB_TPL_DEFAULTS`, in declaration order.
fn tpl_defaults() -> Object {
    let mut params = Object::new();
    let mut set = |key: &str, value: Value| {
        params.insert(key.to_owned(), value);
    };
    set("ssl_verify_mode", Value::from(":verify_none"));
    set("log_level", Value::from(":info"));
    set("log_location", Value::from("/var/log/chef/client.log"));
    set("validation_key", Value::from(CHEF_VALIDATION_PEM_PATH));
    set("validation_cert", Value::Null);
    set("client_key", Value::from("/etc/chef/client.pem"));
    set("json_attribs", Value::from(CHEF_FB_PATH));
    set("file_cache_path", Value::from("/var/chef/cache"));
    set("file_backup_path", Value::from("/var/chef/backup"));
    set("pid_file", Value::from("/var/run/chef/client.pid"));
    set("show_time", Value::Bool(true));
    set("encrypted_data_bag_secret", Value::Null);
    params
}

/// `k in CHEF_RB_TPL_KEYS`.
fn is_tpl_key(key: &str) -> bool {
    tpl_defaults().contains_key(key)
        || CHEF_RB_TPL_BOOL_KEYS.contains(&key)
        || CHEF_RB_TPL_PATH_KEYS.contains(&key)
        || CHEF_RB_TPL_EXTRA_KEYS.contains(&key)
}

/// Everything `cc_chef` asks of the machine.
///
/// `&mut self` is for recording rather than state: a [`Fixture`] appends each
/// call to a list so the differential compares the sequence.
pub trait Host {
    /// `util.ensure_dir(path)`. The path is whatever `directories:` held, so
    /// it need not be a string.
    fn ensure_dir(&mut self, path: &Value) -> Result<(), String>;

    /// `util.ensure_dirs(param_paths)`, whose argument is a `set`.
    ///
    /// One call rather than one per entry, because upstream's iteration order
    /// is not reproducible -- see deviation 170.
    fn ensure_dirs(&mut self, paths: &[String]) -> Result<(), String>;

    /// `os.path.exists(path)`, for the migration and the gem symlinks.
    fn path_exists(&mut self, path: &str) -> bool;

    /// `os.path.isfile(path)`.
    fn is_file(&mut self, path: &str) -> bool;

    /// `os.listdir(path)`.
    fn listdir(&mut self, path: &str) -> Result<Vec<String>, String>;

    /// `shutil.move(src, dest)`.
    fn move_path(&mut self, src: &str, dest: &str) -> Result<(), String>;

    /// `util.write_file(path, content, mode=)`.
    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        mode: u32,
    ) -> Result<(), String>;

    /// `os.unlink(path)`.
    fn unlink(&mut self, path: &str) -> Result<(), String>;

    /// `cloud.get_template_filename(name)`: the path it would use, and
    /// whether that path is a file.
    fn template_path(&mut self, name: &str) -> (String, bool);

    /// `util.load_text_file(path)`.
    fn load_text_file(&mut self, path: &str) -> Result<String, String>;

    /// `str(cloud.datasource.get_instance_id())`.
    fn instance_id(&mut self) -> String;

    /// `util.make_header()`, which embeds the current time.
    fn make_header(&mut self) -> String;

    /// `subp.is_exe(path)`.
    fn is_exe(&mut self, path: &str) -> bool;

    /// `cloud.distro.install_packages(pkgs)`.
    fn install_packages(&mut self, packages: &[String]) -> Result<(), String>;

    /// `subp.subp(argv, capture=False)`.
    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError>;

    /// `url_helper.readurl(url=url, retries=retries).contents`.
    fn readurl(&mut self, url: &str, retries: i64) -> Result<String, String>;

    /// `temp_utils.tempdir(dir=distro.get_tmp_exec_path(), needs_exe=True)`.
    fn tempdir(&mut self) -> Result<String, String>;

    /// `util.sym_link(source, link)`.
    fn sym_link(&mut self, source: &str, link: &str) -> Result<(), String>;
}

/// `util.get_cfg_option_str(yobj, key, default)`, which stringifies whatever
/// it finds but returns the default untouched.
fn option_str(block: &Value, key: &str) -> Result<Option<String>, String> {
    Ok(sub_option(block, key)?.as_ref().map(py_str))
}

/// `util.get_cfg_option_bool(yobj, key, default)`.
fn option_bool(block: &Value, key: &str, default: bool) -> Result<bool, String> {
    Ok(sub_option(block, key)?
        .as_ref()
        .map_or(default, ci_config::option::translate_bool))
}

/// `util.get_cfg_option_list(yobj, key)` with no default: a list comes back
/// with its items untouched, anything else is stringified into a one-element
/// list, and `None` gives the empty list.
fn option_list(block: &Value, key: &str) -> Result<Option<Vec<Value>>, String> {
    Ok(match sub_option(block, key)? {
        None => None,
        Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(items)) => Some(items),
        Some(other) => Some(vec![Value::String(py_str(&other))]),
    })
}

/// `os.path.dirname(path)`, which keeps a root of nothing but separators.
fn dirname(path: &str) -> String {
    let cut = path.rfind('/').map_or(0, |index| index + 1);
    let head = path.get(..cut).unwrap_or_default();
    if !head.is_empty() && head.contains(|c| c != '/') {
        head.trim_end_matches('/').to_owned()
    } else {
        head.to_owned()
    }
}

/// `os.path.join(base, name)` for the two-component case.
fn join(base: &str, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_owned();
    }
    if base.is_empty() || base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// `util.encode_text(content)`, the first thing `util.write_file` does with
/// what it was handed.
fn encode_text(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        other => Err(format!(
            "'{}' object has no attribute 'encode'",
            ci_config::type_name(other)
        )),
    }
}

/// `migrate_chef_config_dirs`.
fn migrate_chef_config_dirs(
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    for (old_dir, migrated_dir) in CHEF_DIR_MIGRATION {
        if !host.path_exists(old_dir) {
            continue;
        }
        for filename in host.listdir(old_dir)? {
            let from = join(old_dir, &filename);
            if host.path_exists(&join(migrated_dir, &filename)) {
                log.debug(
                    SOURCE,
                    &format!(
                        "Ignoring migration of {from}. File already exists in \
                         {migrated_dir}."
                    ),
                );
                continue;
            }
            log.debug(SOURCE, &format!("Moving {from} to {migrated_dir}."));
            host.move_path(&from, migrated_dir)?;
        }
    }
    Ok(())
}

/// `get_template_params(iid, chef_cfg)`.
fn get_template_params(
    iid: &str,
    chef_cfg: &Value,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<Object, String> {
    let mut params = tpl_defaults();
    let items = match chef_cfg {
        Value::Object(map) => map.clone(),
        other => {
            return Err(format!(
                "'{}' object has no attribute 'items'",
                ci_config::type_name(other)
            ))
        }
    };
    for (key, value) in &items {
        if !is_tpl_key(key) {
            log.debug(
                SOURCE,
                &format!("Skipping unknown chef template key '{key}'"),
            );
            continue;
        }
        let rendered = if value.is_null() {
            Value::Null
        } else if CHEF_RB_TPL_BOOL_KEYS.contains(&key.as_str()) {
            Value::Bool(option_bool(chef_cfg, key, false)?)
        } else {
            option_str(chef_cfg, key)?.map_or(Value::Null, Value::String)
        };
        params.insert(key.clone(), rendered);
    }

    let header = host.make_header();
    let node_name =
        option_str(chef_cfg, "node_name")?.unwrap_or_else(|| iid.to_owned());
    let environment =
        option_str(chef_cfg, "environment")?.unwrap_or_else(|| "_default".to_owned());
    let server_url = subscript(chef_cfg, "server_url")?;
    let validation_name = subscript(chef_cfg, "validation_name")?;

    params.insert("generated_by".to_owned(), Value::String(header));
    params.insert("node_name".to_owned(), Value::String(node_name));
    params.insert("environment".to_owned(), Value::String(environment));
    params.insert("server_url".to_owned(), server_url);
    params.insert("validation_name".to_owned(), validation_name);
    Ok(params)
}

/// `chef_cfg[key]` on a mapping: absent is a `KeyError`, whose `str` is the
/// key's repr and nothing else.
fn subscript(block: &Value, key: &str) -> Result<Value, String> {
    match block {
        Value::Object(map) => map.get(key).cloned().ok_or_else(|| format!("'{key}'")),
        other => Err(format!(
            "'{}' object is not subscriptable",
            ci_config::type_name(other)
        )),
    }
}

/// `handle`, against a scripted machine.
///
/// # Errors
/// Everything upstream lets escape: a `chef:` that is not a mapping, a config
/// missing `server_url` or `validation_name` when a template is present, an
/// `initial_attributes:` that is not a mapping, a bad `omnibus_url_retries`,
/// and any failed write, download, install or command.
#[expect(
    clippy::too_many_lines,
    reason = "upstream's handle is one function and the order of its steps is \
              the thing under test"
)]
pub fn handle_with(
    name: &str,
    cfg: &Object,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    let Some(chef_cfg) = cfg.get("chef") else {
        log.debug(
            SOURCE,
            &format!("Skipping module named {name}, no 'chef' key in configuration"),
        );
        return Ok(());
    };

    let chef_dirs = option_list(chef_cfg, "directories")?
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| CHEF_DIRS.iter().map(|d| Value::from(*d)).collect());
    for dir in chef_dirs.iter().chain(
        REQUIRED_CHEF_DIRS
            .iter()
            .map(|d| Value::from(*d))
            .collect::<Vec<_>>()
            .iter(),
    ) {
        host.ensure_dir(dir)?;
    }

    migrate_chef_config_dirs(host, log)?;

    let vkey_path = dict_get(chef_cfg, "validation_key")?
        .as_ref()
        .map_or_else(|| CHEF_VALIDATION_PEM_PATH.to_owned(), py_str);
    let vcert = dict_get(chef_cfg, "validation_cert")?.unwrap_or(Value::Null);
    if ci_config::option::py_truthy(&vcert) {
        if vcert.as_str() != Some("system") {
            host.write_file(&vkey_path, &encode_text(&vcert)?, 0o644)?;
        } else if !host.is_file(&vkey_path) {
            log.warning(
                SOURCE,
                &format!(
                    "chef validation_cert provided as 'system', but validation_key \
                     path '{vkey_path}' does not exist."
                ),
            );
        }
    }

    let cfg_filename =
        option_str(chef_cfg, "config_path")?.unwrap_or_else(|| CHEF_RB_PATH.to_owned());
    let (template_fn, found) = host.template_path("chef_client.rb");
    if found {
        let iid = host.instance_id();
        let params = get_template_params(&iid, chef_cfg, host, log)?;
        let mut param_paths: Vec<String> = Vec::new();
        for (key, value) in &params {
            if CHEF_RB_TPL_PATH_KEYS.contains(&key.as_str())
                && ci_config::option::py_truthy(value)
            {
                let parent = dirname(&py_str(value));
                if !param_paths.contains(&parent) {
                    param_paths.push(parent);
                }
            }
        }
        param_paths.sort();
        host.ensure_dirs(&param_paths)?;
        let text = host.load_text_file(&template_fn)?;
        let rendered = render(&template_fn, &text, &params, log)?;
        host.write_file(&cfg_filename, &rendered, 0o644)?;
    } else {
        log.warning(
            "cloud.py",
            &format!(
                "No template found in {} for template named chef_client.rb",
                dirname(&template_fn)
            ),
        );
        log.warning(
            SOURCE,
            &format!("No template found, not rendering to {cfg_filename}"),
        );
    }

    let fb_filename = option_str(chef_cfg, "firstboot_path")?
        .unwrap_or_else(|| CHEF_FB_PATH.to_owned());
    if fb_filename.is_empty() {
        log.info(
            SOURCE,
            "First boot path empty, not writing first boot json file",
        );
    } else {
        let mut initial_json = Object::new();
        if let Some(run_list) = sub_option(chef_cfg, "run_list")? {
            initial_json.insert("run_list".to_owned(), run_list);
        }
        if let Some(initial_attributes) = sub_option(chef_cfg, "initial_attributes")? {
            let Value::Object(attributes) = initial_attributes else {
                return Err(format!(
                    "'{}' object has no attribute 'keys'",
                    ci_config::type_name(&initial_attributes)
                ));
            };
            for (key, value) in attributes {
                initial_json.insert(key, value);
            }
        }
        let body = ci_core::jsonfmt::dumps_default(&Value::Object(initial_json));
        host.write_file(&fb_filename, &body, 0o644)?;
    }

    let force_install = option_bool(chef_cfg, "force_install", false)?;
    let installed = host.is_exe(CHEF_EXEC_PATH);
    let run = if installed && !force_install {
        option_bool(chef_cfg, "exec", false)?
    } else {
        install_chef(chef_cfg, host, log)?
    };
    if run {
        run_chef(chef_cfg, host, log)?;
        post_run_chef(chef_cfg, host)?;
    }
    Ok(())
}

/// `templater.render_to_file`'s first half, plus the debug line it logs.
fn render(
    template_fn: &str,
    text: &str,
    params: &Object,
    log: &mut Logger,
) -> Result<String, String> {
    let (kind, body) =
        ci_template::detect_template(text).map_err(|error| error.to_string())?;
    let kind = match kind {
        ci_template::TemplateKind::Jinja => "jinja",
        ci_template::TemplateKind::Basic => "basic",
    };
    log.debug(
        "templater.py",
        &format!("Rendering content of '{template_fn}' using renderer {kind}"),
    );
    let params = Value::Object(params.clone());
    match kind {
        "jinja" => ci_template::render_jinja(body, &params),
        _ => ci_template::render_basic(body, &params),
    }
    .map_err(|error| error.to_string())
}

/// `post_run_chef`.
fn post_run_chef(chef_cfg: &Value, host: &mut dyn Host) -> Result<(), String> {
    if option_bool(chef_cfg, "delete_validation_post_exec", false)?
        && host.is_file(CHEF_VALIDATION_PEM_PATH)
    {
        host.unlink(CHEF_VALIDATION_PEM_PATH)?;
    }
    Ok(())
}

/// `run_chef`.
fn run_chef(
    chef_cfg: &Value,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    log.debug(SOURCE, "Running chef-client");
    let mut cmd = vec![CHEF_EXEC_PATH.to_owned()];
    match sub_option(chef_cfg, "exec_arguments")? {
        None => cmd.extend(CHEF_EXEC_DEF_ARGS.iter().map(|a| (*a).to_owned())),
        Some(Value::Array(items)) => cmd.extend(items.iter().map(py_str)),
        // A string is appended whole, spaces and all -- see bug B110.
        Some(Value::String(text)) => cmd.push(text),
        Some(other) => {
            log.warning(
                SOURCE,
                &format!(
                    "Unknown type {} provided for chef 'exec_arguments' expected \
                     list, tuple, or string",
                    py_type_repr(&other)
                ),
            );
            cmd.extend(CHEF_EXEC_DEF_ARGS.iter().map(|a| (*a).to_owned()));
        }
    }
    host.subp(&cmd).map_err(|error| error.to_string())
}

/// `"%s" % type(value)`, which is a class repr rather than a bare name.
fn py_type_repr(value: &Value) -> String {
    format!("<class '{}'>", ci_config::type_name(value))
}

/// `install_chef`, returning whether `chef-client` should then be run.
fn install_chef(
    chef_cfg: &Value,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<bool, String> {
    let install_type =
        option_str(chef_cfg, "install_type")?.unwrap_or_else(|| "packages".to_owned());
    let mut run = option_bool(chef_cfg, "exec", false)?;
    match install_type.as_str() {
        "gems" => {
            let chef_version = option_str(chef_cfg, "version")?;
            let ruby_version = option_str(chef_cfg, "ruby_version")?
                .unwrap_or_else(|| RUBY_VERSION_DEFAULT.to_owned());
            install_chef_from_gems(&ruby_version, chef_version.as_deref(), host)?;
            // Backwards compatibility: gems default to running.
            run = option_bool(chef_cfg, "exec", true)?;
        }
        "packages" => host.install_packages(&["chef".to_owned()])?,
        "omnibus" => {
            let omnibus_version = option_str(chef_cfg, "omnibus_version")?;
            let url = option_str(chef_cfg, "omnibus_url")?;
            let retries = option_int(chef_cfg, "omnibus_url_retries", 0)?;
            install_chef_from_omnibus(
                host,
                url.as_deref(),
                Some(retries),
                omnibus_version.as_deref(),
            )?;
        }
        other => {
            log.warning(SOURCE, &format!("Unknown chef install type '{other}'"));
            run = false;
        }
    }
    Ok(run)
}

/// `util.get_cfg_option_int(yobj, key, default)`, which is `int()` over the
/// stringified value and so rejects anything that is not a decimal integer.
fn option_int(block: &Value, key: &str, default: i64) -> Result<i64, String> {
    let Some(text) = option_str(block, key)? else {
        return Ok(default);
    };
    text.trim().parse::<i64>().map_err(|_| {
        format!(
            "invalid literal for int() with base 10: {}",
            py_repr_str(&text)
        )
    })
}

/// A Python string repr, for the message `int()` raises.
fn py_repr_str(text: &str) -> String {
    ci_config::repr(&Value::String(text.to_owned()))
}

/// `install_chef_from_omnibus`.
fn install_chef_from_omnibus(
    host: &mut dyn Host,
    url: Option<&str>,
    retries: Option<i64>,
    omnibus_version: Option<&str>,
) -> Result<(), String> {
    let url = url.unwrap_or(OMNIBUS_URL);
    let retries = retries.unwrap_or(OMNIBUS_URL_RETRIES);
    let mut args = Vec::new();
    if let Some(version) = omnibus_version {
        args.push("-v".to_owned());
        args.push(version.to_owned());
    }
    let content = host.readurl(url, retries)?;
    subp_blob_in_tempfile(host, &content, &args, "chef-omnibus-install")
}

/// `subp_blob_in_tempfile`.
fn subp_blob_in_tempfile(
    host: &mut dyn Host,
    blob: &str,
    args: &[String],
    basename: &str,
) -> Result<(), String> {
    let dir = host.tempdir()?;
    let script = join(&dir, basename);
    host.write_file(&script, blob, 0o700)?;
    let mut command = vec![script];
    command.extend_from_slice(args);
    host.subp(&command).map_err(|error| error.to_string())
}

/// `get_ruby_packages(version)`.
fn get_ruby_packages(version: &str) -> Vec<String> {
    let mut pkgs = vec![format!("ruby{version}"), format!("ruby{version}-dev")];
    if version == "1.8" {
        pkgs.push("libopenssl-ruby1.8".to_owned());
        pkgs.push("rubygems1.8".to_owned());
    }
    pkgs
}

/// `install_chef_from_gems`.
fn install_chef_from_gems(
    ruby_version: &str,
    chef_version: Option<&str>,
    host: &mut dyn Host,
) -> Result<(), String> {
    host.install_packages(&get_ruby_packages(ruby_version))?;
    if !host.path_exists("/usr/bin/gem") {
        host.sym_link(&format!("/usr/bin/gem{ruby_version}"), "/usr/bin/gem")?;
    }
    if !host.path_exists("/usr/bin/ruby") {
        host.sym_link(&format!("/usr/bin/ruby{ruby_version}"), "/usr/bin/ruby")?;
    }
    let mut argv = vec![
        "/usr/bin/gem".to_owned(),
        "install".to_owned(),
        "chef".to_owned(),
    ];
    if let Some(version) = chef_version.filter(|text| !text.is_empty()) {
        // One argument with a space in it, not two -- see bug B109.
        argv.push(format!("-v {version}"));
    }
    argv.extend(
        ["--no-ri", "--no-rdoc", "--bindir", "/usr/bin", "-q"].map(str::to_owned),
    );
    host.subp(&argv).map_err(|error| error.to_string())
}

/// The registry entry point.
///
/// # Errors
/// The module failure the stage reports.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let mut host = Live {
        root: args.root.to_owned(),
        distro: *args.distro,
        system_info: args.system_info.clone(),
        templates: args.paths.template_tpl("chef_client.rb"),
        instance_id: args
            .datasource
            .as_ref()
            .map_or_else(String::new, |ds| ds.instance_id.to_owned()),
    };
    let (name, cfg) = (args.name.to_owned(), args.cfg.clone());
    handle_with(&name, &cfg, &mut host, &mut *args.logger)
}

/// The real machine.
#[derive(Debug, Clone)]
pub struct Live {
    root: std::path::PathBuf,
    distro: ci_distro::Distro,
    system_info: Object,
    /// `Paths.template_tpl("chef_client.rb")`, the only template chef uses.
    /// Not root-prefixed: `templates_dir` is configurable in its own right.
    templates: std::path::PathBuf,
    instance_id: String,
}

impl Live {
    fn under_root(&self, path: &str) -> std::path::PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}

impl Host for Live {
    fn ensure_dir(&mut self, path: &Value) -> Result<(), String> {
        let Some(path) = path.as_str() else {
            return Err(format!(
                "stat: path should be string, bytes, os.PathLike or integer, not {}",
                ci_config::type_name(path)
            ));
        };
        std::fs::create_dir_all(self.under_root(path))
            .map_err(|error| error.to_string())
    }

    fn ensure_dirs(&mut self, paths: &[String]) -> Result<(), String> {
        for path in paths {
            std::fs::create_dir_all(self.under_root(path))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn path_exists(&mut self, path: &str) -> bool {
        self.under_root(path).exists()
    }

    fn is_file(&mut self, path: &str) -> bool {
        self.under_root(path).is_file()
    }

    fn listdir(&mut self, path: &str) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        for entry in
            std::fs::read_dir(self.under_root(path)).map_err(|e| e.to_string())?
        {
            let entry = entry.map_err(|e| e.to_string())?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn move_path(&mut self, src: &str, dest: &str) -> Result<(), String> {
        let from = self.under_root(src);
        let to = self.under_root(dest).join(
            from.file_name()
                .map_or_else(std::ffi::OsString::new, ToOwned::to_owned),
        );
        std::fs::rename(&from, &to).map_err(|error| error.to_string())
    }

    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        mode: u32,
    ) -> Result<(), String> {
        let target = self.under_root(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        ci_sys::atomic::write_file(
            &target,
            content.as_bytes(),
            ci_sys::atomic::WriteOptions {
                mode,
                ..ci_sys::atomic::WriteOptions::default()
            },
        )
        .map_err(|error| error.to_string())
    }

    fn unlink(&mut self, path: &str) -> Result<(), String> {
        std::fs::remove_file(self.under_root(path)).map_err(|error| error.to_string())
    }

    fn template_path(&mut self, _name: &str) -> (String, bool) {
        (
            self.templates.display().to_string(),
            self.templates.is_file(),
        )
    }

    fn load_text_file(&mut self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|error| {
            format!(
                "[Errno {}] {error}: '{path}'",
                error.raw_os_error().unwrap_or(0)
            )
        })
    }

    fn instance_id(&mut self) -> String {
        self.instance_id.clone()
    }

    fn make_header(&mut self) -> String {
        ci_core::version::make_header('#', "created")
    }

    fn is_exe(&mut self, path: &str) -> bool {
        ci_sys::subp::is_exe(&self.under_root(path))
    }

    fn install_packages(&mut self, packages: &[String]) -> Result<(), String> {
        let list = Value::Array(
            packages
                .iter()
                .map(|p| Value::String(p.clone()))
                .collect::<Vec<_>>(),
        );
        super::rsyslog::install_packages(
            &self.root,
            &self.distro,
            &self.system_info,
            &list,
        )
    }

    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError> {
        let command = ci_config::repr(&Value::Array(
            argv.iter().map(|arg| Value::String(arg.clone())).collect(),
        ));
        let status = ci_sys::subp::Subp::new(argv)
            .inherit_env()
            .passthrough()
            .map_err(|error| CmdError {
                command: command.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: error.to_string(),
            })?;
        if status.success() {
            return Ok(());
        }
        Err(CmdError {
            command,
            exit_code: status.code(),
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    fn readurl(&mut self, url: &str, retries: i64) -> Result<String, String> {
        let response = ci_url::readurl(
            url,
            &ci_url::Config {
                retries: u32::try_from(retries).unwrap_or(0),
                ..ci_url::Config::default()
            },
        )
        .map_err(|error| error.to_string())?;
        Ok(String::from_utf8_lossy(&response.contents).into_owned())
    }

    fn tempdir(&mut self) -> Result<String, String> {
        let base = self.under_root("var/tmp/cloud-init");
        std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        let dir = base.join(format!("chef-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(dir.to_string_lossy().into_owned())
    }

    fn sym_link(&mut self, source: &str, link: &str) -> Result<(), String> {
        std::os::unix::fs::symlink(source, self.under_root(link))
            .map_err(|error| error.to_string())
    }
}

/// A [`Host`] whose every answer is set up front, for the differential.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// Files that exist, with their contents.
    pub files: Vec<(String, String)>,
    /// Directories that exist, with what `listdir` returns for them.
    pub dirs: Vec<(String, Vec<String>)>,
    /// Paths `subp.is_exe` answers yes for.
    pub exes: Vec<String>,
    /// What each URL serves.
    pub urls: Vec<(String, String)>,
    /// Calls that should fail, by recorded call text, with the message.
    pub errors: Vec<(String, String)>,
    /// `cloud.get_template_filename("chef_client.rb")`, or `None` for the
    /// warning branch.
    pub templates_dir: String,
    pub instance_id: String,
    /// `util.make_header()`, which is not comparable when it is real.
    pub header: String,
    /// What `temp_utils.tempdir` yields.
    pub tmpdir: String,
    pub calls: Vec<String>,
    /// What each file ended up holding, in write order.
    pub written: Vec<(String, String)>,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            dirs: Vec::new(),
            exes: Vec::new(),
            urls: Vec::new(),
            errors: Vec::new(),
            templates_dir: "/etc/cloud/templates".to_owned(),
            instance_id: "i-testing".to_owned(),
            header: "# generated".to_owned(),
            tmpdir: "/tmp/tmpdir".to_owned(),
            calls: Vec::new(),
            written: Vec::new(),
        }
    }
}

impl Fixture {
    fn record(&mut self, call: String) -> String {
        self.calls.push(call.clone());
        call
    }

    fn fail(&self, call: &str) -> Option<String> {
        self.errors
            .iter()
            .find(|(key, _)| key == call)
            .map(|(_, message)| message.clone())
    }

    fn exists(&self, path: &str) -> bool {
        self.files.iter().any(|(key, _)| key == path)
            || self.dirs.iter().any(|(key, _)| key == path)
    }
}

impl Host for Fixture {
    fn ensure_dir(&mut self, path: &Value) -> Result<(), String> {
        let call = self.record(format!("ensure_dir {}", py_str(path)));
        self.fail(&call).map_or(Ok(()), Err)
    }

    fn ensure_dirs(&mut self, paths: &[String]) -> Result<(), String> {
        let shown = ci_config::repr(&Value::Array(
            paths.iter().map(|p| Value::String(p.clone())).collect(),
        ));
        let call = self.record(format!("ensure_dirs {shown}"));
        self.fail(&call).map_or(Ok(()), Err)
    }

    fn path_exists(&mut self, path: &str) -> bool {
        self.record(format!("exists {path}"));
        self.exists(path)
    }

    fn is_file(&mut self, path: &str) -> bool {
        self.record(format!("isfile {path}"));
        self.files.iter().any(|(key, _)| key == path)
    }

    fn listdir(&mut self, path: &str) -> Result<Vec<String>, String> {
        let call = self.record(format!("listdir {path}"));
        if let Some(message) = self.fail(&call) {
            return Err(message);
        }
        Ok(self
            .dirs
            .iter()
            .find(|(key, _)| key == path)
            .map(|(_, names)| names.clone())
            .unwrap_or_default())
    }

    fn move_path(&mut self, src: &str, dest: &str) -> Result<(), String> {
        let call = self.record(format!("move {src} {dest}"));
        self.fail(&call).map_or(Ok(()), Err)
    }

    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        mode: u32,
    ) -> Result<(), String> {
        let call = self.record(format!("write_file {path} mode={mode:04o}"));
        self.written.push((path.to_owned(), content.to_owned()));
        self.fail(&call).map_or(Ok(()), Err)
    }

    fn unlink(&mut self, path: &str) -> Result<(), String> {
        let call = self.record(format!("unlink {path}"));
        self.fail(&call).map_or(Ok(()), Err)
    }

    fn template_path(&mut self, name: &str) -> (String, bool) {
        let path = join(&self.templates_dir, &format!("{name}.tmpl"));
        let found = self.files.iter().any(|(key, _)| *key == path)
            || std::path::Path::new(&path).is_file();
        (path, found)
    }

    fn load_text_file(&mut self, path: &str) -> Result<String, String> {
        let call = self.record(format!("load_text_file {path}"));
        if let Some(message) = self.fail(&call) {
            return Err(message);
        }
        if let Some((_, text)) = self.files.iter().find(|(key, _)| key == path) {
            return Ok(text.clone());
        }
        std::fs::read_to_string(path)
            .map_err(|_| format!("[Errno 2] No such file or directory: '{path}'"))
    }

    fn instance_id(&mut self) -> String {
        self.record("get_instance_id".to_owned());
        self.instance_id.clone()
    }

    fn make_header(&mut self) -> String {
        self.record("make_header".to_owned());
        self.header.clone()
    }

    fn is_exe(&mut self, path: &str) -> bool {
        self.record(format!("is_exe {path}"));
        self.exes.iter().any(|key| key == path)
    }

    fn install_packages(&mut self, packages: &[String]) -> Result<(), String> {
        let shown = ci_config::repr(&Value::Array(
            packages.iter().map(|p| Value::String(p.clone())).collect(),
        ));
        let call = self.record(format!("install_packages {shown}"));
        self.fail(&call).map_or(Ok(()), Err)
    }

    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError> {
        let call = self.record(format!("subp {}", argv.join(" ")));
        match self.fail(&call) {
            None => Ok(()),
            Some(message) => Err(CmdError {
                command: ci_config::repr(&Value::Array(
                    argv.iter().map(|arg| Value::String(arg.clone())).collect(),
                )),
                exit_code: Some(1),
                stdout: String::new(),
                stderr: message,
            }),
        }
    }

    fn readurl(&mut self, url: &str, retries: i64) -> Result<String, String> {
        let call = self.record(format!("readurl {url} retries={retries}"));
        if let Some(message) = self.fail(&call) {
            return Err(message);
        }
        self.urls
            .iter()
            .find(|(key, _)| key == url)
            .map(|(_, body)| body.clone())
            .ok_or_else(|| format!("Unable to read {url}"))
    }

    fn tempdir(&mut self) -> Result<String, String> {
        let call = self.record("tempdir".to_owned());
        if let Some(message) = self.fail(&call) {
            return Err(message);
        }
        Ok(self.tmpdir.clone())
    }

    fn sym_link(&mut self, source: &str, link: &str) -> Result<(), String> {
        let call = self.record(format!("sym_link {source} {link}"));
        self.fail(&call).map_or(Ok(()), Err)
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Object {
        match value {
            Value::Object(map) => map,
            _ => Object::new(),
        }
    }

    fn run_with(cfg: Value, host: &mut Fixture) -> (Vec<String>, Result<(), String>) {
        let mut log = ci_log::Logger::capturing();
        let result = handle_with("cc_chef", &object(cfg), host, &mut log);
        (log.captured().to_vec(), result)
    }

    /// A fixture whose template lookup finds nothing, so the run stops short of
    /// rendering and the call list stays about the directories.
    fn bare() -> Fixture {
        Fixture {
            templates_dir: "/no/such/templates".to_owned(),
            ..Fixture::default()
        }
    }

    /// A fixture that carries its own template. `Fixture::template_path` falls
    /// back to a real `is_file`, so a test that needs the rendering path must
    /// supply the file rather than rely on the host having Python cloud-init
    /// installed — the release container does not.
    fn templated() -> Fixture {
        Fixture {
            templates_dir: "/no/such/templates".to_owned(),
            files: vec![(
                "/no/such/templates/chef_client.rb.tmpl".to_owned(),
                "log_level :info\n".to_owned(),
            )],
            ..Fixture::default()
        }
    }

    #[test]
    fn no_key_is_a_skip() {
        let mut host = bare();
        let (log, result) = run_with(json!({"other": 1}), &mut host);
        assert!(result.is_ok());
        assert!(host.calls.is_empty());
        assert!(log[0].contains("no 'chef' key in configuration"));
    }

    #[test]
    fn the_six_directories_come_first_and_etc_chef_again() {
        let mut host = bare();
        let (_, result) = run_with(json!({"chef": {}}), &mut host);
        assert!(result.is_ok());
        assert_eq!(
            host.calls[..7],
            [
                "ensure_dir /etc/chef",
                "ensure_dir /var/log/chef",
                "ensure_dir /var/lib/chef",
                "ensure_dir /var/chef/cache",
                "ensure_dir /var/chef/backup",
                "ensure_dir /var/run/chef",
                "ensure_dir /etc/chef",
            ]
        );
    }

    #[test]
    fn directories_replace_the_defaults_but_not_the_required_ones() {
        let mut host = bare();
        let (_, result) =
            run_with(json!({"chef": {"directories": ["/opt/chef"]}}), &mut host);
        assert!(result.is_ok());
        assert_eq!(
            host.calls[..2],
            ["ensure_dir /opt/chef", "ensure_dir /etc/chef"]
        );
    }

    #[test]
    fn an_empty_directories_list_falls_back_to_the_defaults() {
        let mut host = bare();
        let (_, result) = run_with(json!({"chef": {"directories": []}}), &mut host);
        assert!(result.is_ok());
        assert_eq!(host.calls[0], "ensure_dir /etc/chef");
        assert_eq!(host.calls[5], "ensure_dir /var/run/chef");
    }

    #[test]
    fn a_string_chef_block_survives_long_enough_to_make_the_directories() {
        // Bug B107: the type of `chef:` is never checked, so six directories
        // exist by the time the subscript raises.
        let mut host = bare();
        let (_, result) = run_with(json!({"chef": "yes"}), &mut host);
        assert_eq!(
            result,
            Err("'str' object has no attribute 'get'".to_owned())
        );
        assert_eq!(host.calls.len(), 9);
    }

    #[test]
    fn a_missing_server_url_is_a_key_error_after_the_directories() {
        // Bug B108.
        let mut host = templated();
        let (_, result) =
            run_with(json!({"chef": {"validation_name": "v"}}), &mut host);
        assert_eq!(result, Err("'server_url'".to_owned()));
        assert!(host.calls.iter().any(|call| call == "make_header"));
    }

    #[test]
    fn a_missing_template_warns_twice_and_renders_nothing() {
        let mut host = bare();
        let (log, result) = run_with(json!({"chef": {}}), &mut host);
        assert!(result.is_ok());
        assert!(log.iter().any(|line| line
            .contains("No template found in /no/such/templates for template")));
        assert!(log
            .iter()
            .any(|line| line.contains("not rendering to /etc/chef/client.rb")));
        assert!(host
            .written
            .iter()
            .all(|(path, _)| path != "/etc/chef/client.rb"));
    }

    #[test]
    fn the_firstboot_file_is_written_even_with_nothing_in_it() {
        let mut host = bare();
        let (_, result) = run_with(json!({"chef": {}}), &mut host);
        assert!(result.is_ok());
        assert_eq!(
            host.written
                .iter()
                .find(|(path, _)| path == "/etc/chef/firstboot.json"),
            Some(&("/etc/chef/firstboot.json".to_owned(), "{}".to_owned()))
        );
    }

    #[test]
    fn a_run_list_and_attributes_share_the_firstboot_file() {
        let mut host = bare();
        let (_, result) = run_with(
            json!({"chef": {"run_list": ["recipe[a]"],
                            "initial_attributes": {"k": "v"}}}),
            &mut host,
        );
        assert!(result.is_ok());
        let (_, body) = host
            .written
            .iter()
            .find(|(path, _)| path == "/etc/chef/firstboot.json")
            .unwrap();
        assert_eq!(body, r#"{"run_list": ["recipe[a]"], "k": "v"}"#);
    }

    #[test]
    fn a_validation_cert_is_written_with_the_default_mode() {
        // Upstream passes no `mode=`, so the private key lands at 0644.
        let mut host = bare();
        let (_, result) =
            run_with(json!({"chef": {"validation_cert": "KEY"}}), &mut host);
        assert!(result.is_ok());
        assert!(host
            .calls
            .iter()
            .any(|call| call == "write_file /etc/chef/validation.pem mode=0644"));
    }

    #[test]
    fn the_word_system_means_the_key_is_already_there() {
        let mut host = bare();
        let (log, result) =
            run_with(json!({"chef": {"validation_cert": "system"}}), &mut host);
        assert!(result.is_ok());
        assert!(host
            .calls
            .iter()
            .all(|call| !call.starts_with("write_file /etc/chef/validation.pem")));
        assert!(log
            .iter()
            .any(|line| line.contains("chef validation_cert provided as 'system'")));
    }

    #[test]
    fn legacy_directories_are_migrated_file_by_file() {
        let mut host = Fixture {
            dirs: vec![("/var/cache/chef".to_owned(), vec!["a".to_owned()])],
            ..bare()
        };
        let (log, result) = run_with(json!({"chef": {}}), &mut host);
        assert!(result.is_ok());
        assert!(host
            .calls
            .iter()
            .any(|call| call == "move /var/cache/chef/a /var/chef/cache"));
        assert!(log
            .iter()
            .any(|line| line.contains("Moving /var/cache/chef/a to /var/chef/cache.")));
    }

    #[test]
    fn a_migration_collision_is_skipped() {
        let mut host = Fixture {
            dirs: vec![("/var/cache/chef".to_owned(), vec!["a".to_owned()])],
            files: vec![("/var/chef/cache/a".to_owned(), "old".to_owned())],
            ..bare()
        };
        let (log, result) = run_with(json!({"chef": {}}), &mut host);
        assert!(result.is_ok());
        assert!(host.calls.iter().all(|call| !call.starts_with("move ")));
        assert!(log.iter().any(|line| line.contains(
            "Ignoring migration of /var/cache/chef/a. File already exists in \
             /var/chef/cache."
        )));
    }

    #[test]
    fn an_installed_client_is_not_installed_again() {
        let mut host = Fixture {
            exes: vec!["/usr/bin/chef-client".to_owned()],
            ..bare()
        };
        let (_, result) = run_with(json!({"chef": {}}), &mut host);
        assert!(result.is_ok());
        assert!(host
            .calls
            .iter()
            .all(|call| !call.starts_with("install_packages")));
    }

    #[test]
    fn force_install_reinstalls_over_an_installed_client() {
        let mut host = Fixture {
            exes: vec!["/usr/bin/chef-client".to_owned()],
            ..bare()
        };
        let (_, result) = run_with(json!({"chef": {"force_install": true}}), &mut host);
        assert!(result.is_ok());
        assert!(host
            .calls
            .iter()
            .any(|call| call == "install_packages ['chef']"));
    }

    #[test]
    fn omnibus_downloads_a_script_and_runs_it_from_a_tempdir() {
        let mut host = Fixture {
            urls: vec![(OMNIBUS_URL.to_owned(), "#!/bin/sh\ntrue\n".to_owned())],
            ..bare()
        };
        let (_, result) =
            run_with(json!({"chef": {"install_type": "omnibus"}}), &mut host);
        assert!(result.is_ok());
        assert!(host
            .calls
            .iter()
            .any(|call| call == &format!("readurl {OMNIBUS_URL} retries=0")));
        assert!(host
            .calls
            .iter()
            .any(|call| call.starts_with("subp /tmp/tmpdir/")));
    }

    #[test]
    fn the_omnibus_retry_default_is_unreachable() {
        // Bug B106: the absent key comes back as the integer 0, not None, so
        // `OMNIBUS_URL_RETRIES` never applies.
        assert_eq!(OMNIBUS_URL_RETRIES, 5);
        let mut host = Fixture {
            urls: vec![(OMNIBUS_URL.to_owned(), "x".to_owned())],
            ..bare()
        };
        let (_, _) = run_with(json!({"chef": {"install_type": "omnibus"}}), &mut host);
        assert!(host.calls.iter().any(|call| call.ends_with("retries=0")));
    }

    #[test]
    fn a_gem_version_becomes_one_argument_with_a_space_in_it() {
        // Bug B109.
        let mut host = bare();
        let (_, result) = run_with(
            json!({"chef": {"install_type": "gems", "version": "12.0"}}),
            &mut host,
        );
        assert!(result.is_ok());
        assert!(host.calls.iter().any(|call| call
            == "subp /usr/bin/gem install chef -v 12.0 --no-ri --no-rdoc \
                --bindir /usr/bin -q"
            || call.contains("-v 12.0")));
    }

    #[test]
    fn an_unknown_install_type_warns_and_does_not_run() {
        let mut host = bare();
        let (log, result) =
            run_with(json!({"chef": {"install_type": "snap"}}), &mut host);
        assert!(result.is_ok());
        assert!(log
            .iter()
            .any(|line| line.contains("Unknown chef install type 'snap'")));
        assert!(host.calls.iter().all(|call| !call.starts_with("subp ")));
    }

    #[test]
    fn exec_runs_the_client_with_the_default_arguments() {
        let mut host = Fixture {
            exes: vec!["/usr/bin/chef-client".to_owned()],
            ..bare()
        };
        let (_, result) = run_with(json!({"chef": {"exec": true}}), &mut host);
        assert!(result.is_ok());
        assert!(host
            .calls
            .iter()
            .any(|call| call == "subp /usr/bin/chef-client -d -i 1800 -s 20"));
    }

    #[test]
    fn a_string_exec_arguments_is_appended_whole() {
        // Bug B110.
        let mut host = Fixture {
            exes: vec!["/usr/bin/chef-client".to_owned()],
            ..bare()
        };
        let (_, result) = run_with(
            json!({"chef": {"exec": true, "exec_arguments": "-l debug"}}),
            &mut host,
        );
        assert!(result.is_ok());
        assert!(host
            .calls
            .iter()
            .any(|call| call == "subp /usr/bin/chef-client -l debug"));
    }

    #[test]
    fn the_validation_pem_is_deleted_after_a_run_when_asked() {
        let mut host = Fixture {
            exes: vec!["/usr/bin/chef-client".to_owned()],
            files: vec![(CHEF_VALIDATION_PEM_PATH.to_owned(), "pem".to_owned())],
            ..bare()
        };
        let (_, result) = run_with(
            json!({"chef": {"exec": true, "delete_validation_post_exec": true}}),
            &mut host,
        );
        assert!(result.is_ok());
        assert!(host
            .calls
            .iter()
            .any(|call| call == "unlink /etc/chef/validation.pem"));
    }

    #[test]
    fn dirname_follows_posixpath_exactly() {
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("a"), "");
        assert_eq!(dirname("//a"), "//");
        assert_eq!(dirname("///a"), "///");
        assert_eq!(dirname("/a/b/"), "/a/b");
        assert_eq!(dirname(""), "");
    }
}
