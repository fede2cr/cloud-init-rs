//! `OpenSSLManager`: the transport certificate the wireserver encrypts to, and
//! the PKCS#7 document it answers with.
//!
//! Upstream shells out to `openssl` and `ssh-keygen` rather than linking a
//! crypto library, and so does this, for the same reason: the fabric's
//! `Pkcs7BlobWithPfxContents` is whatever the platform's OpenSSL will accept,
//! and a second implementation would only disagree with it.

use std::collections::BTreeMap;
use std::path::Path;

use ci_config::Value;
use ci_sys::subp;

/// `%(filename)s` for everything in this module.
const SOURCE: &str = "azure.py";

/// `OpenSSLManager.certificate_names`.
const PRIVATE_KEY: &str = "TransportPrivate.pem";
const CERTIFICATE: &str = "TransportCert.pem";

/// The certificates document is one long base64 blob and upstream reads it
/// with no bound at all; a megabyte is not enough for a VM with many
/// extensions.
const XML_LIMITS: ci_config::xml::Limits = ci_config::xml::Limits {
    max_bytes: 16 << 20,
    max_depth: 64,
    max_nodes: 8192,
};

/// A `ProcessExecutionError` or an unreadable document, both of which abort
/// the whole exchange upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// The self-signed key pair generated once per boot, in a 0700 directory that
/// goes away when this value is dropped.
///
/// Upstream's `clean_up` is called from `get_metadata_from_fabric`'s `finally`
/// and is easy to skip; `Drop` cannot be.
#[derive(Debug)]
pub struct Transport {
    dir: ci_sys::path::TempDir,
    certificate: String,
}

impl Transport {
    /// `OpenSSLManager.__init__` and `generate_certificate`.
    pub fn generate(log: &mut ci_log::Logger) -> Result<Self, Error> {
        log.debug(
            SOURCE,
            "Generating certificate for communication with fabric...",
        );
        let dir = ci_sys::path::TempDir::new(std::env::temp_dir(), "cloud-init-azure-")
            .map_err(|err| {
                Error(format!("could not create a temporary directory: {err}"))
            })?;
        let key_path = dir.path().join(PRIVATE_KEY);
        let cert_path = dir.path().join(CERTIFICATE);
        run(
            &[
                "openssl",
                "req",
                "-x509",
                "-nodes",
                "-subj",
                "/CN=LinuxTransport",
                "-days",
                "32768",
                "-newkey",
                "rsa:3072",
                "-keyout",
                &lossy(&key_path),
                "-out",
                &lossy(&cert_path),
            ],
            Vec::new(),
        )?;

        // The header carries the base64 body only, with every line joined.
        let pem = std::fs::read_to_string(&cert_path).map_err(|err| {
            Error(format!("could not read {}: {err}", cert_path.display()))
        })?;
        let certificate = pem
            .lines()
            .filter(|line| !line.contains("CERTIFICATE"))
            .map(str::trim_end)
            .collect::<String>();
        log.debug(SOURCE, "New certificate generated.");
        Ok(Self { dir, certificate })
    }

    /// The value of `x-ms-guest-agent-public-x509-cert`.
    #[must_use]
    pub fn certificate(&self) -> &str {
        &self.certificate
    }

    /// `parse_certificates`: fingerprint to SSH public key.
    pub fn parse_certificates(
        &self,
        certificates_xml: &str,
    ) -> Result<BTreeMap<String, String>, Error> {
        let out = self.decrypt_certs_from_xml(certificates_xml)?;
        let mut keys = BTreeMap::new();
        let mut current: Vec<&str> = Vec::new();
        for line in out.lines() {
            current.push(line);
            if ends_pem_block(line, "KEY") {
                current.clear();
            } else if ends_pem_block(line, "CERTIFICATE") {
                // The bag attributes `openssl pkcs12` prints ahead of each
                // block stay in: `openssl x509` skips anything before the
                // header, so upstream never strips them.
                let certificate = current.join("\n");
                keys.insert(
                    fingerprint_from_cert(&certificate)?,
                    ssh_key_from_cert(&certificate)?,
                );
                current.clear();
            }
        }
        Ok(keys)
    }

    /// `_decrypt_certs_from_xml`.
    ///
    /// Upstream runs `openssl cms | openssl pkcs12` through a shell; the port
    /// has none, so the two run separately and the intermediate PKCS#12 is
    /// carried in memory. That also stops a `cms` failure being hidden by the
    /// pipeline's exit status.
    fn decrypt_certs_from_xml(&self, certificates_xml: &str) -> Result<String, Error> {
        let root = ci_config::xml::parse(certificates_xml, XML_LIMITS)
            .map_err(|err| Error(format!("Failed to parse Certificates XML: {err}")))?;
        let data = root
            .find_descendant("Data")
            .and_then(|node| node.text.clone())
            .ok_or_else(|| Error("Certificates XML has no Data element".to_owned()))?;

        let mut blob = Vec::new();
        for line in [
            "MIME-Version: 1.0",
            "Content-Disposition: attachment; filename=\"Certificates.p7m\"",
            "Content-Type: application/x-pkcs7-mime; name=\"Certificates.p7m\"",
            "Content-Transfer-Encoding: base64",
            "",
            data.as_str(),
        ] {
            if !blob.is_empty() {
                blob.push(b'\n');
            }
            blob.extend_from_slice(line.as_bytes());
        }

        let pkcs12 = run(
            &[
                "openssl",
                "cms",
                "-decrypt",
                "-in",
                "/dev/stdin",
                "-inkey",
                &lossy(&self.dir.path().join(PRIVATE_KEY)),
                "-recip",
                &lossy(&self.dir.path().join(CERTIFICATE)),
            ],
            blob,
        )?;
        let pem = run(
            &["openssl", "pkcs12", "-nodes", "-password", "pass:"],
            pkcs12.stdout,
        )?;
        Ok(String::from_utf8_lossy(&pem.stdout).into_owned())
    }
}

/// `re.match(r"[-]+END .*?<kind>[-]+$", line)`.
fn ends_pem_block(line: &str, kind: &str) -> bool {
    let rest = line.trim_start_matches('-');
    if rest.len() == line.len() {
        return false;
    }
    let Some(rest) = rest.strip_prefix("END ") else {
        return false;
    };
    let body = rest.trim_end_matches('-');
    body.len() != rest.len() && body.ends_with(kind)
}

/// `_get_ssh_key_from_cert`. The trailing newline `ssh-keygen` prints is
/// upstream's too — `subp` does not strip it and the key is stored as it came.
fn ssh_key_from_cert(certificate: &str) -> Result<String, Error> {
    let pubkey = run_x509("-pubkey", certificate)?;
    let key = run(
        &["ssh-keygen", "-i", "-m", "PKCS8", "-f", "/dev/stdin"],
        pubkey.into_bytes(),
    )?;
    Ok(String::from_utf8_lossy(&key.stdout).into_owned())
}

/// `_get_fingerprint_from_cert`: the colons come out, and the trailing newline
/// goes with the slice, which is why upstream indexes to `-1`.
fn fingerprint_from_cert(certificate: &str) -> Result<String, Error> {
    let raw = run_x509("-fingerprint", certificate)?;
    let after = raw
        .find('=')
        .and_then(|eq| raw.get(eq + 1..))
        .unwrap_or_default();
    let mut chars = after.chars();
    chars.next_back();
    Ok(chars.as_str().replace(':', ""))
}

/// `_run_x509_action`.
fn run_x509(action: &str, certificate: &str) -> Result<String, Error> {
    let out = run(
        &["openssl", "x509", "-noout", action],
        certificate.as_bytes().to_vec(),
    )?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run(argv: &[&str], stdin: Vec<u8>) -> Result<subp::Output, Error> {
    subp::Subp::new(argv)
        .stdin(stdin)
        .check()
        .map_err(|err| Error(err.to_string()))
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `WALinuxAgentShim._filter_pubkeys`.
///
/// A `value` wins outright, a `fingerprint` is looked up, and anything else is
/// warned about and dropped.
#[must_use]
pub fn filter_pubkeys(
    keys_by_fingerprint: &BTreeMap<String, String>,
    pubkey_info: &[Value],
    log: &mut ci_log::Logger,
) -> Vec<String> {
    let mut keys = Vec::new();
    for pubkey in pubkey_info {
        let field = |name: &str| {
            pubkey
                .as_object()
                .and_then(|map| map.get(name))
                .filter(|value| ci_config::option::py_truthy(value))
        };
        if let Some(value) = field("value") {
            keys.push(value.as_str().unwrap_or_default().to_owned());
        } else if let Some(fingerprint) = field("fingerprint") {
            let fingerprint = fingerprint.as_str().unwrap_or_default();
            if let Some(key) = keys_by_fingerprint.get(fingerprint) {
                keys.push(key.clone());
            } else {
                log.warning(
                    SOURCE,
                    &format!(
                        "ovf-env.xml specified PublicKey fingerprint \
                         {fingerprint} not found in goalstate XML"
                    ),
                );
            }
        } else {
            log.warning(
                SOURCE,
                &format!(
                    "ovf-env.xml specified PublicKey with neither value nor \
                     fingerprint: {}",
                    ci_config::repr::repr(pubkey)
                ),
            );
        }
    }
    keys
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn logger() -> ci_log::Logger {
        ci_log::Logger::silent()
    }

    /// A logger whose lines can be read back, since `ci_log` has no capture.
    fn logger_to(path: &Path) -> ci_log::Logger {
        let handler = format!(
            "[loggers]\nkeys=root\n[handlers]\nkeys=h\n[formatters]\nkeys=f\n\
             [logger_root]\nlevel=DEBUG\nhandlers=h\n\
             [formatter_f]\nformat=%(filename)s[%(levelname)s]: %(message)s\n\
             [handler_h]\nclass=FileHandler\nlevel=DEBUG\nformatter=f\n\
             args=('{}', 'a')\n",
            path.display()
        );
        let mut cfg = ci_config::Object::new();
        cfg.insert(
            "log_cfgs".to_owned(),
            Value::Array(vec![Value::from(handler)]),
        );
        ci_log::Logger::from_config(&cfg)
    }

    fn obj(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    /// OpenSSL's base64 reader wants armour-width lines, and that is how the
    /// wireserver sends the blob.
    fn wrap64(text: &str) -> String {
        text.as_bytes()
            .chunks(64)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn only_a_pem_terminator_of_the_right_kind_ends_a_block() {
        assert!(ends_pem_block("-----END PRIVATE KEY-----", "KEY"));
        assert!(ends_pem_block("-----END RSA PRIVATE KEY-----", "KEY"));
        assert!(ends_pem_block("-----END CERTIFICATE-----", "CERTIFICATE"));
        // `.*?KEY` cannot reach across the terminator's own dashes.
        assert!(!ends_pem_block("-----END CERTIFICATE-----", "KEY"));
        assert!(!ends_pem_block("-----BEGIN PRIVATE KEY-----", "KEY"));
        assert!(!ends_pem_block("END PRIVATE KEY-----", "KEY"));
        assert!(!ends_pem_block("-----END PRIVATE KEY", "KEY"));
        assert!(!ends_pem_block("subject=CN = KEY", "KEY"));
    }

    #[test]
    fn a_value_wins_over_a_fingerprint_and_neither_is_only_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.log");
        let mut log = logger_to(&path);
        let mut by_fingerprint = BTreeMap::new();
        by_fingerprint.insert("AA".to_owned(), "ssh-rsa from-wireserver\n".to_owned());
        let info = [
            obj(r#"{"fingerprint": "AA", "path": "p", "value": "ssh-rsa inline"}"#),
            obj(r#"{"fingerprint": "AA", "path": "p", "value": ""}"#),
            obj(r#"{"fingerprint": "ZZ", "path": "p", "value": ""}"#),
            obj(r#"{"fingerprint": null, "path": "p", "value": ""}"#),
        ];
        assert_eq!(
            filter_pubkeys(&by_fingerprint, &info, &mut log),
            ["ssh-rsa inline", "ssh-rsa from-wireserver\n"]
        );
        log.flush();
        let logged = std::fs::read_to_string(&path).unwrap();
        assert!(
            logged.contains(
                "ovf-env.xml specified PublicKey fingerprint ZZ not found in \
                 goalstate XML"
            ),
            "{logged}"
        );
        assert!(
            logged.contains(
                "ovf-env.xml specified PublicKey with neither value nor \
                 fingerprint: {'fingerprint': None, 'path': 'p', 'value': ''}"
            ),
            "{logged}"
        );
    }

    /// Golden values taken from upstream's own `OpenSSLManager` methods run
    /// against this certificate, so the two transforms the port reimplements
    /// -- the fingerprint munging and the `ssh-keygen` conversion -- are
    /// pinned to bytes cloud-init produced rather than to bytes this port
    /// produced.
    #[test]
    fn the_two_certificate_transforms_match_upstream_byte_for_byte() {
        const CERT: &str = "\
             -----BEGIN CERTIFICATE-----\n\
             MIIDDTCCAfWgAwIBAgIUdXhL5vS5xcB377lZ7U2XOEfANDQwDQYJKoZIhvcNAQEL\n\
             BQAwFTETMBEGA1UEAwwKYXp1cmUtdGVzdDAgFw0yNjA5MDQyMzM3NDVaGA8yMTE2\n\
             MDUyMzIzMzc0NVowFTETMBEGA1UEAwwKYXp1cmUtdGVzdDCCASIwDQYJKoZIhvcN\n\
             AQEBBQADggEPADCCAQoCggEBAJix044fRmKizJ9vM/FQqYtETPr4M5HitigB5Y/y\n\
             PlZlTkFzv1K3+0F5MzKTNUlcMCfEOPOILryY0o97fQj/IZIPrI4k5DAayV6VaWB3\n\
             zsSIpXxDiagA9e/+OBrWmq0FuiHk0bT40wLwM6Auux3w+4ab+ab6ZXOubYKCjTsq\n\
             Jk5NUFvE7DCc7vQineljwvZ+dYO/k66AFM+8y678IeCRCt3cWtpzstcM+x++gPvr\n\
             9SC+3GzAHoaBuHA92KkSiA57iEn26cagm5iC6XN1Si4vUFStadGVroUSMc23AYEc\n\
             BlVk/BTIVht2UFcFceFxGhHrNlYPN/yg6EIzMHvm3E6WPZcCAwEAAaNTMFEwHQYD\n\
             VR0OBBYEFDISGjrGvKcceHg5uBi28dN33Jy2MB8GA1UdIwQYMBaAFDISGjrGvKcc\n\
             eHg5uBi28dN33Jy2MA8GA1UdEwEB/wQFMAMBAf8wDQYJKoZIhvcNAQELBQADggEB\n\
             AFHe595PTj8IVeuc1sVPoB+JmfcUp7l+iCkK1828ZykNBheYluSq9CM6CpF7fvih\n\
             uKWTTfpSNxgrFnxn0awPW4E5sq1tUo0ojbb8OQi8xUIftSoKwKtM6WXbRnJKHwFa\n\
             tXnS/u8n6R8QRvUojYs9enfMkBRck0UnkWi68sOOA9x5rcsM/qxmZ5dzZE+ilqDO\n\
             SzRmgPatE1u7E+/iJhtXPBdkmarsvZck/JUV2gpCY21srwdS3kk37OoXOFSRMXxZ\n\
             le/MxO6bb2gqtdG5vrpBA8BTA1VafP6voLwr0j/9kZ7Q5h/7pDQ7eCXcvKESD1sd\n\
             zqzHIFcQLe7f/fr4W6BeA6M=\n\
             -----END CERTIFICATE-----\n";
        const KEY: &str = concat!(
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCYsdOOH0ZiosyfbzPxUKmL",
            "REz6+DOR4rYoAeWP8j5WZU5Bc79St/tBeTMykzVJXDAnxDjziC68mNKPe30I",
            "/yGSD6yOJOQwGslelWlgd87EiKV8Q4moAPXv/jga1pqtBboh5NG0+NMC8DOg",
            "Lrsd8PuGm/mm+mVzrm2Cgo07KiZOTVBbxOwwnO70Ip3pY8L2fnWDv5OugBTP",
            "vMuu/CHgkQrd3Frac7LXDPsfvoD76/UgvtxswB6GgbhwPdipEogOe4hJ9unG",
            "oJuYgulzdUouL1BUrWnRla6FEjHNtwGBHAZVZPwUyFYbdlBXBXHhcRoR6zZW",
            "Dzf8oOhCMzB75txOlj2X",
            "\n",
        );

        for tool in ["openssl", "ssh-keygen"] {
            if subp::which(tool).is_none() {
                return;
            }
        }

        assert_eq!(
            fingerprint_from_cert(CERT).unwrap(),
            "EB501746444B049FF2805A018DE906512C46C108"
        );
        assert_eq!(ssh_key_from_cert(CERT).unwrap(), KEY);
    }

    /// The whole exchange, with this process playing the fabric: encrypt a
    /// PKCS#12 bundle to the certificate the port just generated and hand it
    /// back in the document the wireserver would have returned.
    #[test]
    fn a_document_encrypted_to_the_transport_certificate_yields_the_ssh_key() {
        for tool in ["openssl", "ssh-keygen"] {
            if subp::which(tool).is_none() {
                return;
            }
        }
        let mut log = logger();
        let transport = Transport::generate(&mut log).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| lossy(&dir.path().join(name));

        // The header value is the base64 body with the armour removed, so a
        // PEM has to be built back up before OpenSSL will take it.
        let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
        for chunk in transport.certificate().as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(chunk).unwrap());
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
        std::fs::write(dir.path().join("recip.pem"), &pem).unwrap();

        run(
            &[
                "openssl",
                "req",
                "-x509",
                "-nodes",
                "-subj",
                "/CN=user",
                "-newkey",
                "rsa:2048",
                "-keyout",
                &path("user.key"),
                "-out",
                &path("user.crt"),
            ],
            Vec::new(),
        )
        .unwrap();
        run(
            &[
                "openssl",
                "pkcs12",
                "-export",
                "-inkey",
                &path("user.key"),
                "-in",
                &path("user.crt"),
                "-out",
                &path("bundle.p12"),
                "-passout",
                "pass:",
            ],
            Vec::new(),
        )
        .unwrap();
        let encrypted = run(
            &[
                // `-binary`, or OpenSSL canonicalises the PKCS#12 as text and
                // truncates it. The fabric's own blob is binary-clean.
                "openssl",
                "cms",
                "-encrypt",
                "-binary",
                "-des3",
                "-outform",
                "DER",
                "-in",
                &path("bundle.p12"),
                &path("recip.pem"),
            ],
            Vec::new(),
        )
        .unwrap();
        let document = format!(
            "<CertificateFile><Version>2012-11-30</Version>\
             <Incarnation>1</Incarnation>\
             <Format>Pkcs7BlobWithPfxContents</Format>\
             <Data>{}</Data></CertificateFile>",
            wrap64(&ci_core::b64::encode(&encrypted.stdout))
        );

        let keys = transport.parse_certificates(&document).unwrap();
        let expected_fingerprint = fingerprint_from_cert(
            &std::fs::read_to_string(dir.path().join("user.crt")).unwrap(),
        )
        .unwrap();
        assert_eq!(keys.len(), 1, "{keys:?}");
        let key = &keys[&expected_fingerprint];
        assert!(key.starts_with("ssh-rsa "), "{key}");
        // `subp` does not strip, so upstream stores the newline too.
        assert!(key.ends_with('\n'), "{key:?}");
    }

    #[test]
    fn a_document_without_a_data_element_is_refused_before_openssl_runs() {
        if subp::which("openssl").is_none() {
            return;
        }
        let mut log = logger();
        let transport = Transport::generate(&mut log).unwrap();
        assert_eq!(
            transport
                .parse_certificates(
                    "<CertificateFile><Format>x</Format></CertificateFile>"
                )
                .unwrap_err(),
            Error("Certificates XML has no Data element".to_owned())
        );
    }
}
