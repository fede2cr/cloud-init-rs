//! Port of `OvfEnvXml` from `sources/helpers/azure.py`: the `ovf-env.xml`
//! document Azure writes onto the provisioning media.

use ci_config::xml::{self, Element};

const OVF_NS: &str = "http://schemas.dmtf.org/ovf/environment/1";
const WA_NS: &str = "http://schemas.microsoft.com/windowsazure";
const SOURCE: &str = "azure.py";

/// One entry of the `PublicKeys` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub fingerprint: Option<String>,
    pub path: Option<String>,
    pub value: String,
}

/// The provisioning settings carried by `ovf-env.xml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OvfEnv {
    pub username: Option<String>,
    pub password: Option<String>,
    pub hostname: Option<String>,
    pub custom_data: Option<Vec<u8>>,
    pub disable_ssh_password_auth: Option<bool>,
    pub public_keys: Vec<PublicKey>,
    pub preprovisioned_vm: bool,
    pub preprovisioned_vm_type: Option<String>,
    pub provision_guest_proxy_agent: bool,
}

/// Why a document was rejected.
///
/// The split matters to the caller: `NonAzure` means "look elsewhere for a
/// datasource", while the other two are failures worth reporting to the
/// platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Upstream's `NonAzureDataSource`.
    NonAzure(String),
    /// Upstream's `ReportableErrorOvfParsingException`.
    Parsing(String),
    /// Upstream's `ReportableErrorOvfInvalidMetadata`.
    InvalidMetadata(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonAzure(message) => f.write_str(message),
            Self::Parsing(message) => {
                write!(f, "error parsing ovf-env.xml: {message}")
            }
            Self::InvalidMetadata(message) => {
                write!(f, "unexpected metadata parsing ovf-env.xml: {message}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Read an `ovf-env.xml` document.
///
/// # Errors
/// The document is not XML, is not an Azure environment, or carries metadata
/// that cannot be understood.
pub fn parse_text(text: &str, log: &mut ci_log::Logger) -> Result<OvfEnv, Error> {
    let root = xml::parse(text, xml::Limits::default())
        .map_err(|reason| Error::Parsing(reason.to_string()))?;

    if find(&root, "ProvisioningSection", false, WA_NS, log)?.is_none() {
        return Err(Error::NonAzure(
            "Ignoring non-Azure ovf-env.xml: ProvisioningSection not found".to_owned(),
        ));
    }

    let mut env = OvfEnv::default();
    parse_linux_configuration_set_section(&mut env, &root, log)?;
    parse_platform_settings_section(&mut env, &root, log)?;
    Ok(env)
}

fn parse_linux_configuration_set_section(
    env: &mut OvfEnv,
    root: &Element,
    log: &mut ci_log::Logger,
) -> Result<(), Error> {
    let provisioning = require(root, "ProvisioningSection", log)?;
    let config_set = require(provisioning, "LinuxProvisioningConfigurationSet", log)?;

    env.custom_data = property_base64(config_set, "CustomData", log)?;
    env.username = property_text(config_set, "UserName", false, log)?;
    env.password = property_text(config_set, "UserPassword", false, log)?;
    env.hostname = property_text(config_set, "HostName", true, log)?;
    env.disable_ssh_password_auth =
        property_bool(config_set, "DisableSshPasswordAuthentication", None, log)?;

    parse_ssh_section(env, config_set, log)
}

fn parse_platform_settings_section(
    env: &mut OvfEnv,
    root: &Element,
    log: &mut ci_log::Logger,
) -> Result<(), Error> {
    let section = require(root, "PlatformSettingsSection", log)?;
    let settings = require(section, "PlatformSettings", log)?;

    env.preprovisioned_vm =
        property_bool(settings, "PreprovisionedVm", Some(false), log)?.unwrap_or(false);
    env.preprovisioned_vm_type =
        property_text(settings, "PreprovisionedVMType", false, log)?;
    env.provision_guest_proxy_agent =
        property_bool(settings, "ProvisionGuestProxyAgent", Some(false), log)?
            .unwrap_or(false);
    Ok(())
}

fn parse_ssh_section(
    env: &mut OvfEnv,
    config_set: &Element,
    log: &mut ci_log::Logger,
) -> Result<(), Error> {
    let Some(ssh) = find(config_set, "SSH", false, WA_NS, log)? else {
        return Ok(());
    };
    let Some(keys) = find(ssh, "PublicKeys", false, WA_NS, log)? else {
        return Ok(());
    };

    for key in keys.children_named(WA_NS, "PublicKey") {
        env.public_keys.push(PublicKey {
            fingerprint: property_text(key, "Fingerprint", false, log)?,
            path: property_text(key, "Path", false, log)?,
            value: property_text(key, "Value", false, log)?.unwrap_or_default(),
        });
    }
    Ok(())
}

/// `_find` with `required=True`.
fn require<'a>(
    node: &'a Element,
    name: &str,
    log: &mut ci_log::Logger,
) -> Result<&'a Element, Error> {
    find(node, name, true, WA_NS, log)?
        .ok_or_else(|| Error::InvalidMetadata(missing(name)))
}

/// `_find`: exactly zero or one direct child, or a hard error.
fn find<'a>(
    node: &'a Element,
    name: &str,
    required: bool,
    namespace: &str,
    log: &mut ci_log::Logger,
) -> Result<Option<&'a Element>, Error> {
    let mut matches = node.children_named(namespace, name);
    let Some(first) = matches.next() else {
        let message = missing(name);
        log.debug(SOURCE, &message);
        if required {
            return Err(Error::InvalidMetadata(message));
        }
        return Ok(None);
    };
    let extra = matches.count();
    if extra > 0 {
        return Err(Error::InvalidMetadata(multiple(name, extra + 1)));
    }
    Ok(Some(first))
}

/// `_parse_property` without any of the conversions.
fn property_text(
    node: &Element,
    name: &str,
    required: bool,
    log: &mut ci_log::Logger,
) -> Result<Option<String>, Error> {
    Ok(find(node, name, required, WA_NS, log)?.and_then(|e| e.text.clone()))
}

/// `_parse_property(parse_bool=True)`.
///
/// A missing element yields `default` untouched, but an element that is present
/// and empty is run through `translate_bool`, which turns it into `false`
/// whatever the default was.
fn property_bool(
    node: &Element,
    name: &str,
    default: Option<bool>,
    log: &mut ci_log::Logger,
) -> Result<Option<bool>, Error> {
    let Some(element) = find(node, name, false, WA_NS, log)? else {
        return Ok(default);
    };
    let value = element.text.clone().unwrap_or_default();
    Ok(Some(ci_config::option::translate_bool(
        &ci_config::Value::String(value),
    )))
}

/// `_parse_property(decode_base64=True)`.
fn property_base64(
    node: &Element,
    name: &str,
    log: &mut ci_log::Logger,
) -> Result<Option<Vec<u8>>, Error> {
    let Some(element) = find(node, name, false, WA_NS, log)? else {
        return Ok(None);
    };
    let Some(text) = element.text.as_deref() else {
        return Ok(None);
    };
    let packed: String = text.split_whitespace().collect();
    // Upstream lets binascii's error escape as an unhandled exception here
    // (bug B45); classify it the way every other bad field is classified.
    decode_base64(&packed).map(Some).ok_or_else(|| {
        Error::InvalidMetadata(format!("invalid base64 for {}", repr(name)))
    })
}

/// `base64.b64decode` without `validate`: characters outside the alphabet are
/// dropped, but the padding still has to add up.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let kept: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    let data = kept.chars().filter(|c| *c != '=').count();
    if data % 4 == 1 || kept.len() % 4 != 0 {
        return None;
    }
    ci_core::b64::decode(&kept)
}

fn missing(name: &str) -> String {
    format!("missing configuration for {}", repr(name))
}

fn multiple(name: &str, count: usize) -> String {
    format!(
        "multiple configuration matches for {} ({count})",
        repr(name)
    )
}

/// Python's `%r` for a plain name.
fn repr(name: &str) -> String {
    format!("'{name}'")
}

/// The namespace URIs, exposed so callers can build documents in tests.
#[must_use]
pub fn namespaces() -> (&'static str, &'static str) {
    (OVF_NS, WA_NS)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{parse_text, Error};

    fn parse_with(text: &str) -> Result<super::OvfEnv, Error> {
        parse_text(text, &mut ci_log::Logger::silent())
    }

    fn document(config_set: &str, platform: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <Environment \
             xmlns=\"http://schemas.dmtf.org/ovf/environment/1\" \
             xmlns:wa=\"http://schemas.microsoft.com/windowsazure\">\
             <wa:ProvisioningSection>\
             <wa:LinuxProvisioningConfigurationSet>{config_set}\
             </wa:LinuxProvisioningConfigurationSet>\
             </wa:ProvisioningSection>\
             <wa:PlatformSettingsSection>\
             <wa:PlatformSettings>{platform}</wa:PlatformSettings>\
             </wa:PlatformSettingsSection></Environment>"
        )
    }

    fn parse(config_set: &str, platform: &str) -> Result<super::OvfEnv, Error> {
        parse_with(&document(config_set, platform))
    }

    #[test]
    fn a_full_document_yields_every_field() {
        let env = parse(
            "<wa:HostName>vm1</wa:HostName>\
             <wa:UserName>azureuser</wa:UserName>\
             <wa:UserPassword>secret</wa:UserPassword>\
             <wa:CustomData>dGVzdA==</wa:CustomData>\
             <wa:DisableSshPasswordAuthentication>true\
             </wa:DisableSshPasswordAuthentication>\
             <wa:SSH><wa:PublicKeys>\
             <wa:PublicKey><wa:Fingerprint>AA</wa:Fingerprint>\
             <wa:Path>/root/.ssh/authorized_keys</wa:Path></wa:PublicKey>\
             <wa:PublicKey><wa:Value>ssh-rsa AAAA</wa:Value></wa:PublicKey>\
             </wa:PublicKeys></wa:SSH>",
            "<wa:PreprovisionedVm>true</wa:PreprovisionedVm>\
             <wa:PreprovisionedVMType>Savable</wa:PreprovisionedVMType>\
             <wa:ProvisionGuestProxyAgent>1</wa:ProvisionGuestProxyAgent>",
        )
        .unwrap();

        assert_eq!(env.hostname.as_deref(), Some("vm1"));
        assert_eq!(env.username.as_deref(), Some("azureuser"));
        assert_eq!(env.password.as_deref(), Some("secret"));
        assert_eq!(env.custom_data.as_deref(), Some(&b"test"[..]));
        assert_eq!(env.disable_ssh_password_auth, Some(true));
        assert!(env.preprovisioned_vm);
        assert_eq!(env.preprovisioned_vm_type.as_deref(), Some("Savable"));
        assert!(env.provision_guest_proxy_agent);

        assert_eq!(env.public_keys.len(), 2);
        assert_eq!(env.public_keys[0].fingerprint.as_deref(), Some("AA"));
        assert_eq!(env.public_keys[0].value, "");
        assert_eq!(env.public_keys[1].value, "ssh-rsa AAAA");
        assert_eq!(env.public_keys[1].path, None);
    }

    #[test]
    fn a_document_without_a_provisioning_section_is_not_azure() {
        let blob = "<Environment \
                    xmlns:wa=\"http://schemas.microsoft.com/windowsazure\"/>";
        let error = parse_with(blob).unwrap_err();
        assert!(matches!(error, Error::NonAzure(_)));
        assert_eq!(
            error.to_string(),
            "Ignoring non-Azure ovf-env.xml: ProvisioningSection not found"
        );
    }

    #[test]
    fn a_missing_required_field_reports_which_one() {
        let error = parse("<wa:UserName>u</wa:UserName>", "").unwrap_err();
        assert_eq!(
            error.to_string(),
            "unexpected metadata parsing ovf-env.xml: \
             missing configuration for 'HostName'"
        );
    }

    #[test]
    fn duplicate_elements_are_refused_with_a_count() {
        let error = parse(
            "<wa:HostName>a</wa:HostName><wa:HostName>b</wa:HostName>",
            "",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unexpected metadata parsing ovf-env.xml: \
             multiple configuration matches for 'HostName' (2)"
        );
    }

    #[test]
    fn defaults_apply_when_the_optional_fields_are_absent() {
        let env = parse("<wa:HostName>vm1</wa:HostName>", "").unwrap();
        assert_eq!(env.custom_data, None);
        assert_eq!(env.disable_ssh_password_auth, None);
        assert!(!env.preprovisioned_vm);
        assert!(!env.provision_guest_proxy_agent);
        assert!(env.public_keys.is_empty());
    }

    #[test]
    fn an_empty_boolean_element_is_false_rather_than_absent() {
        let env = parse(
            "<wa:HostName>vm1</wa:HostName>\
             <wa:DisableSshPasswordAuthentication/>",
            "",
        )
        .unwrap();
        assert_eq!(env.disable_ssh_password_auth, Some(false));
    }

    #[test]
    fn custom_data_is_unwrapped_before_it_is_decoded() {
        let env = parse(
            "<wa:HostName>vm1</wa:HostName>\
             <wa:CustomData>dGVz\n  dGlu\tZw==</wa:CustomData>",
            "",
        )
        .unwrap();
        assert_eq!(env.custom_data.as_deref(), Some(&b"testing"[..]));
    }

    #[test]
    fn an_empty_custom_data_element_is_absent_not_empty() {
        let env = parse("<wa:HostName>vm1</wa:HostName><wa:CustomData/>", "").unwrap();
        assert_eq!(env.custom_data, None);
    }

    #[test]
    fn malformed_custom_data_is_reported_rather_than_escaping() {
        let error = parse(
            "<wa:HostName>vm1</wa:HostName>\
             <wa:CustomData>dGVzdA=</wa:CustomData>",
            "",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unexpected metadata parsing ovf-env.xml: \
             invalid base64 for 'CustomData'"
        );
    }

    #[test]
    fn a_document_that_is_not_xml_is_a_parsing_error() {
        let error = parse_with("not xml at all").unwrap_err();
        assert!(matches!(error, Error::Parsing(_)));
        assert!(error.to_string().starts_with("error parsing ovf-env.xml: "));
    }
}
