//! Port of `sources/DataSourceAzure.py`, the part that shapes metadata.
//!
//! The crawl itself is not here: it needs ephemeral networking, netlink,
//! reprovisioning and the resource-disk work, all of which sit below the
//! metadata layer.

use std::path::Path;

use ci_config::{Object, Value};

/// `DS_NAME`.
pub const DS_NAME: &str = "Azure";

/// `DEFAULT_FS`.
pub const DEFAULT_FS: &str = "ext4";

/// `DS_CFG_KEY_PRESERVE_NTFS`.
pub const DS_CFG_KEY_PRESERVE_NTFS: &str = "never_destroy_ntfs";

/// `DEF_PASSWD_REDACTION`. A password this weak cannot have been set, so it
/// is what the platform substitutes once provisioning is done.
pub const PASSWD_REDACTION: &str = "REDACTED";

/// `AGENT_SEED_DIR`.
pub const AGENT_SEED_DIR: &str = "/var/lib/waagent";

/// `DEFAULT_PROVISIONING_ISO_DEV`.
pub const DEFAULT_PROVISIONING_ISO_DEV: &str = "/dev/sr0";

/// `RESOURCE_DISK_PATH`.
pub const RESOURCE_DISK_PATH: &str = "/dev/disk/cloud/azure_resource";

/// `PLATFORM_ENTROPY_SOURCE`.
pub const PLATFORM_ENTROPY_SOURCE: &str = "/sys/firmware/acpi/tables/OEM0";

const SOURCE: &str = "DataSourceAzure.py";

/// `ssh_util.VALID_KEY_TYPES`.
const VALID_KEY_TYPES: &[&str] = &[
    "rsa",
    "ecdsa",
    "ed25519",
    "ecdsa-sha2-nistp256-cert-v01@openssh.com",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384-cert-v01@openssh.com",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521-cert-v01@openssh.com",
    "ecdsa-sha2-nistp521",
    "sk-ecdsa-sha2-nistp256-cert-v01@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
    "sk-ssh-ed25519-cert-v01@openssh.com",
    "sk-ssh-ed25519@openssh.com",
    "ssh-ed25519-cert-v01@openssh.com",
    "ssh-ed25519",
    "ssh-rsa-cert-v01@openssh.com",
    "ssh-rsa",
    "ssh-xmss-cert-v01@openssh.com",
    "ssh-xmss@openssh.com",
];

/// `PPSType` — which flavour of pre-provisioning this boot is part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PpsType {
    None,
    OsDisk,
    Running,
    Savable,
    Unknown,
}

impl PpsType {
    /// The enum *value*, which is what the OVF and IMDS documents carry.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::OsDisk => "PreprovisionedOSDisk",
            Self::Running => "Running",
            Self::Savable => "Savable",
            Self::Unknown => "Unknown",
        }
    }
}

impl std::fmt::Display for PpsType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What `read_azure_ovf` returns, as the tuple upstream unpacks.
#[derive(Debug, Default, Clone)]
pub struct OvfCrawl {
    pub metadata: Object,
    pub userdata: Vec<u8>,
    pub config: Object,
}

/// `read_azure_ovf`.
///
/// The password is not hashed here: `encrypt_pass` is sha512-crypt with a
/// random salt, so `system_info.default_user.hashed_passwd` is left out
/// (deviation 81).
///
/// # Errors
/// Whatever [`super::ovf::parse_text`] rejects the document with.
pub fn read_azure_ovf(
    contents: &str,
    log: &mut ci_log::Logger,
) -> Result<OvfCrawl, super::ovf::Error> {
    let env = super::ovf::parse_text(contents, log)?;

    let mut metadata = Object::new();
    if let Some(hostname) = &env.hostname {
        metadata.insert("local-hostname".to_owned(), Value::from(hostname.clone()));
    }

    let mut config = Object::new();
    if !env.public_keys.is_empty() {
        let keys = env
            .public_keys
            .iter()
            .map(|key| {
                let mut entry = Object::new();
                entry.insert(
                    "fingerprint".to_owned(),
                    key.fingerprint.clone().map_or(Value::Null, Value::from),
                );
                entry.insert(
                    "path".to_owned(),
                    key.path.clone().map_or(Value::Null, Value::from),
                );
                entry.insert("value".to_owned(), Value::from(key.value.clone()));
                Value::Object(entry)
            })
            .collect();
        config.insert("_pubkeys".to_owned(), Value::Array(keys));
    }

    if let Some(disabled) = env.disable_ssh_password_auth {
        config.insert("ssh_pwauth".to_owned(), Value::Bool(!disabled));
    } else if env.password.is_some() {
        config.insert("ssh_pwauth".to_owned(), Value::Bool(true));
    }

    let mut default_user = Object::new();
    if let Some(username) = &env.username {
        default_user.insert("name".to_owned(), Value::from(username.clone()));
    }
    if env.password.is_some() {
        default_user.insert("lock_passwd".to_owned(), Value::Bool(false));
    }
    if !default_user.is_empty() {
        let mut system_info = Object::new();
        system_info.insert("default_user".to_owned(), Value::Object(default_user));
        config.insert("system_info".to_owned(), Value::Object(system_info));
    }

    config.insert(
        "PreprovisionedVm".to_owned(),
        Value::Bool(env.preprovisioned_vm),
    );
    log.info(
        SOURCE,
        &format!("PreprovisionedVm: {}", py_bool(env.preprovisioned_vm)),
    );

    let vm_type = env
        .preprovisioned_vm_type
        .clone()
        .map_or(Value::Null, Value::from);
    log.info(SOURCE, &format!("PreprovisionedVMType: {}", show(&vm_type)));
    config.insert("PreprovisionedVMType".to_owned(), vm_type);

    config.insert(
        "ProvisionGuestProxyAgent".to_owned(),
        Value::Bool(env.provision_guest_proxy_agent),
    );
    log.info(
        SOURCE,
        &format!(
            "ProvisionGuestProxyAgent: {}",
            py_bool(env.provision_guest_proxy_agent)
        ),
    );

    Ok(OvfCrawl {
        metadata,
        userdata: env.custom_data.unwrap_or_default(),
        config,
    })
}

/// `write_files._redact_password`.
///
/// Azure hands the admin password over in cleartext inside `ovf-env.xml`, and
/// the file is about to be cached where it will outlive the boot, so the
/// password is replaced with a value that cannot satisfy the platform's own
/// complexity rules before anything touches the disk.
///
/// Upstream's test is `"UserPassword" in elem.tag`, a substring of the whole
/// `{uri}local` tag, so `MyUserPasswordX` matches and so would an element in a
/// namespace whose URI happened to contain the word. That is transcribed
/// rather than tightened: this decides what gets *hidden*, and narrowing it
/// could leave a password behind.
///
/// A document that will not parse or will not resolve is returned untouched,
/// after a `CRITICAL` log — the same bargain upstream strikes with its bare
/// `except Exception`. The caller must then refuse to write it.
fn redact_password(
    contents: &[u8],
    path: &Path,
    log: &mut ci_log::Logger,
) -> Option<Vec<u8>> {
    fn redact(element: &mut ci_config::xml::Element) {
        if element.tag().contains("UserPassword")
            && element.text.as_deref() != Some(PASSWD_REDACTION)
        {
            element.text = Some(PASSWD_REDACTION.to_owned());
        }
        for child in &mut element.children {
            redact(child);
        }
    }

    let parsed = std::str::from_utf8(contents).ok().and_then(|text| {
        ci_config::xml::parse(text, ci_config::xml::Limits::default()).ok()
    });
    let Some(mut root) = parsed else {
        log.log(
            ci_log::Level::Critical,
            SOURCE,
            &format!("failed to redact userpassword in {}", path.display()),
        );
        return None;
    };
    redact(&mut root);
    Some(ci_config::xml::serialize(&root))
}

/// `write_files`.
///
/// The walinux agent writes these world readable and relies on the directory
/// to protect them; the port does not, because the cached `ovf-env.xml` is the
/// one file here that carried a password and a redaction that silently failed
/// would publish it. So `dirmode` is still `0o700` and the files are still
/// `0o600`, but an `ovf-env.xml` that could not be redacted is **skipped**
/// rather than written through, which is where upstream differs.
pub fn write_files(
    data_dir: &Path,
    files: &Object,
    dirmode: u32,
    log: &mut ci_log::Logger,
) {
    if data_dir.as_os_str().is_empty() {
        return;
    }
    if let Err(error) = ci_sys::path::ensure_dir(data_dir, dirmode) {
        log.warning(
            SOURCE,
            &format!("Failed to create {}: {error}", data_dir.display()),
        );
        return;
    }
    for (name, value) in files {
        let path = data_dir.join(name);
        let Some(encoded) =
            value.as_str().and_then(|text| text.strip_prefix("ci-b64:"))
        else {
            continue;
        };
        let Some(content) = ci_core::b64::decode(encoded) else {
            continue;
        };
        let content = if name.contains("ovf-env.xml") {
            let Some(redacted) = redact_password(&content, &path, log) else {
                log.warning(
                    SOURCE,
                    &format!(
                        "Not caching {}: it could not be redacted",
                        path.display()
                    ),
                );
                continue;
            };
            redacted
        } else {
            content
        };
        if let Err(error) = ci_sys::atomic::write_file(
            &path,
            &content,
            ci_sys::atomic::WriteOptions::SECRET,
        ) {
            log.warning(
                SOURCE,
                &format!("Failed to write {}: {error}", path.display()),
            );
        }
    }
}

/// `load_azure_ds_dir`, minus the raw-file dict the caller only writes back.
///
/// # Errors
/// `NonAzure` when the directory holds no `ovf-env.xml`, otherwise whatever
/// the document is rejected with.
pub fn load_azure_ds_dir(
    dir: &Path,
    log: &mut ci_log::Logger,
) -> Result<OvfCrawl, super::ovf::Error> {
    let path = dir.join("ovf-env.xml");
    let contents = std::fs::read(&path)
        .map_err(|_| super::ovf::Error::NonAzure("No ovf-env file found".to_owned()))?;
    read_azure_ovf(&String::from_utf8_lossy(&contents), log)
}

/// `_username_from_imds`.
#[must_use]
pub fn username_from_imds(imds: &Object) -> Option<&str> {
    imds.get("compute")?
        .get("osProfile")?
        .get("adminUsername")?
        .as_str()
}

/// `_userdata_from_imds`.
#[must_use]
pub fn userdata_from_imds(imds: &Object) -> Option<&str> {
    imds.get("compute")?.get("userData")?.as_str()
}

/// `_hostname_from_imds`.
#[must_use]
pub fn hostname_from_imds(imds: &Object) -> Option<&str> {
    imds.get("compute")?
        .get("osProfile")?
        .get("computerName")?
        .as_str()
}

/// `_disable_password_from_imds`.
///
/// The field is a string, and upstream compares it to `"true"`, so anything
/// else — including a real JSON `true` — reads as "not disabled".
#[must_use]
pub fn disable_password_from_imds(imds: &Object) -> Option<bool> {
    let value = imds
        .get("compute")?
        .get("osProfile")?
        .get("disablePasswordAuthentication")?;
    Some(value.as_str() == Some("true"))
}

/// `_ppstype_from_imds`.
#[must_use]
pub fn ppstype_from_imds(imds: &Object) -> Option<&str> {
    imds.get("extended")?
        .get("compute")?
        .get("ppsType")?
        .as_str()
}

/// `_determine_pps_type`.
///
/// `reported_ready` stands in for `os.path.isfile(_reported_ready_marker_file)`.
#[must_use]
pub fn determine_pps_type(
    ovf_cfg: &Object,
    imds: &Object,
    reported_ready: bool,
    log: &mut ci_log::Logger,
) -> PpsType {
    let ovf_type = ovf_cfg.get("PreprovisionedVMType").and_then(Value::as_str);
    let imds_type = ppstype_from_imds(imds);
    let is = |kind: PpsType| {
        ovf_type == Some(kind.as_str()) || imds_type == Some(kind.as_str())
    };

    let pps_type = if reported_ready {
        PpsType::Unknown
    } else if is(PpsType::Savable) {
        PpsType::Savable
    } else if is(PpsType::OsDisk) {
        PpsType::OsDisk
    } else if ovf_cfg.get("PreprovisionedVm") == Some(&Value::Bool(true))
        || is(PpsType::Running)
    {
        PpsType::Running
    } else {
        PpsType::None
    };

    log.info(SOURCE, &format!("PPS type: {pps_type}"));
    pps_type
}

/// `_key_is_openssh_formatted`.
#[must_use]
pub fn key_is_openssh_formatted(key: &str) -> bool {
    // LP: #1910835. A bare `\n` is not caught, because `str.split(None, 2)`
    // treats it as ordinary whitespace.
    if key.trim().contains("\r\n") {
        return false;
    }

    let line = key.trim_end_matches(['\r', '\n']);
    if line.starts_with('#') || line.trim().is_empty() {
        return false;
    }

    let entry = line.trim();
    if has_key_type(entry) {
        return true;
    }
    // `AuthKeyLineParser` falls back to reading leading options.
    has_key_type(extract_options(entry).1)
}

/// `parse_ssh_key`'s first two fields, as far as validity is concerned.
fn has_key_type(entry: &str) -> bool {
    let mut fields = entry.split_ascii_whitespace();
    let Some(keytype) = fields.next() else {
        return false;
    };
    fields.next().is_some() && VALID_KEY_TYPES.contains(&keytype)
}

/// `AuthKeyLineParser._extract_options`, returning `(options, remain)`.
///
/// Upstream walks characters, not bytes, so this does too.
fn extract_options(entry: &str) -> (&str, &str) {
    let chars: Vec<char> = entry.chars().collect();
    let at = |index: usize| chars.get(index).copied();
    let mut quoted = false;
    let mut i = 0;
    while at(i).is_some_and(|c| quoted || !matches!(c, ' ' | '\t')) {
        if i + 1 >= chars.len() {
            i += 1;
            break;
        }
        match (at(i), at(i + 1)) {
            (Some('\\'), Some('"')) => i += 1,
            (Some('"'), _) => quoted = !quoted,
            _ => {}
        }
        i += 1;
    }
    let split = entry
        .char_indices()
        .nth(i)
        .map_or(entry.len(), |(offset, _)| offset);
    let (options, rest) = entry.split_at(split);
    (options, rest.trim_start())
}

/// `_get_public_keys_from_imds`.
///
/// `None` is upstream's `KeyError` or `ValueError`, both of which send the
/// caller to the OVF keys instead. A `publicKeys` that is not a list, or a
/// `keyData` that is not a string, also lands here rather than escaping the
/// fallback (bug B49).
#[must_use]
pub fn public_keys_from_imds(
    imds: &Object,
    log: &mut ci_log::Logger,
) -> Option<Vec<String>> {
    let entries = imds
        .get("compute")
        .and_then(|compute| compute.get("publicKeys"))
        .and_then(Value::as_array);
    let keys: Option<Vec<String>> = entries.and_then(|entries| {
        entries
            .iter()
            .map(|entry| {
                entry
                    .get("keyData")
                    .map(|key| key.as_str().map(ToOwned::to_owned).unwrap_or_default())
            })
            .collect()
    });
    let Some(keys) = keys else {
        log.debug(SOURCE, "No SSH keys found in IMDS metadata");
        return None;
    };

    if keys.iter().any(|key| !key_is_openssh_formatted(key)) {
        log.debug(SOURCE, "Key(s) not in OpenSSH format");
        return None;
    }

    log.debug(SOURCE, &format!("Retrieved {} keys from IMDS", keys.len()));
    Some(keys)
}

/// `_get_random_seed`.
///
/// Upstream's docstring promises `None` when the file is missing, but
/// `load_binary_file(quiet=True)` hands back empty bytes, so the answer is an
/// empty string. The only caller tests it for truthiness, so the two agree.
#[must_use]
pub fn random_seed(source: &Path) -> String {
    ci_core::b64::encode(&std::fs::read(source).unwrap_or_default())
}

/// `_get_subplatform`.
#[must_use]
pub fn subplatform(seed: Option<&str>) -> String {
    let Some(seed) = seed else {
        return "unknown (None)".to_owned();
    };
    let kind = if seed.starts_with("/dev") {
        "config-disk"
    } else if seed.eq_ignore_ascii_case("imds") {
        "imds"
    } else {
        "seed-dir"
    };
    format!("{kind} ({seed})")
}

/// `_iid`.
///
/// The previous id wins when it differs from the system UUID only in case or
/// in byte order (LP: #1835584).
#[must_use]
pub fn iid(
    system_uuid: &str,
    previous: Option<&str>,
    log: &mut ci_log::Logger,
) -> String {
    let Some(previous) = previous.map(str::trim) else {
        return system_uuid.to_owned();
    };
    let swapped = super::identity::byte_swap_system_uuid(system_uuid, log);
    if previous.to_lowercase() == system_uuid
        || Some(previous.to_lowercase()) == swapped
    {
        return previous.to_owned();
    }
    system_uuid.to_owned()
}

/// `ds_detect`: the chassis asset tag, or a seeded `ovf-env.xml`.
#[must_use]
pub fn ds_detect(seed_dir: Option<&Path>, log: &mut ci_log::Logger) -> bool {
    if super::identity::query_chassis_asset_tag(log).is_some() {
        return true;
    }
    seed_dir.is_some_and(|dir| dir.join("ovf-env.xml").is_file())
}

/// `DEFAULT_METADATA`.
#[must_use]
pub fn default_metadata() -> Object {
    let mut metadata = Object::new();
    metadata.insert("instance-id".to_owned(), Value::from("iid-AZURE-NODE"));
    metadata
}

/// `BUILTIN_DS_CONFIG`.
#[must_use]
pub fn builtin_ds_config() -> Object {
    let mut aliases = Object::new();
    aliases.insert("ephemeral0".to_owned(), Value::from(RESOURCE_DISK_PATH));

    let mut config = Object::new();
    config.insert("data_dir".to_owned(), Value::from(AGENT_SEED_DIR));
    config.insert("disk_aliases".to_owned(), Value::Object(aliases));
    config.insert("apply_network_config".to_owned(), Value::Bool(true));
    config.insert(
        "apply_network_config_for_secondary_ips".to_owned(),
        Value::Bool(true),
    );
    config
}

/// `self.ds_cfg`: `datasource.Azure` over `BUILTIN_DS_CONFIG`.
#[must_use]
pub fn ds_config(sys_cfg: &Object) -> Object {
    let user = sys_cfg
        .get("datasource")
        .and_then(|datasource| datasource.get(DS_NAME))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    ci_config::merge::merge_many(vec![user, builtin_ds_config()], false)
}

/// `device_name_to_device`.
#[must_use]
pub fn device_name_to_device<'a>(ds_cfg: &'a Object, name: &str) -> Option<&'a str> {
    ds_cfg
        .get("disk_aliases")
        .and_then(|aliases| aliases.get(name))
        .and_then(Value::as_str)
}

/// `get_instance_id`: the crawled id, or `_iid` when there is none.
#[must_use]
pub fn instance_id(metadata: &Object, fallback: &str) -> String {
    metadata
        .get("instance-id")
        .map_or_else(|| fallback.to_owned(), py_str)
}

/// `_get_public_keys_from_ovf`: the keys wireserver supplied, if any.
#[must_use]
pub fn public_keys_from_ovf(
    metadata: &Object,
    log: &mut ci_log::Logger,
) -> Vec<String> {
    let Some(keys) = metadata.get("public-keys").and_then(Value::as_array) else {
        log.debug(SOURCE, "No keys available from OVF");
        return Vec::new();
    };
    log.debug(SOURCE, &format!("Retrieved {} keys from OVF", keys.len()));
    keys.iter().map(py_str).collect()
}

/// `get_public_ssh_keys`: IMDS first, the OVF keys when IMDS cannot answer.
#[must_use]
pub fn public_ssh_keys(metadata: &Object, log: &mut ci_log::Logger) -> Vec<String> {
    let imds = metadata.get("imds").and_then(Value::as_object);
    if let Some(keys) = imds.and_then(|imds| public_keys_from_imds(imds, log)) {
        return keys;
    }
    public_keys_from_ovf(metadata, log)
}

/// `_determine_wireserver_pubkey_info`: the OVF fingerprints to fetch.
///
/// `None` is both of upstream's `None`s — IMDS already answered, or it did not
/// and the OVF carried no fingerprints either. The caller treats them alike.
#[must_use]
pub fn wireserver_pubkey_info(
    cfg: &Object,
    imds: &Object,
    log: &mut ci_log::Logger,
) -> Option<Vec<Value>> {
    if public_keys_from_imds(imds, log).is_some() {
        return None;
    }
    let info = cfg.get("_pubkeys").and_then(Value::as_array).cloned();
    log.debug(
        SOURCE,
        &format!(
            "Retrieved {} fingerprints from OVF",
            info.as_ref().map_or(0, Vec::len)
        ),
    );
    info
}

/// `availability_zone`: `imds.compute.platformFaultDomain`.
#[must_use]
pub fn availability_zone(metadata: &Object) -> Option<&Value> {
    metadata
        .get("imds")
        .and_then(|imds| imds.get("compute"))
        .and_then(|compute| compute.get("platformFaultDomain"))
}

/// `region`: `imds.compute.location`.
#[must_use]
pub fn region(metadata: &Object) -> Option<&Value> {
    metadata
        .get("imds")
        .and_then(|imds| imds.get("compute"))
        .and_then(|compute| compute.get("location"))
}

/// `check_instance_id`: `sources.instance_id_matches_system_uuid`.
#[must_use]
pub fn check_instance_id(instance_id: &str, system_uuid: Option<&str>) -> bool {
    if instance_id.is_empty() {
        return false;
    }
    system_uuid.is_some_and(|uuid| {
        !uuid.is_empty() && instance_id.to_lowercase() == uuid.to_lowercase()
    })
}

/// `_generate_network_config`.
///
/// `None` is "use the fallback config", which needs
/// `net.generate_fallback_config` and is not ported. An IMDS document that
/// describes no usable interface still wins here, as upstream (bug B50).
#[must_use]
pub fn generate_network_config(
    ds_cfg: &Object,
    imds: Option<&Object>,
    interfaces: &[super::netcfg::Interface],
    log: &mut ci_log::Logger,
) -> Option<Value> {
    let apply = ds_cfg
        .get("apply_network_config")
        .is_some_and(ci_config::option::py_truthy);
    let imds = imds.filter(|imds| !imds.is_empty() && apply)?;
    let secondary = ds_cfg
        .get("apply_network_config_for_secondary_ips")
        .is_some_and(ci_config::option::py_truthy);

    let generated = match imds.get("network").and_then(Value::as_object) {
        Some(network) => {
            super::netcfg::generate_network_config(network, secondary, interfaces, log)
        }
        None => Err("'network'".to_owned()),
    };
    match generated {
        Ok(config) => Some(config),
        Err(error) => {
            log.error(
                SOURCE,
                &format!(
                    "Failed generating network config \
                     from IMDS network metadata: {error}"
                ),
            );
            None
        }
    }
}

/// `repr()` of an optional string, which is what `%s` on a `None` prints.
fn show(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| "None".to_owned(), ToOwned::to_owned)
}

/// `%s` on a Python bool.
pub(crate) fn py_bool(value: bool) -> &'static str {
    if value {
        "True"
    } else {
        "False"
    }
}

/// `str()` of a value, for the two places upstream calls it.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => py_bool(*flag).to_owned(),
        Value::Null => "None".to_owned(),
        other => other.to_string(),
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

    fn obj(json: &str) -> Object {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn an_ovf_without_a_password_leaves_ssh_pwauth_alone() {
        let mut log = ci_log::Logger::silent();
        let document = concat!(
            r#"<Environment xmlns="http://schemas.dmtf.org/ovf/environment/1""#,
            r#" xmlns:wa="http://schemas.microsoft.com/windowsazure">"#,
            "<wa:ProvisioningSection><wa:Version>1.0</wa:Version>",
            "<wa:LinuxProvisioningConfigurationSet>",
            "<wa:HostName>host</wa:HostName>",
            "<wa:UserName>user</wa:UserName>",
            "</wa:LinuxProvisioningConfigurationSet>",
            "</wa:ProvisioningSection>",
            "<wa:PlatformSettingsSection><wa:Version>1.0</wa:Version>",
            "<wa:PlatformSettings/></wa:PlatformSettingsSection>",
            "</Environment>",
        );

        let crawl = read_azure_ovf(document, &mut log).unwrap();

        assert_eq!(crawl.metadata["local-hostname"], Value::from("host"));
        assert!(!crawl.config.contains_key("ssh_pwauth"));
        assert_eq!(
            crawl.config["system_info"]["default_user"]["name"],
            Value::from("user")
        );
        assert_eq!(crawl.config["PreprovisionedVm"], Value::Bool(false));
        assert_eq!(crawl.config["PreprovisionedVMType"], Value::Null);
    }

    #[test]
    fn the_pps_marker_outranks_every_document() {
        let mut log = ci_log::Logger::silent();
        let ovf = obj(r#"{"PreprovisionedVMType": "Savable"}"#);

        let kind = determine_pps_type(&ovf, &Object::new(), true, &mut log);

        assert_eq!(kind, PpsType::Unknown);
    }

    #[test]
    fn savable_outranks_os_disk_and_either_document_can_say_it() {
        let mut log = ci_log::Logger::silent();
        let ovf = obj(r#"{"PreprovisionedVMType": "PreprovisionedOSDisk"}"#);
        let imds = obj(r#"{"extended": {"compute": {"ppsType": "Savable"}}}"#);

        let kind = determine_pps_type(&ovf, &imds, false, &mut log);

        assert_eq!(kind, PpsType::Savable);
    }

    #[test]
    fn the_legacy_boolean_still_means_running() {
        let mut log = ci_log::Logger::silent();
        let ovf = obj(r#"{"PreprovisionedVm": true}"#);

        let kind = determine_pps_type(&ovf, &Object::new(), false, &mut log);

        assert_eq!(kind, PpsType::Running);
    }

    #[test]
    fn nothing_anywhere_is_not_preprovisioning() {
        let mut log = ci_log::Logger::silent();
        let ovf = obj(r#"{"PreprovisionedVm": false}"#);

        let kind = determine_pps_type(&ovf, &Object::new(), false, &mut log);

        assert_eq!(kind, PpsType::None);
    }

    #[test]
    fn disable_password_only_believes_the_string_true() {
        let profile = |value: &str| {
            obj(&format!(
                r#"{{"compute": {{"osProfile": {{"disablePasswordAuthentication": {value}}}}}}}"#
            ))
        };

        assert_eq!(
            disable_password_from_imds(&profile(r#""true""#)),
            Some(true)
        );
        assert_eq!(
            disable_password_from_imds(&profile(r#""false""#)),
            Some(false)
        );
        assert_eq!(disable_password_from_imds(&profile("true")), Some(false));
        assert_eq!(disable_password_from_imds(&Object::new()), None);
    }

    #[test]
    fn a_key_with_options_is_still_openssh_formatted() {
        assert!(key_is_openssh_formatted("ssh-rsa AAAAB3 user@host"));
        assert!(key_is_openssh_formatted("ssh-ed25519 AAAAC3"));
        assert!(key_is_openssh_formatted(
            r#"command="/bin/true" ssh-rsa AAAAB3"#
        ));
        assert!(!key_is_openssh_formatted("ssh-rsa"));
        assert!(!key_is_openssh_formatted("not-a-type AAAAB3"));
        assert!(!key_is_openssh_formatted("# ssh-rsa AAAAB3"));
        assert!(!key_is_openssh_formatted("   "));
    }

    #[test]
    fn a_windows_line_ending_inside_the_key_is_rejected() {
        assert!(!key_is_openssh_formatted("ssh-rsa AAAAB3\r\nssh-rsa CCCC"));
        // A trailing one is stripped before the check, so it is fine.
        assert!(key_is_openssh_formatted("ssh-rsa AAAAB3\r\n"));
    }

    #[test]
    fn one_unusable_key_discards_the_whole_imds_set() {
        let mut log = ci_log::Logger::silent();
        let good = obj(r#"{"compute": {"publicKeys": [{"keyData": "ssh-rsa AAAA"}]}}"#);
        let bad = obj(r#"{"compute": {"publicKeys": [
                {"keyData": "ssh-rsa AAAA"}, {"keyData": "junk"}]}}"#);
        let missing = obj(r#"{"compute": {}}"#);

        assert_eq!(
            public_keys_from_imds(&good, &mut log),
            Some(vec!["ssh-rsa AAAA".to_owned()])
        );
        assert_eq!(public_keys_from_imds(&bad, &mut log), None);
        assert_eq!(public_keys_from_imds(&missing, &mut log), None);
    }

    #[test]
    fn a_byte_swapped_or_uppercased_previous_id_is_kept() {
        let mut log = ci_log::Logger::silent();
        let uuid = "5f4dcc3b-5aa7-65d6-1b0e-99e5f4dcc3b5";
        let swapped =
            super::super::identity::byte_swap_system_uuid(uuid, &mut log).unwrap();

        assert_eq!(iid(uuid, None, &mut log), uuid);
        assert_eq!(
            iid(uuid, Some(&uuid.to_uppercase()), &mut log),
            uuid.to_uppercase()
        );
        assert_eq!(iid(uuid, Some(&swapped), &mut log), swapped);
        assert_eq!(iid(uuid, Some("other"), &mut log), uuid);
    }

    #[test]
    fn the_subplatform_slug_comes_from_the_shape_of_the_seed() {
        assert_eq!(subplatform(None), "unknown (None)");
        assert_eq!(subplatform(Some("/dev/sr0")), "config-disk (/dev/sr0)");
        assert_eq!(subplatform(Some("IMDS")), "imds (IMDS)");
        assert_eq!(
            subplatform(Some("/var/lib/waagent")),
            "seed-dir (/var/lib/waagent)"
        );
    }

    #[test]
    fn the_datasource_config_is_the_builtin_with_the_users_keys_on_top() {
        let empty = ds_config(&Object::new());
        assert_eq!(empty, builtin_ds_config());
        assert_eq!(
            device_name_to_device(&empty, "ephemeral0"),
            Some(RESOURCE_DISK_PATH)
        );
        assert_eq!(device_name_to_device(&empty, "ephemeral1"), None);

        let sys_cfg = obj(r#"{"datasource": {"Azure": {"apply_network_config": false,
                "disk_aliases": {"ephemeral1": "/dev/sdc"}}}}"#);
        let merged = ds_config(&sys_cfg);

        assert_eq!(merged["apply_network_config"], Value::Bool(false));
        assert_eq!(merged["data_dir"], Value::from(AGENT_SEED_DIR));
        // The alias maps are merged, not replaced.
        assert_eq!(
            device_name_to_device(&merged, "ephemeral0"),
            Some(RESOURCE_DISK_PATH)
        );
        assert_eq!(
            device_name_to_device(&merged, "ephemeral1"),
            Some("/dev/sdc")
        );
    }

    #[test]
    fn imds_keys_win_and_the_ovf_keys_are_the_fallback() {
        let mut log = ci_log::Logger::silent();
        let both = obj(r#"{"public-keys": ["ssh-rsa OVF"],
                "imds": {"compute": {"publicKeys": [{"keyData": "ssh-rsa IMDS"}]}}}"#);
        let unusable = obj(r#"{"public-keys": ["ssh-rsa OVF"],
                "imds": {"compute": {"publicKeys": [{"keyData": null}]}}}"#);

        assert_eq!(public_ssh_keys(&both, &mut log), ["ssh-rsa IMDS"]);
        assert_eq!(public_ssh_keys(&unusable, &mut log), ["ssh-rsa OVF"]);
        assert!(public_ssh_keys(&Object::new(), &mut log).is_empty());
    }

    #[test]
    fn wireserver_is_only_asked_for_fingerprints_imds_could_not_supply() {
        let mut log = ci_log::Logger::silent();
        let cfg = obj(r#"{"_pubkeys": [{"fingerprint": "AA", "path": "p"}]}"#);
        let good = obj(r#"{"compute": {"publicKeys": [{"keyData": "ssh-rsa AAAA"}]}}"#);

        assert!(wireserver_pubkey_info(&cfg, &good, &mut log).is_none());
        assert_eq!(
            wireserver_pubkey_info(&cfg, &Object::new(), &mut log)
                .map(|info| info.len()),
            Some(1)
        );
        // Upstream returns `None` here too: absent `_pubkeys` is not `[]`.
        assert_eq!(
            wireserver_pubkey_info(&Object::new(), &Object::new(), &mut log),
            None
        );
        assert_eq!(
            wireserver_pubkey_info(
                &obj(r#"{"_pubkeys": []}"#),
                &Object::new(),
                &mut log
            ),
            Some(Vec::new())
        );
    }

    #[test]
    fn the_instance_id_falls_back_only_when_the_key_is_absent() {
        let md = obj(r#"{"instance-id": "iid-AZURE-NODE"}"#);

        assert_eq!(instance_id(&md, "from-uuid"), "iid-AZURE-NODE");
        assert_eq!(instance_id(&Object::new(), "from-uuid"), "from-uuid");
        assert!(check_instance_id("AABB", Some("aabb")));
        assert!(!check_instance_id("AABB", Some("ccdd")));
        assert!(!check_instance_id("AABB", None));
        assert!(!check_instance_id("", Some("aabb")));
    }

    #[test]
    fn the_network_config_is_skipped_when_the_datasource_config_says_so() {
        let mut log = ci_log::Logger::silent();
        let cfg = ds_config(&Object::new());
        let off = ds_config(&obj(
            r#"{"datasource": {"Azure": {"apply_network_config": false}}}"#,
        ));
        let imds = obj(r#"{"network": {"interface": [{"macAddress": "001122AABBCC",
                "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"}]}}]}}"#);

        let generated =
            generate_network_config(&cfg, Some(&imds), &[], &mut log).unwrap();
        assert_eq!(
            generated["ethernets"]["eth0"]["set-name"],
            Value::from("eth0")
        );
        assert!(generate_network_config(&off, Some(&imds), &[], &mut log).is_none());
        assert!(generate_network_config(&cfg, None, &[], &mut log).is_none());
        // An IMDS document without a `network` key is upstream's KeyError.
        assert!(generate_network_config(
            &cfg,
            Some(&obj(r#"{"compute": {}}"#)),
            &[],
            &mut log
        )
        .is_none());
    }

    #[test]
    fn the_region_and_zone_are_read_straight_off_the_imds_compute_block() {
        let md = obj(r#"{"imds": {"compute": {"location": "westus2",
                "platformFaultDomain": "0"}}}"#);

        assert_eq!(region(&md), Some(&Value::from("westus2")));
        assert_eq!(availability_zone(&md), Some(&Value::from("0")));
        assert_eq!(region(&Object::new()), None);
    }

    fn cached(files: &[(&str, &[u8])]) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let mut log = ci_log::Logger::silent();
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("waagent");
        let blobs = files
            .iter()
            .map(|(name, contents)| {
                (
                    (*name).to_owned(),
                    Value::from(format!("ci-b64:{}", ci_core::b64::encode(contents))),
                )
            })
            .collect();

        write_files(&data_dir, &blobs, 0o700, &mut log);

        assert_eq!(
            std::fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        (dir, data_dir)
    }

    #[test]
    fn the_cached_ovf_has_its_password_replaced_and_nothing_else_moved() {
        let document = concat!(
            r#"<ns0:Environment xmlns:ns0="http://schemas.dmtf.org/ovf/environment/1""#,
            r#" xmlns:ns1="http://schemas.microsoft.com/windowsazure">"#,
            "<ns1:UserName>user</ns1:UserName>",
            "<ns1:UserPassword>hunter2</ns1:UserPassword>",
            "</ns0:Environment>",
        );
        let (_dir, data_dir) = cached(&[("ovf-env.xml", document.as_bytes())]);

        let written = std::fs::read_to_string(data_dir.join("ovf-env.xml")).unwrap();
        assert!(!written.contains("hunter2"), "{written}");
        assert_eq!(written, document.replace("hunter2", PASSWD_REDACTION));
    }

    #[test]
    fn a_file_that_is_not_the_ovf_is_cached_verbatim_and_all_of_them_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, data_dir) = cached(&[
            (
                "ovf-env.xml",
                b"<R><UserPassword>hunter2</UserPassword></R>",
            ),
            (
                "SharedConfig.xml",
                b"<Config><Password>kept</Password></Config>",
            ),
        ]);

        assert_eq!(
            std::fs::read_to_string(data_dir.join("SharedConfig.xml")).unwrap(),
            "<Config><Password>kept</Password></Config>"
        );
        for name in ["ovf-env.xml", "SharedConfig.xml"] {
            let mode = std::fs::metadata(data_dir.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }

    #[test]
    fn an_ovf_that_cannot_be_redacted_is_not_cached_at_all() {
        // The password is in there in cleartext; a parse failure must not
        // become a decision to write it out unchanged.
        let (_dir, data_dir) =
            cached(&[("ovf-env.xml", b"<R><UserPassword>hunter2</R>")]);

        assert!(!data_dir.join("ovf-env.xml").exists());
    }
}
