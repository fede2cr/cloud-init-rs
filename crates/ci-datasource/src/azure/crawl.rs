//! `DataSourceAzure.crawl_metadata`: the order the readers in [`super::ds`] run
//! in, and what happens when one of them cannot answer.
//!
//! Everything that touches the machine — finding and mounting provisioning
//! media, raising a DHCP lease, reaching IMDS, talking to wireserver — is
//! behind [`Platform`], because the interesting part of the crawl is the
//! sequencing and it should be testable without any of that.

use std::path::{Path, PathBuf};

use ci_config::{Object, Value};

use super::ds::{self, OvfCrawl, PpsType};
use super::errors::ReportableError;

const SOURCE: &str = "azure.py";

/// `files` holds the OVF document as upstream read it: bytes, which only
/// survive JSON as the `ci-b64:` strings `json_dumps` renders them into.
fn blob(contents: &[u8]) -> Value {
    Value::String(format!("ci-b64:{}", ci_core::b64::encode(contents)))
}

/// `DEFAULT_PROVISIONING_ISO_DEV`.
pub const DEFAULT_PROVISIONING_ISO_DEV: &str = "/dev/sr0";

/// A candidate provisioning-media source, in `list_possible_azure_ds` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The seed directory or the cached `data_dir`, read in place.
    Dir(PathBuf),
    /// A block device, which has to be mounted first.
    Device(PathBuf),
}

impl Source {
    /// The string upstream logs and stores as `seed`.
    #[must_use]
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Dir(path) | Self::Device(path) => path.to_string_lossy(),
        }
    }
}

/// Why a candidate source yielded no OVF document.
#[derive(Debug)]
pub enum SourceError {
    /// `NonAzureDataSource`.
    NonAzure,
    /// `util.MountFailedError`.
    MountFailed,
}

/// What the crawl found, as the keys `crawl_metadata` returns.
#[derive(Debug, Default, Clone)]
pub struct Crawled {
    pub cfg: Object,
    pub files: Object,
    pub metadata: Object,
    pub userdata: Vec<u8>,
    /// `self.seed`: the source that answered, or `IMDS`.
    pub seed: String,
}

/// Why the crawl could not produce metadata.
#[derive(Debug)]
pub enum Error {
    /// `sources.InvalidMetaDataException`.
    InvalidMetadata(String),
    /// A failure upstream reports to the platform before re-raising.
    Reportable(Box<ReportableError>),
    /// Pre-provisioning, which needs the netlink and reprovisioning halves
    /// this port does not have yet.
    Preprovisioning(PpsType),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMetadata(message) => f.write_str(message),
            Self::Reportable(error) => f.write_str(&error.reason),
            Self::Preprovisioning(pps) => {
                write!(f, "Pre-provisioning ({pps}) is not supported")
            }
        }
    }
}

/// The machine the crawl runs on.
///
/// Upstream reaches for the host through `self`; the split here is the same
/// one, made explicit so the sequencing above can be driven from a fixture.
pub trait Platform {
    /// `identity.query_system_uuid`.
    ///
    /// # Errors
    /// The message `_query_vm_id` turns into a reportable error.
    fn system_uuid(&mut self) -> Result<String, String>;

    /// `identity.is_vm_gen1`, which upstream calls from inside
    /// `convert_system_uuid_to_vm_id` and the port takes as an argument.
    fn is_gen1(&mut self) -> bool;

    /// `list_possible_azure_ds`.
    fn candidates(&mut self) -> Vec<Source>;

    /// `load_azure_ds_dir`, through `mount_cb` for a device.
    ///
    /// The raw document comes back alongside the parsed one because upstream
    /// puts it in `files` untouched.
    ///
    /// # Errors
    /// Whichever of upstream's two swallowed failures applies.
    fn load(
        &mut self,
        source: &Source,
        log: &mut ci_log::Logger,
    ) -> Result<(OvfCrawl, Vec<u8>), SourceError>;

    /// `_setup_ephemeral_networking`, returning `_is_ephemeral_networking_up`.
    ///
    /// Upstream swallows `NoDHCPLeaseError` here and asks afterwards, so the
    /// two are one answer.
    fn setup_ephemeral_networking(
        &mut self,
        timeout_minutes: u64,
        log: &mut ci_log::Logger,
    ) -> bool;

    /// `get_metadata_from_imds`, which reports its own failures and returns an
    /// empty document rather than raising.
    fn fetch_imds(&mut self, report_failure: bool, log: &mut ci_log::Logger) -> Object;

    /// `_report_ready`, returning the keys wireserver supplied.
    ///
    /// # Errors
    /// Any failure. Upstream continues on a best-effort basis regardless.
    fn report_ready(
        &mut self,
        pubkey_info: Option<&[Value]>,
        log: &mut ci_log::Logger,
    ) -> Result<Vec<String>, String>;

    /// The `instance-id` of the previous boot, if this instance had one.
    fn previous_instance_id(&mut self) -> Option<String>;

    /// `_get_random_seed`.
    fn random_seed(&mut self) -> Option<String>;

    /// `os.path.isfile(self._reported_ready_marker_file)`.
    fn reported_ready_marker(&mut self) -> bool;

    /// `_cleanup_markers`.
    fn cleanup_markers(&mut self, log: &mut ci_log::Logger);

    /// `_check_azure_proxy_agent_status`, run only when the OVF opts in.
    ///
    /// # Errors
    /// The reportable error that aborts the crawl.
    fn check_proxy_agent(
        &mut self,
        _log: &mut ci_log::Logger,
    ) -> Result<(), ReportableError> {
        Ok(())
    }

    /// `validate_imds_network_metadata`, which only reports telemetry.
    fn validate_imds_network_metadata(
        &mut self,
        _imds: &Object,
        _log: &mut ci_log::Logger,
    ) -> bool {
        true
    }
}

/// `crawl_metadata`.
///
/// `negotiated` is `self._negotiated`, which is true only on the second crawl
/// of a boot that already reported ready.
///
/// # Errors
/// [`Error::InvalidMetadata`] when neither OVF nor IMDS answers,
/// [`Error::Reportable`] when the VM cannot be identified or the proxy agent
/// check fails, and [`Error::Preprovisioning`] for a PPS boot.
// The order the steps run in is the whole content of this function; splitting
// it into named halves would hide the one thing worth reading.
#[allow(clippy::too_many_lines)]
pub fn crawl_metadata<P: Platform + ?Sized>(
    platform: &mut P,
    data_dir: &Path,
    negotiated: bool,
    reporter: &mut ci_report::Reporter,
    log: &mut ci_log::Logger,
) -> Result<Crawled, Error> {
    let system_uuid = platform.system_uuid().map_err(|message| {
        Error::Reportable(Box::new(super::errors::vm_identification(&message, None)))
    })?;
    let gen1 = platform.is_gen1();
    let vm_id = super::identity::convert_system_uuid_to_vm_id(&system_uuid, gen1, log)
        .ok_or_else(|| {
            Error::Reportable(Box::new(super::errors::vm_identification(
                "invalid system uuid",
                Some(&system_uuid),
            )))
        })?;
    log.info(
        SOURCE,
        &format!("Azure VM ID: {vm_id} System UUID: {system_uuid}"),
    );

    // The defaults matter: a crawl that finds no provisioning media still
    // returns this shape, and the IMDS overrides below write into it.
    let mut crawl = OvfCrawl {
        metadata: [("local-hostname".to_owned(), Value::from(""))]
            .into_iter()
            .collect(),
        userdata: Vec::new(),
        config: serde_json::json!({"system_info": {"default_user": {"name": ""}}})
            .as_object()
            .cloned()
            .unwrap_or_default(),
    };
    let mut files = Object::new();
    let mut ovf_source = None;
    let mut iso_dev = false;

    for source in platform.candidates() {
        match platform.load(&source, log) {
            Ok((loaded, contents)) => {
                crawl = loaded;
                files.insert("ovf-env.xml".to_owned(), blob(&contents));
                iso_dev = matches!(source, Source::Device(_));
                log.debug(
                    SOURCE,
                    &format!("Found provisioning metadata in {}", source.as_str()),
                );
                ovf_source = Some(source);
                break;
            }
            Err(SourceError::NonAzure) => log.debug(
                SOURCE,
                &format!("Did not find Azure data source in {}", source.as_str()),
            ),
            Err(SourceError::MountFailed) => {
                let message = format!("{} was not mountable", source.as_str());
                log.debug(SOURCE, &message);
            }
        }
    }
    if ovf_source.is_none() {
        log.warning(
            SOURCE,
            "Unable to find provisioning media, falling back to IMDS metadata. \
             Be aware that IMDS metadata does not support admin passwords or \
             custom-data (user-data only).",
        );
    }

    // Media that had to be mounted, or none at all, means this is a first boot
    // and IMDS is the only remaining source, so networking is worth waiting on.
    let requires_imds = iso_dev || ovf_source.is_none();
    let timeout_minutes = if requires_imds { 20 } else { 5 };
    let networking_up = platform.setup_ephemeral_networking(timeout_minutes, log);

    let mut imds = Object::new();
    if networking_up {
        if crawl
            .config
            .get("ProvisionGuestProxyAgent")
            .is_some_and(truthy)
        {
            platform
                .check_proxy_agent(log)
                .map_err(|error| Error::Reportable(Box::new(error)))?;
        }
        imds = platform.fetch_imds(true, log);
    }

    if imds.is_empty() && ovf_source.is_none() {
        let message = "No OVF or IMDS available";
        log.debug(SOURCE, message);
        return Err(Error::InvalidMetadata(message.to_owned()));
    }

    let reported_ready = platform.reported_ready_marker();
    let pps_type = ds::determine_pps_type(&crawl.config, &imds, reported_ready, log);
    if pps_type != PpsType::None {
        if !networking_up {
            let message = "DHCP failed while in source PPS";
            log.error(SOURCE, message);
            return Err(Error::InvalidMetadata(message.to_owned()));
        }
        return Err(Error::Preprovisioning(pps_type));
    }

    platform.validate_imds_network_metadata(&imds, log);

    let seed = ovf_source
        .as_ref()
        .map_or_else(|| "IMDS".to_owned(), |source| source.as_str().into_owned());

    // `mergemanydict([md, {"imds": imds}])`. No OVF document can carry an
    // `imds` key, so first-wins never has anything to decide.
    let mut metadata = crawl.metadata.clone();
    metadata.insert("imds".to_owned(), Value::Object(imds.clone()));

    let imds_username = ds::username_from_imds(&imds).map(str::to_owned);
    let imds_hostname = ds::hostname_from_imds(&imds).map(str::to_owned);
    let imds_disable_password = ds::disable_password_from_imds(&imds);
    if let Some(username) = &imds_username {
        log.debug(SOURCE, &format!("Username retrieved from IMDS: {username}"));
        set_default_user_name(&mut crawl.config, username);
    }
    if let Some(hostname) = &imds_hostname {
        log.debug(SOURCE, &format!("Hostname retrieved from IMDS: {hostname}"));
        metadata.insert("local-hostname".to_owned(), Value::from(hostname.clone()));
    }
    if let Some(disabled) = imds_disable_password {
        log.debug(
            SOURCE,
            &format!(
                "Disable password retrieved from IMDS: {}",
                ds::py_bool(disabled)
            ),
        );
        crawl
            .config
            .insert("ssh_pwauth".to_owned(), Value::Bool(!disabled));
    }

    if seed == "IMDS" && files.is_empty() {
        let contents = super::wire::build_minimal_ovf(
            imds_username.as_deref(),
            imds_hostname.as_deref().unwrap_or(""),
            imds_disable_password,
        );
        files.insert("ovf-env.xml".to_owned(), blob(&contents));
    }

    // IMDS user data is always base64, and only fills in for an OVF that
    // carried none.
    if crawl.userdata.is_empty() {
        if let Some(encoded) = ds::userdata_from_imds(&imds) {
            log.debug(SOURCE, "Retrieved userdata from IMDS");
            let stripped: String =
                encoded.split_whitespace().collect::<Vec<_>>().concat();
            match ci_core::b64::decode(&stripped) {
                Some(decoded) => crawl.userdata = decoded,
                None => log.warning(SOURCE, "Bad userdata in IMDS"),
            }
        }
    }

    if ovf_source.as_ref().is_some_and(|source| match source {
        Source::Dir(path) => path == data_dir,
        Source::Device(_) => false,
    }) {
        log.debug(
            SOURCE,
            &format!("using files cached in {}", data_dir.display()),
        );
    }

    if let Some(seed_value) = platform.random_seed() {
        metadata.insert("random_seed".to_owned(), Value::from(seed_value));
    }
    let previous = platform.previous_instance_id();
    metadata.insert(
        "instance-id".to_owned(),
        Value::from(ds::iid(&system_uuid, previous.as_deref(), log)),
    );

    if !negotiated && networking_up {
        let pubkey_info = ds::wireserver_pubkey_info(&crawl.config, &imds, log);
        // `_report_ready` tells the host it succeeded before it asks the
        // fabric for anything, so a VM that then fails to reach wireserver has
        // still said so through the channel that does not need a network.
        super::kvp::report_success_to_host(reporter, Some(&vm_id), log);
        // Upstream swallows every failure here: a VM that cannot report ready
        // has bigger problems than the keys it came for.
        if let Ok(keys) = platform.report_ready(pubkey_info.as_deref(), log) {
            log.debug(SOURCE, &format!("negotiating returned {keys:?}"));
            if !keys.is_empty() {
                metadata.insert(
                    "public-keys".to_owned(),
                    Value::Array(keys.into_iter().map(Value::from).collect()),
                );
            }
            platform.cleanup_markers(log);
        }
    }

    Ok(Crawled {
        cfg: crawl.config,
        files,
        metadata,
        userdata: crawl.userdata,
        seed,
    })
}

/// Python truthiness for the one OVF flag the crawl branches on.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::String(text) => !text.is_empty(),
        Value::Null => false,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// `cfg["system_info"]["default_user"]["name"] = username`, which upstream can
/// assume exists because it built the default itself.
fn set_default_user_name(cfg: &mut Object, username: &str) {
    let name = cfg
        .entry("system_info")
        .or_insert_with(|| Value::Object(Object::new()))
        .as_object_mut()
        .and_then(|info| {
            info.entry("default_user")
                .or_insert_with(|| Value::Object(Object::new()))
                .as_object_mut()
        });
    if let Some(user) = name {
        user.insert("name".to_owned(), Value::from(username));
    }
}

/// A [`Platform`] whose every answer is set up front.
///
/// This is what `dump-azure crawl` drives, so the differential can compare the
/// sequencing against upstream without either side touching a real VM.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub system_uuid: Result<String, String>,
    pub gen1: bool,
    pub candidates: Vec<Source>,
    /// Sources that yield an OVF, by their string form, holding the raw
    /// document so it goes through the real parser.
    pub sources: Vec<(String, String)>,
    /// Sources that fail to mount rather than simply holding no OVF.
    pub unmountable: Vec<String>,
    pub networking_up: bool,
    pub imds: Object,
    pub reported_ready_marker: bool,
    pub report_ready: Result<Vec<String>, String>,
    pub previous_instance_id: Option<String>,
    pub random_seed: Option<String>,
    /// What the crawl asked for, in order, so the differential can compare the
    /// calls and not just the result.
    pub calls: Vec<String>,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            system_uuid: Ok(String::new()),
            gen1: false,
            candidates: Vec::new(),
            sources: Vec::new(),
            unmountable: Vec::new(),
            networking_up: false,
            imds: Object::new(),
            reported_ready_marker: false,
            report_ready: Ok(Vec::new()),
            previous_instance_id: None,
            random_seed: None,
            calls: Vec::new(),
        }
    }
}

impl Platform for Fixture {
    fn system_uuid(&mut self) -> Result<String, String> {
        self.calls.push("system_uuid".to_owned());
        self.system_uuid.clone()
    }

    fn is_gen1(&mut self) -> bool {
        self.gen1
    }

    fn candidates(&mut self) -> Vec<Source> {
        self.calls.push("candidates".to_owned());
        self.candidates.clone()
    }

    fn load(
        &mut self,
        source: &Source,
        log: &mut ci_log::Logger,
    ) -> Result<(OvfCrawl, Vec<u8>), SourceError> {
        let key = source.as_str().into_owned();
        self.calls.push(format!("load({key})"));
        if self.unmountable.contains(&key) {
            return Err(SourceError::MountFailed);
        }
        let text = self
            .sources
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, text)| text.clone())
            .ok_or(SourceError::NonAzure)?;
        let parsed =
            ds::read_azure_ovf(&text, log).map_err(|_| SourceError::NonAzure)?;
        Ok((parsed, text.into_bytes()))
    }

    fn setup_ephemeral_networking(
        &mut self,
        timeout_minutes: u64,
        _log: &mut ci_log::Logger,
    ) -> bool {
        self.calls.push(format!("dhcp({timeout_minutes})"));
        self.networking_up
    }

    fn fetch_imds(
        &mut self,
        report_failure: bool,
        _log: &mut ci_log::Logger,
    ) -> Object {
        self.calls
            .push(format!("imds({})", ds::py_bool(report_failure)));
        self.imds.clone()
    }

    fn report_ready(
        &mut self,
        pubkey_info: Option<&[Value]>,
        _log: &mut ci_log::Logger,
    ) -> Result<Vec<String>, String> {
        self.calls.push(format!(
            "report_ready({})",
            pubkey_info.map_or(0, <[Value]>::len)
        ));
        self.report_ready.clone()
    }

    fn previous_instance_id(&mut self) -> Option<String> {
        self.previous_instance_id.clone()
    }

    fn random_seed(&mut self) -> Option<String> {
        self.random_seed.clone()
    }

    fn reported_ready_marker(&mut self) -> bool {
        self.reported_ready_marker
    }

    fn cleanup_markers(&mut self, _log: &mut ci_log::Logger) {
        self.calls.push("cleanup_markers".to_owned());
    }

    fn check_proxy_agent(
        &mut self,
        _log: &mut ci_log::Logger,
    ) -> Result<(), ReportableError> {
        self.calls.push("proxy_agent".to_owned());
        Ok(())
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
    use super::{crawl_metadata, Crawled, Error, Fixture, Source};
    use ci_config::{Object, Value};

    const UUID: &str = "8c9e6a3a-1b2c-4d5e-8f90-0a1b2c3d4e5f";

    fn fixture() -> Fixture {
        Fixture {
            system_uuid: Ok(UUID.to_owned()),
            networking_up: true,
            report_ready: Ok(Vec::new()),
            ..Fixture::default()
        }
    }

    /// An OVF the reader accepts, with the two fields the tests look at.
    fn ovf_text() -> String {
        concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<Environment xmlns="http://schemas.dmtf.org/ovf/environment/1""#,
            r#" xmlns:wa="http://schemas.microsoft.com/windowsazure">"#,
            "<wa:ProvisioningSection><wa:Version>1.0</wa:Version>",
            r#"<LinuxProvisioningConfigurationSet"#,
            r#" xmlns="http://schemas.microsoft.com/windowsazure">"#,
            "<ConfigurationSetType>LinuxProvisioningConfiguration",
            "</ConfigurationSetType>",
            "<HostName>from-ovf</HostName>",
            "<UserName>ovfuser</UserName>",
            "<CustomData>IyEvYmluL3NoCnRydWUK</CustomData>",
            "</LinuxProvisioningConfigurationSet>",
            "</wa:ProvisioningSection>",
            "<wa:PlatformSettingsSection><wa:Version>1.0</wa:Version>",
            r#"<PlatformSettings"#,
            r#" xmlns="http://schemas.microsoft.com/windowsazure">"#,
            "<PreprovisionedVm>false</PreprovisionedVm>",
            "</PlatformSettings></wa:PlatformSettingsSection></Environment>",
        )
        .to_owned()
    }

    fn imds_with(compute: &Value) -> Object {
        serde_json::json!({"compute": compute})
            .as_object()
            .cloned()
            .unwrap()
    }

    fn run(platform: &mut Fixture) -> Result<Crawled, Error> {
        crawl_metadata(
            platform,
            std::path::Path::new("/var/lib/waagent"),
            false,
            &mut ci_report::Reporter::silent(),
            &mut ci_log::Logger::silent(),
        )
    }

    #[test]
    fn no_ovf_and_no_imds_is_the_one_fatal_combination() {
        let mut platform = fixture();

        let error = run(&mut platform).unwrap_err();

        assert!(matches!(error, Error::InvalidMetadata(ref m)
            if m == "No OVF or IMDS available"));
    }

    #[test]
    fn imds_alone_synthesises_the_ovf_that_was_never_delivered() {
        let mut platform = Fixture {
            imds: imds_with(&serde_json::json!({
                "osProfile": {
                    "adminUsername": "azureuser",
                    "computerName": "vm-1",
                    "disablePasswordAuthentication": "true",
                },
            })),
            ..fixture()
        };

        let crawled = run(&mut platform).unwrap();

        assert_eq!(crawled.seed, "IMDS");
        assert_eq!(crawled.metadata["local-hostname"], Value::from("vm-1"));
        assert_eq!(
            crawled.cfg["system_info"]["default_user"]["name"],
            Value::from("azureuser")
        );
        // `disablePasswordAuthentication` is inverted on the way in.
        assert_eq!(crawled.cfg["ssh_pwauth"], Value::Bool(false));
        // `files` holds the document as bytes, so it reads back through b64.
        let stored = crawled.files["ovf-env.xml"].as_str().unwrap();
        let raw =
            ci_core::b64::decode(stored.strip_prefix("ci-b64:").unwrap()).unwrap();
        let ovf = String::from_utf8(raw).unwrap();
        assert!(
            ovf.contains("<ns1:UserName>azureuser</ns1:UserName>"),
            "{ovf}"
        );
        assert!(ovf.contains("<ns1:HostName>vm-1</ns1:HostName>"), "{ovf}");
    }

    #[test]
    fn provisioning_media_wins_over_imds_and_stops_the_search() {
        let mut platform = Fixture {
            candidates: vec![
                Source::Dir("/var/lib/cloud/seed/azure".into()),
                Source::Device("/dev/sr0".into()),
                Source::Dir("/var/lib/waagent".into()),
            ],
            sources: vec![("/dev/sr0".to_owned(), ovf_text())],
            imds: imds_with(&serde_json::json!({
                "osProfile": {"computerName": "from-imds"},
                "userData": "aWdub3JlZA==",
            })),
            ..fixture()
        };

        let crawled = run(&mut platform).unwrap();

        assert_eq!(crawled.seed, "/dev/sr0");
        // IMDS still overrides the hostname, but not the user data.
        assert_eq!(crawled.metadata["local-hostname"], Value::from("from-imds"));
        assert_eq!(crawled.userdata, b"#!/bin/sh\ntrue\n");
        // The third candidate is never consulted.
        assert!(!platform.calls.iter().any(|c| c == "load(/var/lib/waagent)"));
        // Media that had to be mounted means a first boot: wait the full 20.
        assert!(
            platform.calls.iter().any(|c| c == "dhcp(20)"),
            "{:?}",
            platform.calls
        );
    }

    #[test]
    fn a_cached_seed_directory_only_waits_five_minutes_for_a_lease() {
        let mut platform = Fixture {
            candidates: vec![Source::Dir("/var/lib/waagent".into())],
            sources: vec![("/var/lib/waagent".to_owned(), ovf_text())],
            ..fixture()
        };

        run(&mut platform).unwrap();

        assert!(
            platform.calls.iter().any(|c| c == "dhcp(5)"),
            "{:?}",
            platform.calls
        );
    }

    #[test]
    fn reporting_ready_contributes_the_keys_wireserver_holds() {
        let mut platform = Fixture {
            imds: imds_with(&serde_json::json!({"osProfile": {"computerName": "vm"}})),
            report_ready: Ok(vec!["ssh-rsa AAAA".to_owned()]),
            ..fixture()
        };

        let crawled = run(&mut platform).unwrap();

        assert_eq!(
            crawled.metadata["public-keys"],
            serde_json::json!(["ssh-rsa AAAA"])
        );
        assert!(platform.calls.iter().any(|c| c == "cleanup_markers"));
    }

    #[test]
    fn a_failure_to_report_ready_is_swallowed_and_leaves_the_markers_alone() {
        let mut platform = Fixture {
            imds: imds_with(&serde_json::json!({"osProfile": {"computerName": "vm"}})),
            report_ready: Err("wireserver unreachable".to_owned()),
            ..fixture()
        };

        let crawled = run(&mut platform).unwrap();

        assert!(!crawled.metadata.contains_key("public-keys"));
        assert!(!platform.calls.iter().any(|c| c == "cleanup_markers"));
    }

    #[test]
    fn without_a_lease_imds_is_never_asked() {
        let mut platform = Fixture {
            candidates: vec![Source::Dir("/var/lib/waagent".into())],
            sources: vec![("/var/lib/waagent".to_owned(), ovf_text())],
            networking_up: false,
            ..fixture()
        };

        let crawled = run(&mut platform).unwrap();

        assert!(!platform.calls.iter().any(|c| c.starts_with("imds(")));
        assert!(!platform
            .calls
            .iter()
            .any(|c| c.starts_with("report_ready(")));
        assert_eq!(crawled.metadata["imds"], serde_json::json!({}));
    }

    #[test]
    fn the_instance_id_survives_a_byte_swapped_previous_boot() {
        let mut platform = Fixture {
            imds: imds_with(&serde_json::json!({"osProfile": {"computerName": "vm"}})),
            previous_instance_id: Some(UUID.to_uppercase()),
            ..fixture()
        };

        let crawled = run(&mut platform).unwrap();

        assert_eq!(
            crawled.metadata["instance-id"],
            Value::from(UUID.to_uppercase())
        );
    }

    #[test]
    fn preprovisioning_is_detected_and_refused_rather_than_half_done() {
        let mut platform = Fixture {
            imds: serde_json::json!({
                "compute": {"osProfile": {"computerName": "vm"}},
                "extended": {"compute": {"ppsType": "Savable"}},
            })
            .as_object()
            .cloned()
            .unwrap(),
            ..fixture()
        };

        let error = run(&mut platform).unwrap_err();

        assert!(matches!(
            error,
            Error::Preprovisioning(super::PpsType::Savable)
        ));
        // Nothing was reported ready: the platform would take that as a
        // finished provision.
        assert!(!platform
            .calls
            .iter()
            .any(|c| c.starts_with("report_ready(")));
    }

    #[test]
    fn an_unmountable_device_is_stepped_over() {
        let mut platform = Fixture {
            candidates: vec![
                Source::Device("/dev/sr0".into()),
                Source::Dir("/var/lib/waagent".into()),
            ],
            unmountable: vec!["/dev/sr0".to_owned()],
            sources: vec![("/var/lib/waagent".to_owned(), ovf_text())],
            ..fixture()
        };

        let crawled = run(&mut platform).unwrap();

        assert_eq!(crawled.seed, "/var/lib/waagent");
    }
}
