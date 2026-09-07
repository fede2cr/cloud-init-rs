//! `sources/helpers/azure.py`, the wireserver half: fetching the goal state
//! and reporting provisioning health back to the fabric.
//!
//! The certificate exchange itself lives in [`super::certs`]; this module
//! carries the transport certificate into the one request that needs it.

use std::time::Duration;

use ci_config::{xml, Value};

/// `AzureEndpointHttpClient.headers`.
const AGENT_NAME: &str = "WALinuxAgent";
const WIRE_VERSION: &str = "2012-11-30";
/// `extra_secure_headers`' cipher. The wireserver picks the algorithm it
/// encrypts the PKCS#7 blob with from this, so it is not decoration.
const CIPHER_NAME: &str = "DES_EDE3_CBC";
/// `%(filename)s` for everything in this module.
const SOURCE: &str = "azure.py";

/// `http_with_retries`' defaults.
const RETRY_SLEEP: Duration = Duration::from_secs(1);
const TIMEOUT_MINUTES: u64 = 20;
/// `readurl(timeout=(5, 60))`; the port has one timeout, so it takes the read
/// half — the connect half is the shorter of the two and would cut transfers.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

const SUCCESS_STATUS: &str = "Ready";
const NOT_READY_STATUS: &str = "NotReady";
const FAILURE_SUBSTATUS: &str = "ProvisioningFailed";
/// `HEALTH_REPORT_DESCRIPTION_TRIM_LEN`.
const DESCRIPTION_TRIM_LEN: usize = 512;

/// `DEFAULT_WIRESERVER_ENDPOINT`.
///
/// Every Azure fabric answers here, which is why upstream can start from the
/// constant and only correct it when a lease says otherwise.
pub const DEFAULT_ENDPOINT: &str = "168.63.129.16";

/// The wireserver address a DHCP lease carries, in option 245.
///
/// `_setup_ephemeral_networking` spells this as an assignment into
/// `self._wireserver_endpoint`; as a function it can be applied to a lease that
/// came from anywhere, including one read back off disk.
#[must_use]
pub fn endpoint_from_lease(lease: &ci_config::Object) -> Option<&str> {
    lease.get("unknown-245").and_then(ci_config::Value::as_str)
}

/// What the wireserver told us about this VM's provisioning slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalState {
    pub incarnation: String,
    pub container_id: String,
    pub instance_id: String,
    /// The URL the certificates document is fetched from, present whether or
    /// not this run asked for it.
    pub certificates_url: Option<String>,
    /// The document itself, once a transport certificate has been offered.
    /// `None` is upstream's `need_certificate=False`.
    pub certificates_xml: Option<String>,
}

/// `InvalidGoalStateXMLException`, plus the transport failures around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Parsing(String),
    InvalidGoalState(String),
    Transport(String),
    Certificate(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parsing(m) => write!(f, "Failed to parse GoalState XML: {m}"),
            Self::InvalidGoalState(m) | Self::Transport(m) | Self::Certificate(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl GoalState {
    /// `GoalState.__init__`'s parsing half.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let root = xml::parse(text, xml::Limits::default())
            .map_err(|e| Error::Parsing(e.to_string()))?;

        let text_at =
            |path: &[&str]| root.find_path(path).and_then(|node| node.text.clone());

        // Upstream checks the three in this order and names the Python
        // attribute, not the element, in the message.
        let container_id = text_at(&["Container", "ContainerId"]);
        let instance_id = text_at(&[
            "Container",
            "RoleInstanceList",
            "RoleInstance",
            "InstanceId",
        ]);
        let incarnation = text_at(&["Incarnation"]);
        for (name, value) in [
            ("container_id", &container_id),
            ("instance_id", &instance_id),
            ("incarnation", &incarnation),
        ] {
            if value.is_none() {
                return Err(Error::InvalidGoalState(format!(
                    "Missing {name} in GoalState XML"
                )));
            }
        }

        Ok(Self {
            incarnation: incarnation.unwrap_or_default(),
            container_id: container_id.unwrap_or_default(),
            instance_id: instance_id.unwrap_or_default(),
            certificates_url: text_at(&[
                "Container",
                "RoleInstanceList",
                "RoleInstance",
                "Configuration",
                "Certificates",
            ]),
            certificates_xml: None,
        })
    }
}

/// `xml.sax.saxutils.escape`, which leaves quotes alone because every value
/// here is element text rather than an attribute.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// `GoalStateHealthReporter.build_report`. The odd indentation of the details
/// block and the blank line after it are upstream's, and the wireserver does
/// not care, so they are reproduced rather than tidied.
#[must_use]
pub fn build_report(
    goal_state: &GoalState,
    status: &str,
    substatus: Option<&str>,
    description: &str,
) -> Vec<u8> {
    let detail = match substatus {
        None => String::new(),
        Some(substatus) => {
            let trimmed: String =
                description.chars().take(DESCRIPTION_TRIM_LEN).collect();
            format!(
                "<Details>\n  <SubStatus>{}</SubStatus>\n  \
                 <Description>{}</Description>\n</Details>\n",
                escape(substatus),
                escape(&trimmed),
            )
        }
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <Health xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"\n\
         \x20xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">\n\
         \x20 <GoalStateIncarnation>{}</GoalStateIncarnation>\n\
         \x20 <Container>\n\
         \x20   <ContainerId>{}</ContainerId>\n\
         \x20   <RoleInstanceList>\n\
         \x20     <Role>\n\
         \x20       <InstanceId>{}</InstanceId>\n\
         \x20       <Health>\n\
         \x20         <State>{}</State>\n\
         \x20         {}\n\
         \x20       </Health>\n\
         \x20     </Role>\n\
         \x20   </RoleInstanceList>\n\
         \x20 </Container>\n\
         </Health>\n",
        escape(&goal_state.incarnation),
        escape(&goal_state.container_id),
        escape(&goal_state.instance_id),
        escape(status),
        detail,
    )
    .into_bytes()
}

/// `build_minimal_ovf`, the document synthesised for a savable preprovisioned
/// VM that never got one from the platform.
#[must_use]
pub fn build_minimal_ovf(
    username: Option<&str>,
    hostname: &str,
    disable_ssh_password_auth: Option<bool>,
) -> Vec<u8> {
    // Upstream interpolates into a `textwrap.dedent`ed f-string, and dedent
    // normalises a line left holding only whitespace to an empty one.
    let line = |body: String| {
        if body.is_empty() {
            String::new()
        } else {
            format!("      {body}")
        }
    };
    let ns_username = line(
        username
            .map_or_else(String::new, |u| format!("<ns1:UserName>{u}</ns1:UserName>")),
    );
    let ns_disable =
        line(disable_ssh_password_auth.map_or_else(String::new, |value| {
            format!(
                "<ns1:DisableSshPasswordAuthentication>{value}\
                 </ns1:DisableSshPasswordAuthentication>"
            )
        }));
    format!(
        "<ns0:Environment xmlns:ns0=\"http://schemas.dmtf.org/ovf/environment/1\"\n\
         \x20xmlns:ns1=\"http://schemas.microsoft.com/windowsazure\"\n\
         \x20xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n\
         \x20 <ns1:ProvisioningSection>\n\
         \x20   <ns1:Version>1.0</ns1:Version>\n\
         \x20   <ns1:LinuxProvisioningConfigurationSet>\n\
         \x20     <ns1:ConfigurationSetType>LinuxProvisioningConfiguration\n\
         \x20     </ns1:ConfigurationSetType>\n\
         {ns_username}\n\
         {ns_disable}\n\
         \x20     <ns1:HostName>{hostname}</ns1:HostName>\n\
         \x20   </ns1:LinuxProvisioningConfigurationSet>\n\
         \x20 </ns1:ProvisioningSection>\n\
         \x20 <ns1:PlatformSettingsSection>\n\
         \x20   <ns1:Version>1.0</ns1:Version>\n\
         \x20   <ns1:PlatformSettings>\n\
         \x20     <ns1:ProvisionGuestAgent>true</ns1:ProvisionGuestAgent>\n\
         \x20   </ns1:PlatformSettings>\n\
         \x20 </ns1:PlatformSettingsSection>\n\
         </ns0:Environment>\n"
    )
    .into_bytes()
}

/// `AzureEndpointHttpClient`.
#[derive(Debug, Clone)]
pub struct Client {
    endpoint: String,
    timeout: Duration,
    retry_sleep: Duration,
    /// `extra_secure_headers`' certificate. `None` is upstream constructing
    /// the client with `http_client_certificate = None`.
    certificate: Option<String>,
}

impl Client {
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeout: Duration::from_secs(TIMEOUT_MINUTES * 60),
            retry_sleep: RETRY_SLEEP,
            certificate: None,
        }
    }

    /// Offer a transport certificate, which is what makes `comp=certificates`
    /// answer at all.
    #[must_use]
    pub fn with_certificate(mut self, certificate: impl Into<String>) -> Self {
        self.certificate = Some(certificate.into());
        self
    }

    /// Shorten the retry budget. The differential needs a run that finishes.
    #[must_use]
    pub fn with_budget(mut self, timeout: Duration, retry_sleep: Duration) -> Self {
        self.timeout = timeout;
        self.retry_sleep = retry_sleep;
        self
    }

    fn config(method: ci_url::Method, body: Vec<u8>) -> ci_url::Config {
        let mut headers = vec![
            ("x-ms-agent-name".to_owned(), AGENT_NAME.to_owned()),
            ("x-ms-version".to_owned(), WIRE_VERSION.to_owned()),
        ];
        if method == ci_url::Method::Post {
            headers.push((
                "Content-Type".to_owned(),
                "text/xml; charset=utf-8".to_owned(),
            ));
        }
        ci_url::Config {
            timeout: READ_TIMEOUT,
            // `http_with_retries` runs its own loop, so `readurl` gets one try.
            retries: 0,
            headers,
            method,
            body,
            ..ci_url::Config::default()
        }
    }

    /// `_get_raw_goal_state_xml_from_azure`.
    pub fn fetch_goal_state_raw(
        &self,
        log: &mut ci_log::Logger,
    ) -> Result<Vec<u8>, Error> {
        let url = format!("http://{}/machine/?comp=goalstate", self.endpoint);
        log.info(SOURCE, "Registering with Azure...");
        let response = self.with_retries(&url, ci_url::Method::Get, Vec::new(), log)?;
        log.debug(SOURCE, "Successfully fetched GoalState XML.");
        Ok(response)
    }

    /// `_get_raw_goal_state_xml_from_azure` plus `_parse_raw_goal_state_xml`,
    /// followed by `GoalState.__init__`'s certificates fetch when a transport
    /// certificate is on hand.
    pub fn fetch_goal_state(
        &self,
        log: &mut ci_log::Logger,
    ) -> Result<GoalState, Error> {
        let response = self.fetch_goal_state_raw(log)?;
        let mut goal_state = GoalState::parse(&String::from_utf8_lossy(&response))?;
        let (Some(url), Some(_)) = (&goal_state.certificates_url, &self.certificate)
        else {
            return Ok(goal_state);
        };
        let document = self.fetch_url_secure(&url.clone(), log)?;
        if document.is_empty() {
            return Err(Error::InvalidGoalState(
                "Azure endpoint returned empty certificates xml.".to_owned(),
            ));
        }
        goal_state.certificates_xml =
            Some(String::from_utf8_lossy(&document).into_owned());
        Ok(goal_state)
    }

    /// A bare wireserver GET, for a URL the goal state named.
    ///
    /// Not enough for `comp=certificates`, which upstream fetches with
    /// `secure=True`: without `x-ms-cipher-name` and a transport certificate in
    /// `x-ms-guest-agent-public-x509-cert` the wireserver answers 400.
    pub fn fetch_url(
        &self,
        url: &str,
        log: &mut ci_log::Logger,
    ) -> Result<Vec<u8>, Error> {
        self.with_retries(url, ci_url::Method::Get, Vec::new(), log)
    }

    /// `get(url, secure=True)`.
    pub fn fetch_url_secure(
        &self,
        url: &str,
        log: &mut ci_log::Logger,
    ) -> Result<Vec<u8>, Error> {
        let certificate = self.certificate.clone().ok_or_else(|| {
            Error::Certificate("no transport certificate was generated".to_owned())
        })?;
        let mut config = Self::config(ci_url::Method::Get, Vec::new());
        config
            .headers
            .push(("x-ms-cipher-name".to_owned(), CIPHER_NAME.to_owned()));
        config
            .headers
            .push(("x-ms-guest-agent-public-x509-cert".to_owned(), certificate));
        self.request(url, &config, log)
    }

    /// `_post_health_report`.
    pub fn post_health_report(
        &self,
        document: Vec<u8>,
        log: &mut ci_log::Logger,
    ) -> Result<(), Error> {
        let url = format!("http://{}/machine?comp=health", self.endpoint);
        log.debug(SOURCE, "Sending health report to Azure fabric.");
        self.with_retries(&url, ci_url::Method::Post, document, log)?;
        log.debug(SOURCE, "Successfully sent health report to Azure fabric");
        Ok(())
    }

    /// `http_with_retries`, on a monotonic deadline rather than upstream's
    /// wall clock (bug B48).
    fn with_retries(
        &self,
        url: &str,
        method: ci_url::Method,
        body: Vec<u8>,
        log: &mut ci_log::Logger,
    ) -> Result<Vec<u8>, Error> {
        let config = Self::config(method, body);
        self.request(url, &config, log)
    }

    fn request(
        &self,
        url: &str,
        config: &ci_url::Config,
        log: &mut ci_log::Logger,
    ) -> Result<Vec<u8>, Error> {
        let deadline = std::time::Instant::now() + self.timeout;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let failure = match ci_url::readurl(url, config) {
                Ok(response) => {
                    log.debug(
                        SOURCE,
                        &format!(
                            "Successful HTTP request with Azure endpoint {url} \
                         after {attempt} attempts"
                        ),
                    );
                    return Ok(response.contents);
                }
                Err(failure) => failure,
            };
            log.debug(
                SOURCE,
                &format!(
                    "Failed HTTP request with Azure endpoint {url} during attempt \
                 {attempt} with exception: {failure} (code={:?})",
                    failure.code
                ),
            );
            let out_of_time = std::time::Instant::now() + self.retry_sleep >= deadline;
            if out_of_time || failure.message.contains("Network is unreachable") {
                return Err(Error::Transport(failure.to_string()));
            }
            std::thread::sleep(self.retry_sleep);
        }
    }
}

/// `WALinuxAgentShim.register_with_azure_and_fetch_data`.
///
/// The certificate exchange only happens when the caller asked for keys, and
/// it happens *before* the ready signal: upstream lets an `openssl` failure
/// propagate out of `_get_user_pubkeys`, so a VM whose certificates cannot be
/// read never reports ready at all.
pub fn report_ready(
    endpoint: &str,
    pubkey_info: Option<&[Value]>,
    log: &mut ci_log::Logger,
) -> Result<Vec<String>, Error> {
    let transport = match pubkey_info {
        Some(_) => Some(
            super::certs::Transport::generate(log)
                .map_err(|err| Error::Certificate(err.to_string()))?,
        ),
        None => None,
    };
    let mut client = Client::new(endpoint);
    if let Some(transport) = &transport {
        client = client.with_certificate(transport.certificate());
    }
    let goal_state = client.fetch_goal_state(log)?;

    let ssh_keys = match (pubkey_info, &transport, &goal_state.certificates_xml) {
        (Some(pubkey_info), Some(transport), Some(document)) => {
            log.debug(SOURCE, "Certificate XML found; parsing out public keys.");
            let by_fingerprint = transport
                .parse_certificates(document)
                .map_err(|err| Error::Certificate(err.to_string()))?;
            super::certs::filter_pubkeys(&by_fingerprint, pubkey_info, log)
        }
        _ => Vec::new(),
    };

    log.debug(SOURCE, "Reporting ready to Azure fabric.");
    let document = build_report(&goal_state, SUCCESS_STATUS, None, "");
    client.post_health_report(document, log)?;
    log.info(SOURCE, "Reported ready to Azure fabric.");
    Ok(ssh_keys)
}

/// `report_failure_to_fabric`. The description is the encoded report deviation
/// 79 builds.
pub fn report_failure(
    endpoint: &str,
    encoded_report: &str,
    log: &mut ci_log::Logger,
) -> Result<(), Error> {
    let client = Client::new(endpoint);
    let goal_state = client.fetch_goal_state(log)?;
    let document = build_report(
        &goal_state,
        NOT_READY_STATUS,
        Some(FAILURE_SUBSTATUS),
        encoded_report,
    );
    client.post_health_report(document, log)?;
    log.warning(SOURCE, "Reported failure to Azure fabric.");
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{
        build_minimal_ovf, build_report, endpoint_from_lease, escape, Error, GoalState,
        DEFAULT_ENDPOINT,
    };

    #[test]
    fn a_lease_without_option_245_leaves_the_default_endpoint_standing() {
        let mut lease = ci_config::Object::new();
        lease.insert(
            "fixed-address".to_owned(),
            ci_config::Value::String("10.0.0.4".to_owned()),
        );
        assert_eq!(endpoint_from_lease(&lease), None);
        lease.insert(
            "unknown-245".to_owned(),
            ci_config::Value::String("10.1.2.3".to_owned()),
        );
        assert_eq!(endpoint_from_lease(&lease), Some("10.1.2.3"));
        assert_eq!(DEFAULT_ENDPOINT, "168.63.129.16");
    }

    const FULL: &str = "<GoalState><Incarnation>1</Incarnation><Container>\
        <ContainerId>cid</ContainerId><RoleInstanceList><RoleInstance>\
        <InstanceId>iid</InstanceId><Configuration>\
        <Certificates>http://host/certs</Certificates></Configuration>\
        </RoleInstance></RoleInstanceList></Container></GoalState>";

    fn goal_state() -> GoalState {
        GoalState::parse(FULL).unwrap()
    }

    #[test]
    fn a_full_goal_state_yields_every_field() {
        let parsed = goal_state();
        assert_eq!(parsed.incarnation, "1");
        assert_eq!(parsed.container_id, "cid");
        assert_eq!(parsed.instance_id, "iid");
        assert_eq!(
            parsed.certificates_url.as_deref(),
            Some("http://host/certs")
        );
    }

    #[test]
    fn a_missing_field_names_the_attribute_upstream_names() {
        let blob = FULL.replace("<Incarnation>1</Incarnation>", "");
        assert_eq!(
            GoalState::parse(&blob),
            Err(Error::InvalidGoalState(
                "Missing incarnation in GoalState XML".to_owned()
            ))
        );
    }

    #[test]
    fn an_empty_element_counts_as_missing() {
        let blob = FULL.replace("<ContainerId>cid</ContainerId>", "<ContainerId/>");
        assert_eq!(
            GoalState::parse(&blob),
            Err(Error::InvalidGoalState(
                "Missing container_id in GoalState XML".to_owned()
            ))
        );
    }

    #[test]
    fn a_goal_state_without_certificates_still_parses() {
        let blob = FULL.replace(
            "<Configuration><Certificates>http://host/certs</Certificates>\
             </Configuration>",
            "",
        );
        assert_eq!(GoalState::parse(&blob).unwrap().certificates_url, None);
    }

    #[test]
    fn only_the_three_markup_characters_are_escaped() {
        assert_eq!(escape("a <b> & 'c' \"d\""), "a &lt;b&gt; &amp; 'c' \"d\"");
    }

    #[test]
    fn a_ready_report_leaves_the_details_line_blank() {
        let document =
            String::from_utf8(build_report(&goal_state(), "Ready", None, "")).unwrap();
        assert!(document.contains("<State>Ready</State>\n          \n"));
        assert!(!document.contains("<Details>"));
    }

    #[test]
    fn a_failure_report_carries_the_substatus_and_description() {
        let document = String::from_utf8(build_report(
            &goal_state(),
            "NotReady",
            Some("ProvisioningFailed"),
            "a <bad> & thing",
        ))
        .unwrap();
        assert!(document.contains(
            "<Details>\n  <SubStatus>ProvisioningFailed</SubStatus>\n  \
             <Description>a &lt;bad&gt; &amp; thing</Description>\n</Details>\n\n"
        ));
    }

    #[test]
    fn a_long_description_is_trimmed_before_it_is_escaped() {
        let document = String::from_utf8(build_report(
            &goal_state(),
            "NotReady",
            Some("ProvisioningFailed"),
            &"<".repeat(600),
        ))
        .unwrap();
        assert_eq!(document.matches("&lt;").count(), 512);
    }

    #[test]
    fn a_minimal_ovf_drops_the_lines_it_has_no_value_for() {
        let document =
            String::from_utf8(build_minimal_ovf(None, "host", None)).unwrap();
        assert!(
            document.contains("</ns1:ConfigurationSetType>\n\n\n      <ns1:HostName>")
        );

        let document =
            String::from_utf8(build_minimal_ovf(Some("user"), "host", Some(true)))
                .unwrap();
        assert!(document.contains("      <ns1:UserName>user</ns1:UserName>\n"));
        assert!(document.contains(
            "      <ns1:DisableSshPasswordAuthentication>true\
             </ns1:DisableSshPasswordAuthentication>\n"
        ));
    }
}
