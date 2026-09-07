//! Port of `sources.DataSourceNoCloud` and `DataSourceNoCloudNet`.
//!
//! `NoCloud` is seeded from the filesystem — a seed directory, the kernel command
//! line, or the `datasource: NoCloud:` config block — which makes it the one
//! datasource that can be exercised without a cloud.
//!
//! Two seeding routes are not here yet: the `cidata` filesystem label, which
//! needs block-device probing, and `seedfrom` over `ftp`. `http` and `https`
//! are implemented, as is the local variant's `file://` and absolute-path
//! `seedfrom`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ci_config::{Object, Value};

use crate::types::{Context, Datasource, DsMode, Probe, METADATA_UNKNOWN};

/// `s2l` — the short spellings the kernel command line accepts.
const SHORT_KEYS: [(&str, &str); 3] = [
    ("h", "local-hostname"),
    ("i", "instance-id"),
    ("s", "seedfrom"),
];

/// `DataSourceNoCloud`, and its `DataSourceNoCloudNet` subclass.
#[derive(Debug)]
pub struct NoCloud {
    net: bool,
}

impl NoCloud {
    /// `DataSourceNoCloud`, the `DEP_FILESYSTEM` variant.
    #[must_use]
    pub fn local() -> Self {
        Self { net: false }
    }

    /// `DataSourceNoCloudNet`, the `DEP_FILESYSTEM + DEP_NETWORK` variant.
    #[must_use]
    pub fn net() -> Self {
        Self { net: true }
    }

    /// `supported_seed_starts`.
    fn seed_starts(&self) -> &'static [&'static str] {
        if self.net {
            &["http://", "https://", "ftp://", "ftps://"]
        } else {
            &["/", "file://"]
        }
    }

    /// `self.seed_dirs`.
    fn seed_dirs(paths: &ci_core::Paths) -> [PathBuf; 2] {
        [
            paths.seed_dir().join("nocloud"),
            paths.seed_dir().join("nocloud-net"),
        ]
    }
}

impl Probe for NoCloud {
    fn dsname(&self) -> &'static str {
        "NoCloud"
    }

    fn class_name(&self) -> &'static str {
        if self.net {
            "DataSourceNoCloudNet"
        } else {
            "DataSourceNoCloud"
        }
    }

    fn display(&self) -> String {
        // `__str__` joins the class name to its bracket groups with a space
        // that is emitted whether or not any group follows it.
        format!("{} ", self.class_name())
    }

    /// `DataSourceNoCloudNet.ds_detect`. The local variant inherits the base
    /// class's unconditional yes.
    fn ds_detect(&self, ctx: &mut Context<'_>) -> bool {
        if !self.net {
            return true;
        }
        if crate::search::parse_cmdline_or_dmi(ctx.cmdline) == "nocloud-net" {
            deprecate_nocloud_net(ctx.logger);
            return true;
        }
        let serial = crate::dmi::read_dmi_data("system-serial-number", ctx.logger)
            .unwrap_or_default();
        let serial = crate::search::parse_cmdline_or_dmi(&serial).to_lowercase();
        if serial == "nocloud" || serial == "nocloud-net" {
            ctx.logger.debug(
                "DataSourceNoCloud.py",
                &format!(
                    "Machine is configured by dmi serial number to run on \
                     single datasource {}.",
                    self.display()
                ),
            );
            if serial == "nocloud-net" {
                deprecate_nocloud_net(ctx.logger);
            }
            return true;
        }
        if ctx.ds_cfg(self.dsname()).contains_key("seedfrom") {
            ctx.logger.debug(
                "DataSourceNoCloud.py",
                &format!(
                    "Machine is configured by system configuration to run on \
                     single datasource {}.",
                    self.display()
                ),
            );
            return true;
        }
        false
    }

    fn get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource> {
        let ds_cfg = ctx.ds_cfg(self.dsname());
        let (mut found, mut crawl) = Self::gather(ctx, &ds_cfg);

        if found.is_empty() {
            return None;
        }

        if let Some(seedfrom) = crawl
            .metadata
            .get("seedfrom")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        {
            if !self
                .seed_starts()
                .iter()
                .any(|proto| seedfrom.starts_with(proto))
            {
                self.log_unusable_seedfrom(ctx, &seedfrom);
                return None;
            }
            let seedfrom = crate::dmi::sub_dmi_vars(&seedfrom, ctx.logger);
            let seeded = read_seeded(&seedfrom, ctx.logger)?;
            ctx.logger.debug(
                "DataSourceNoCloud.py",
                &format!("Using seeded cache data from {seedfrom}"),
            );
            crawl.merge_seedfrom(&seeded, ctx);
            found.push(seedfrom);
        }

        crawl.metadata = ci_config::merge::merge_many(
            vec![std::mem::take(&mut crawl.metadata), defaults()],
            false,
        );

        let dsmode = DsMode::determine(
            &[crawl.metadata.get("dsmode")],
            DsMode::Network,
            ctx.logger,
        );
        if dsmode == DsMode::Disabled {
            ctx.logger.debug(
                "DataSourceNoCloud.py",
                &format!(
                    "{}: not claiming datasource, dsmode={dsmode}",
                    self.display()
                ),
            );
            return None;
        }

        let seed = found.join(",");
        let instance_id = crawl
            .metadata
            .get("instance-id")
            .map_or_else(|| "iid-datasource".to_owned(), scalar_string);
        Some(Datasource {
            class_name: self.class_name(),
            dsname: self.dsname(),
            dsmode,
            instance_id,
            platform_type: platform_type(),
            subplatform: subplatform(&seed),
            // `_get_cloud_name` is overridden to say it does not know, so the
            // `cloud-name` metadata key is the only thing that can name it.
            cloud_name_default: METADATA_UNKNOWN.to_owned(),
            detail: detail(&seed, dsmode),
            metadata: crawl.metadata,
            userdata_raw: Some(crawl.user_data),
            vendordata_raw: Some(crawl.vendor_data),
            vendordata2_raw: None,
            network_config: crawl.network_config,
        })
    }

    /// `check_instance_id` — answered from the command line and the seed
    /// directories only, so it costs nothing to run on every boot.
    fn check_instance_id(&self, ctx: &mut Context<'_>, current: &str) -> Option<bool> {
        if current.is_empty() {
            return None;
        }
        let quick = quick_read_instance_id(ctx)?;
        Some(quick == current)
    }
}

impl NoCloud {
    /// Every seed source `_get_data` consults, in upstream's order, and what
    /// each contributed.
    fn gather(ctx: &mut Context<'_>, ds_cfg: &Object) -> (Vec<String>, Crawl) {
        let mut found: Vec<String> = Vec::new();
        let mut crawl = Crawl::default();

        // The system serial number is read as if it were a command line, so a
        // hypervisor can seed an instance without touching its disk.
        let serial = crate::dmi::read_dmi_data("system-serial-number", ctx.logger)
            .unwrap_or_default();
        let mut from_dmi = Object::new();
        if !serial.is_empty() && load_cmdline_data(&mut from_dmi, &serial) {
            found.push("dmi".to_owned());
            crawl.merge(&Seed::from_metadata(Meta::Parsed(from_dmi)), ctx);
        }

        let mut from_cmdline = Object::new();
        if load_cmdline_data(&mut from_cmdline, ctx.cmdline) {
            found.push("cmdline".to_owned());
            crawl.merge(&Seed::from_metadata(Meta::Parsed(from_cmdline)), ctx);
        }

        for dir in NoCloud::seed_dirs(ctx.paths) {
            if let Some(seed) = read_seed_dir(&dir) {
                ctx.logger.debug(
                    "DataSourceNoCloud.py",
                    &format!("Using seeded data from {}", dir.display()),
                );
                found.push(dir.display().to_string());
                crawl.merge(&seed, ctx);
                break;
            }
        }

        // A `seedfrom` in the datasource config outranks one found on disk.
        if let Some(seedfrom) = ds_cfg.get("seedfrom").and_then(Value::as_str) {
            found.push("ds_config_seedfrom".to_owned());
            crawl
                .metadata
                .insert("seedfrom".to_owned(), Value::from(seedfrom));
        }

        if ds_cfg.contains_key("user-data") && ds_cfg.contains_key("meta-data") {
            crawl.merge(&Seed::from_config(ds_cfg), ctx);
            found.push("ds_config".to_owned());
        }

        (found, crawl)
    }

    /// `_log_unusable_seedfrom`, which the two variants disagree about: the
    /// local one expects the network stage to pick the seed up, the network one
    /// knows nothing else will.
    fn log_unusable_seedfrom(&self, ctx: &mut Context<'_>, seedfrom: &str) {
        let starts = self
            .seed_starts()
            .iter()
            .map(|proto| format!("'{proto}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let name = self.display();
        if self.net {
            ctx.logger.warning(
                "DataSourceNoCloud.py",
                &format!(
                    "{name} only uses seeds starting with ({starts}) - \
                     {seedfrom} is not valid."
                ),
            );
        } else {
            ctx.logger.info(
                "DataSourceNoCloud.py",
                &format!(
                    "{name} only uses seeds starting with ({starts}) - will try \
                     to use {seedfrom} in the network stage."
                ),
            );
        }
    }
}

/// `log_deprecated`, the `lifecycle.deprecate` partial both `ds_detect`
/// branches share. The missing space in `protocolscheme` is upstream's
/// (bug B36).
fn deprecate_nocloud_net(logger: &mut ci_log::Logger) {
    logger.log(
        ci_log::Level::Deprecated,
        "lifecycle.py",
        "The 'nocloud-net' datasource name is deprecated in 24.1 and scheduled \
         to be removed in 29.1. Use 'nocloud' instead, which uses the seedfrom \
         protocolscheme (http:// or file://) to decide how to run.",
    );
}

/// `mydata`, the accumulator `_merge_new_seed` folds each source into.
#[derive(Debug, Default)]
struct Crawl {
    metadata: Object,
    user_data: Vec<u8>,
    vendor_data: Vec<u8>,
    network_config: Option<Value>,
}

impl Crawl {
    /// `_merge_new_seed`. Metadata accumulates first-wins; user and vendor data
    /// are replaced outright by any source that carries them.
    fn merge(&mut self, seed: &Seed, ctx: &mut Context<'_>) {
        let new = match &seed.meta {
            Meta::Parsed(map) => map.clone(),
            Meta::Raw(bytes) => load_object(bytes, ctx),
        };
        self.metadata = ci_config::merge::merge_many(
            vec![std::mem::take(&mut self.metadata), new],
            false,
        );
        if let Some(bytes) = &seed.network_config {
            if !bytes.is_empty() {
                self.network_config = load_value(bytes, ctx);
            }
        }
        if let Some(bytes) = &seed.user_data {
            self.user_data.clone_from(bytes);
        }
        if let Some(bytes) = &seed.vendor_data {
            self.vendor_data.clone_from(bytes);
        }
    }

    /// The `seedfrom` block, which replaces user, vendor and network data
    /// unconditionally rather than only when the source carries them.
    fn merge_seedfrom(&mut self, seed: &Seed, ctx: &mut Context<'_>) {
        let new = match &seed.meta {
            Meta::Parsed(map) => map.clone(),
            Meta::Raw(bytes) => load_object(bytes, ctx),
        };
        self.metadata = ci_config::merge::merge_many(
            vec![std::mem::take(&mut self.metadata), new],
            false,
        );
        self.user_data = seed.user_data.clone().unwrap_or_default();
        self.vendor_data = seed.vendor_data.clone().unwrap_or_default();
        self.network_config = seed
            .network_config
            .as_ref()
            .and_then(|bytes| load_value(bytes, ctx));
    }
}

/// Metadata as a source supplied it: already a mapping, or bytes still to be
/// parsed.
#[derive(Debug)]
pub enum Meta {
    Parsed(Object),
    Raw(Vec<u8>),
}

/// One source's contribution, shaped like `util.pathprefix2dict`'s result.
#[derive(Debug)]
pub struct Seed {
    pub meta: Meta,
    pub user_data: Option<Vec<u8>>,
    pub vendor_data: Option<Vec<u8>>,
    pub network_config: Option<Vec<u8>>,
}

impl Seed {
    fn from_metadata(meta: Meta) -> Self {
        Self {
            meta,
            user_data: None,
            vendor_data: None,
            network_config: None,
        }
    }

    /// The `datasource: NoCloud:` block used as a seed in its own right.
    fn from_config(ds_cfg: &Object) -> Self {
        Self {
            meta: ds_cfg.get("meta-data").map_or_else(
                || Meta::Parsed(Object::new()),
                |value| match value {
                    Value::Object(map) => Meta::Parsed(map.clone()),
                    Value::String(text) => Meta::Raw(text.clone().into_bytes()),
                    _ => Meta::Parsed(Object::new()),
                },
            ),
            // Only strings can carry user or vendor data; upstream stores
            // anything else and fails later in the user-data pipeline.
            user_data: config_bytes(ds_cfg, "user-data"),
            vendor_data: config_bytes(ds_cfg, "vendor-data"),
            network_config: None,
        }
    }
}

fn config_bytes(ds_cfg: &Object, key: &str) -> Option<Vec<u8>> {
    ds_cfg
        .get(key)
        .and_then(Value::as_str)
        .map(|text| text.as_bytes().to_vec())
}

/// `defaults` — merged in last, so anything already found outranks them.
fn defaults() -> Object {
    let mut map = Object::new();
    map.insert("instance-id".to_owned(), Value::from("nocloud"));
    // The class attribute, which `_get_data` has not touched at this point.
    map.insert("dsmode".to_owned(), Value::from(DsMode::Network.as_str()));
    map
}

/// `platform_type` — LXD seeds `NoCloud`, and says so.
fn platform_type() -> String {
    if Path::new("/dev/lxd/sock").exists() {
        "lxd".to_owned()
    } else {
        "nocloud".to_owned()
    }
}

/// `_get_subplatform`.
fn subplatform(seed: &str) -> String {
    let kind = if seed.starts_with("/dev") {
        "config-disk"
    } else {
        "seed-dir"
    };
    format!("{kind} ({seed})")
}

/// The `__str__` suffix: the separator is unconditional, the bracket groups are
/// not.
fn detail(seed: &str, dsmode: DsMode) -> String {
    let mut out = String::from(" ");
    if !seed.is_empty() {
        let _ = write!(out, "[seed={seed}]");
    }
    if dsmode != DsMode::Network {
        let _ = write!(out, "[dsmode={dsmode}]");
    }
    out
}

/// `get_instance_id`: whatever the key holds, rendered the way `str()` would.
fn scalar_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        other => other.to_string(),
    }
}

/// `util.pathprefix2dict(path, required=["user-data", "meta-data"],
/// optional=["vendor-data", "network-config"])`.
fn read_seed_dir(base: &Path) -> Option<Seed> {
    let meta = std::fs::read(base.join("meta-data")).ok()?;
    let user = std::fs::read(base.join("user-data")).ok()?;
    Some(Seed {
        meta: Meta::Raw(meta),
        user_data: Some(user),
        vendor_data: std::fs::read(base.join("vendor-data")).ok(),
        network_config: std::fs::read(base.join("network-config")).ok(),
    })
}

/// `util.read_seeded`.
///
/// `meta-data` and `user-data` are required; upstream lets their read errors
/// escape `_get_data`, which `find_source` then logs and treats as "this
/// datasource did not match".
pub fn read_seeded(seedfrom: &str, logger: &mut ci_log::Logger) -> Option<Seed> {
    read_seeded_with(seedfrom, &seed_fetch_config(), logger)
}

/// `read_seeded` with the fetch budget spelled out, as upstream's keyword
/// arguments allow.
pub fn read_seeded_with(
    seedfrom: &str,
    config: &ci_url::Config,
    logger: &mut ci_log::Logger,
) -> Option<Seed> {
    let url = |name: &str| -> String {
        if seedfrom.contains("%s") {
            seedfrom.replace("%s", name)
        } else if seedfrom.ends_with('/') || has_query(seedfrom) {
            format!("{seedfrom}{name}")
        } else {
            // `NOCLOUD_SEED_URL_APPEND_FORWARD_SLASH`.
            format!("{seedfrom}/{name}")
        }
    };

    let mut network_config = None;
    match ci_url::read_file_or_url(&url("network-config"), config) {
        Ok(resp) => {
            if resp.ok() {
                network_config = Some(resp.contents);
            }
        }
        Err(err) => {
            let message = format!("No network config provided: {err}");
            logger.debug("util.py", &message);
        }
    }

    let meta = ci_url::read_file_or_url(&url("meta-data"), config).ok()?;
    let user = ci_url::read_file_or_url(&url("user-data"), config).ok()?;

    let mut vendor_data = None;
    match ci_url::read_file_or_url(&url("vendor-data"), config) {
        Ok(resp) => {
            if resp.ok() {
                vendor_data = Some(resp.contents);
            } else {
                logger.debug("util.py", "Error in vendor-data response");
            }
        }
        Err(err) => {
            let message = format!("Error in vendor-data response: {err}");
            logger.debug("util.py", &message);
        }
    }

    Some(Seed {
        meta: Meta::Raw(meta.contents),
        user_data: Some(user.contents),
        vendor_data,
        network_config,
    })
}

/// Upstream passes `timeout=None` here, which never times out (bug B37); the
/// port keeps the retry budget and applies `read_seeded`'s own default.
fn seed_fetch_config() -> ci_url::Config {
    ci_url::Config {
        timeout: std::time::Duration::from_secs(5),
        retries: 10,
        ..ci_url::Config::default()
    }
}

/// Whether `urlparse` would report a non-empty query, which suppresses the
/// trailing slash.
fn has_query(base: &str) -> bool {
    let before_fragment = base.split('#').next().unwrap_or("");
    before_fragment
        .split_once('?')
        .is_some_and(|(_, query)| !query.is_empty())
}

/// `_quick_read_instance_id`.
fn quick_read_instance_id(ctx: &mut Context<'_>) -> Option<String> {
    let mut fill = Object::new();
    if load_cmdline_data(&mut fill, ctx.cmdline) {
        if let Some(iid) = fill.get("instance-id") {
            return Some(scalar_string(iid));
        }
    }
    for dir in NoCloud::seed_dirs(ctx.paths) {
        let Ok(meta) = std::fs::read(dir.join("meta-data")) else {
            continue;
        };
        let map = load_object(&meta, ctx);
        if let Some(iid) = map.get("instance-id") {
            return Some(scalar_string(iid));
        }
    }
    None
}

/// `load_cmdline_data`.
fn load_cmdline_data(fill: &mut Object, cmdline: &str) -> bool {
    for (idstr, dsmode) in [
        ("ds=nocloud", DsMode::Local),
        ("ds=nocloud-net", DsMode::Network),
    ] {
        if !parse_cmdline_data(idstr, fill, cmdline) {
            continue;
        }
        if fill.contains_key("dsmode") {
            return true;
        }
        match fill.get("seedfrom").and_then(Value::as_str) {
            Some(seedfrom) => {
                let inferred = if seedfrom.starts_with("http://")
                    || seedfrom.starts_with("https://")
                    || seedfrom.starts_with("ftp://")
                    || seedfrom.starts_with("ftps://")
                {
                    Some(DsMode::Network)
                } else if seedfrom.starts_with("file://") || seedfrom.starts_with('/') {
                    Some(DsMode::Local)
                } else {
                    None
                };
                if let Some(mode) = inferred {
                    fill.insert("dsmode".to_owned(), Value::from(mode.as_str()));
                }
            }
            None => {
                fill.insert("dsmode".to_owned(), Value::from(dsmode.as_str()));
            }
        }
        return true;
    }
    false
}

/// `parse_cmdline_data`: `ds=nocloud[;key=val;key=val]`.
fn parse_cmdline_data(ds_id: &str, fill: &mut Object, cmdline: &str) -> bool {
    let padded = format!(" {cmdline} ");
    if !padded.contains(&format!(" {ds_id} "))
        && !padded.contains(&format!(" {ds_id};"))
    {
        return false;
    }
    // The last matching token wins, because upstream keeps assigning.
    let Some(argline) = padded
        .split_whitespace()
        .rev()
        .find(|tok| tok.starts_with(ds_id))
        .and_then(|tok| tok.split_once('='))
        .map(|(_, rest)| rest)
    else {
        return false;
    };
    for item in argline.split(';').skip(1) {
        if item.is_empty() {
            continue;
        }
        let (key, value) = match item.split_once('=') {
            Some((key, value)) => (key, Value::from(value)),
            None => (item, Value::Null),
        };
        let key = SHORT_KEYS
            .iter()
            .find(|(short, _)| *short == key)
            .map_or(key, |(_, long)| *long);
        fill.insert(key.to_owned(), value);
    }
    true
}

/// `util.load_yaml(blob)` restricted to mappings, as the seed parsers use it.
fn load_object(bytes: &[u8], ctx: &mut Context<'_>) -> Object {
    load_value(bytes, ctx)
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

fn load_value(bytes: &[u8], ctx: &mut Context<'_>) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?;
    match ci_config::load_yaml(text, ctx.limits) {
        Ok(Value::Null) => None,
        Ok(value) => Some(value),
        Err(err) => {
            ctx.logger
                .warning("util.py", &format!("Failed loading yaml blob. {err}"));
            None
        }
    }
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
    use ci_core::Paths;
    use ci_log::Logger;

    struct Fixture {
        _root: tempfile::TempDir,
        paths: Paths,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: root.path().join("var/lib/cloud"),
            run_dir: root.path().join("run/cloud-init"),
            ..Paths::default()
        };
        Fixture { _root: root, paths }
    }

    fn seed(paths: &Paths, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = paths.seed_dir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, content) in files {
            std::fs::write(dir.join(file), content).unwrap();
        }
        dir
    }

    fn crawl(
        paths: &Paths,
        sys_cfg: &Object,
        cmdline: &str,
        net: bool,
    ) -> Option<Datasource> {
        let mut logger = Logger::silent();
        let mut reporter = ci_report::Reporter::silent();
        let mut ctx = Context {
            sys_cfg,
            paths,
            cmdline,
            limits: ci_config::Limits::default(),
            logger: &mut logger,
            reporter: &mut reporter,
        };
        let probe = if net {
            NoCloud::net()
        } else {
            NoCloud::local()
        };
        probe.get_data(&mut ctx)
    }

    #[test]
    fn nothing_on_disk_and_nothing_on_the_command_line_means_no_datasource() {
        let f = fixture();
        assert!(crawl(&f.paths, &Object::new(), "ro quiet", false).is_none());
    }

    #[test]
    fn a_seed_directory_supplies_the_instance_id_and_the_user_data() {
        let f = fixture();
        let dir = seed(
            &f.paths,
            "nocloud",
            &[
                (
                    "meta-data",
                    "instance-id: iid-local01\nlocal-hostname: me\n",
                ),
                ("user-data", "#cloud-config\n"),
            ],
        );
        let found = crawl(&f.paths, &Object::new(), "", false).unwrap();
        assert_eq!(found.instance_id, "iid-local01");
        assert_eq!(
            found.userdata_raw.as_deref(),
            Some(b"#cloud-config\n".as_ref())
        );
        assert_eq!(found.subplatform, format!("seed-dir ({})", dir.display()));
        assert_eq!(found.dsmode, DsMode::Network);
        assert_eq!(found.cloud_name(), "unknown");
        assert_eq!(
            found.record(),
            format!(
                "DataSourceNoCloud: DataSourceNoCloud [seed={}]",
                dir.display()
            )
        );
    }

    #[test]
    fn the_first_seed_directory_that_answers_wins() {
        let f = fixture();
        seed(
            &f.paths,
            "nocloud",
            &[("meta-data", "instance-id: first\n"), ("user-data", "a")],
        );
        seed(
            &f.paths,
            "nocloud-net",
            &[("meta-data", "instance-id: second\n"), ("user-data", "b")],
        );
        let found = crawl(&f.paths, &Object::new(), "", false).unwrap();
        assert_eq!(found.instance_id, "first");
    }

    #[test]
    fn a_seed_directory_without_user_data_is_not_a_seed() {
        let f = fixture();
        seed(&f.paths, "nocloud", &[("meta-data", "instance-id: x\n")]);
        assert!(crawl(&f.paths, &Object::new(), "", false).is_none());
    }

    #[test]
    fn the_command_line_outranks_the_seed_directory() {
        let f = fixture();
        seed(
            &f.paths,
            "nocloud",
            &[("meta-data", "instance-id: from-disk\n"), ("user-data", "")],
        );
        let found = crawl(
            &f.paths,
            &Object::new(),
            "ro ds=nocloud;i=from-cmdline",
            false,
        )
        .unwrap();
        assert_eq!(found.instance_id, "from-cmdline");
        assert_eq!(found.dsmode, DsMode::Local);
    }

    #[test]
    fn the_short_command_line_keys_expand_to_their_metadata_names() {
        let mut fill = Object::new();
        assert!(load_cmdline_data(
            &mut fill,
            "ro ds=nocloud;h=host;i=iid;s=/seed"
        ));
        assert_eq!(fill["local-hostname"], Value::from("host"));
        assert_eq!(fill["instance-id"], Value::from("iid"));
        assert_eq!(fill["seedfrom"], Value::from("/seed"));
        // A local `seedfrom` implies the local dsmode.
        assert_eq!(fill["dsmode"], Value::from("local"));
    }

    #[test]
    fn ds_nocloud_net_is_not_matched_by_the_ds_nocloud_prefix() {
        let mut fill = Object::new();
        assert!(!parse_cmdline_data(
            "ds=nocloud",
            &mut fill,
            "ro ds=nocloud-net"
        ));
        assert!(parse_cmdline_data(
            "ds=nocloud-net",
            &mut fill,
            "ro ds=nocloud-net"
        ));
    }

    #[test]
    fn a_local_seedfrom_is_followed_but_only_by_the_local_variant() {
        let f = fixture();
        let elsewhere = f.paths.cloud_dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("meta-data"), "instance-id: seeded\n").unwrap();
        std::fs::write(elsewhere.join("user-data"), "from-seedfrom").unwrap();
        let cmdline = format!("ro ds=nocloud;s={}", elsewhere.display());
        let found = crawl(&f.paths, &Object::new(), &cmdline, false).unwrap();
        assert_eq!(found.instance_id, "seeded");
        assert_eq!(
            found.userdata_raw.as_deref(),
            Some(b"from-seedfrom".as_ref())
        );
        assert!(crawl(&f.paths, &Object::new(), &cmdline, true).is_none());
    }

    #[test]
    fn a_disabled_dsmode_makes_the_datasource_stand_down() {
        let f = fixture();
        seed(
            &f.paths,
            "nocloud",
            &[
                ("meta-data", "instance-id: x\ndsmode: disabled\n"),
                ("user-data", ""),
            ],
        );
        assert!(crawl(&f.paths, &Object::new(), "", false).is_none());
    }

    #[test]
    fn the_datasource_config_can_carry_the_whole_seed() {
        let f = fixture();
        let sys_cfg: Object = serde_json::from_str(
            r##"{"datasource": {"NoCloud": {"meta-data": {"instance-id": "i-cfg"},
                                           "user-data": "#cloud-config\n"}}}"##,
        )
        .unwrap();
        let found = crawl(&f.paths, &sys_cfg, "", false).unwrap();
        assert_eq!(found.instance_id, "i-cfg");
        assert_eq!(found.detail, " [seed=ds_config]");
    }

    #[test]
    fn an_empty_meta_data_file_still_yields_the_default_instance_id() {
        let f = fixture();
        seed(&f.paths, "nocloud", &[("meta-data", ""), ("user-data", "")]);
        let found = crawl(&f.paths, &Object::new(), "", false).unwrap();
        assert_eq!(found.instance_id, "nocloud");
    }

    #[test]
    fn a_query_string_suppresses_the_appended_slash() {
        assert!(!has_query("http://h/seed"));
        assert!(!has_query("http://h/seed?"));
        assert!(!has_query("http://h/seed#a?b"));
        assert!(has_query("http://h/seed?q=1"));
        assert!(has_query("http://h/seed?q=1#frag"));
    }

    #[test]
    fn a_seedfrom_without_a_trailing_slash_gains_one() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("seed");
        std::fs::create_dir(&base).unwrap();
        std::fs::write(base.join("meta-data"), b"instance-id: i-seed\n").unwrap();
        std::fs::write(base.join("user-data"), b"#cloud-config\n").unwrap();
        let mut logger = ci_log::Logger::silent();
        let seed = read_seeded(base.to_str().unwrap(), &mut logger).unwrap();
        assert!(
            matches!(seed.meta, Meta::Raw(ref raw) if raw.starts_with(b"instance-id"))
        );
        assert!(seed.vendor_data.is_none());
        assert!(seed.network_config.is_none());
    }

    #[test]
    fn a_seedfrom_with_a_placeholder_is_substituted_rather_than_appended() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x-meta-data"), b"instance-id: i-pct\n")
            .unwrap();
        std::fs::write(dir.path().join("x-user-data"), b"hi").unwrap();
        let base = format!("{}/x-%s", dir.path().display());
        let mut logger = ci_log::Logger::silent();
        let seed = read_seeded(&base, &mut logger).unwrap();
        assert_eq!(seed.user_data.as_deref(), Some(b"hi".as_slice()));
    }
}
