//! TLS for `https:` URLs, and `util.fetch_ssl_details`.
//!
//! Upstream is `requests`, which is `urllib3`, which is `CPython`'s `ssl`,
//! which is OpenSSL. This is the same OpenSSL, reached through the `openssl`
//! crate's safe bindings instead: the trust decisions a booting machine makes
//! about a metadata service are not somewhere to be inventive, and no pure-Rust
//! stack could be built at the declared MSRV without also replacing the system
//! trust store. See docs/COMPAT.md.

use std::net::TcpStream;
use std::path::{Path, PathBuf};

use openssl::ssl::{SslConnector, SslFiletype, SslMethod, SslStream, SslVersion};
use openssl::x509::store::{X509Lookup, X509StoreBuilder};
use openssl::x509::verify::X509VerifyFlags;

use ci_core::{Lookup, Paths};

/// `ssl_details`: the dict `_get_ssl_args` turns into `requests` arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SslDetails {
    /// `verify`. Unset means the system trust store, as `verify=True` does.
    pub ca_certs: Option<PathBuf>,
    /// `cert`, first element: the client certificate chain.
    pub cert_file: Option<PathBuf>,
    /// `cert`, second element. Ignored unless `cert_file` is also set, which
    /// is upstream's shape rather than a choice made here.
    pub key_file: Option<PathBuf>,
}

/// `util.fetch_ssl_details`. Looks for a client certificate under the instance
/// data directory and then the shared one.
///
/// A `key.pem` without a `cert.pem` is discarded, because upstream's final
/// `elif` only carries the certificate forward.
#[must_use]
pub fn fetch_ssl_details(paths: &Paths) -> SslDetails {
    let dirs = [
        paths.instance_path(Lookup::Data).join("ssl"),
        paths.cpath(Lookup::Data).join("ssl"),
    ];
    let first = |name: &str| {
        dirs.iter()
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
    };
    let Some(cert_file) = first("cert.pem") else {
        return SslDetails::default();
    };
    SslDetails {
        ca_certs: None,
        cert_file: Some(cert_file),
        key_file: first("key.pem"),
    }
}

/// Wrap a connected socket in TLS, verifying the peer against `details`.
///
/// `host` is the name from the URL: it is the SNI value, and it is what the
/// certificate has to match. An IP literal is matched against the certificate's
/// IP entries and sent without SNI, which is what `requests` does too.
pub(crate) fn connect(
    stream: TcpStream,
    host: &str,
    details: &SslDetails,
) -> Result<SslStream<TcpStream>, String> {
    let connector = build(details).map_err(|e| format!("TLS setup failed: {e}"))?;
    connector
        .connect(host, stream)
        .map_err(|e| format!("TLS handshake failed: {e}"))
}

fn build(details: &SslDetails) -> Result<SslConnector, openssl::error::ErrorStack> {
    // The crate's connector defaults are modelled on CPython's: peer
    // verification on, hostname checked, compression off, the anonymous and
    // broken cipher suites removed, and the system trust store loaded.
    let mut builder = SslConnector::builder(SslMethod::tls_client())?;
    // Ubuntu's OpenSSL config already refuses anything older; saying so here
    // means a host with a laxer /etc/ssl/openssl.cnf does not quietly relax it.
    builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    // What `ssl.create_default_context` turns on and the crate's connector does
    // not: `X509_STRICT` holds certificates to the letter of RFC 5280, and
    // `PARTIAL_CHAIN` lets an intermediate in the trust store be an anchor.
    builder
        .verify_param_mut()
        .set_flags(X509VerifyFlags::X509_STRICT | X509VerifyFlags::PARTIAL_CHAIN)?;
    if let Some(ca_certs) = &details.ca_certs {
        // `verify=<path>` replaces the trust set rather than adding to it, so
        // the default store is dropped instead of being extended.
        builder.set_cert_store(trust_store(ca_certs)?);
    } else if let Some(bundle) = distribution_bundle() {
        builder.set_ca_file(bundle)?;
    }
    if let Some(cert_file) = &details.cert_file {
        builder.set_certificate_chain_file(cert_file)?;
        // `requests` reads the key out of the certificate file when it was
        // given one path instead of a pair.
        let key_file = details.key_file.as_deref().unwrap_or(cert_file);
        builder.set_private_key_file(key_file, SslFiletype::PEM)?;
        builder.check_private_key()?;
    }
    Ok(builder.build())
}

/// Where Debian and Ubuntu patch `requests` to look:
/// `requests.utils.DEFAULT_CA_BUNDLE_PATH`. Naming it rather than leaning on
/// the trust store the connector loads by default matters for the static
/// tarballs, which carry their own OpenSSL and so their own `OPENSSLDIR`, and
/// that is not the distribution's.
///
/// `None` when OpenSSL's own environment overrides are set — they are how the
/// differential harness points both implementations at a throwaway CA, and
/// they must keep winning — or when the file simply is not there, which leaves
/// the connector's default store in place.
fn distribution_bundle() -> Option<&'static Path> {
    if std::env::var_os("SSL_CERT_FILE").is_some()
        || std::env::var_os("SSL_CERT_DIR").is_some()
    {
        return None;
    }
    let path = Path::new("/etc/ssl/certs/ca-certificates.crt");
    path.is_file().then_some(path)
}

/// A store holding only `ca_certs`, which `requests` accepts as either a
/// bundle file or an `openssl rehash`-ed directory.
fn trust_store(
    ca_certs: &Path,
) -> Result<openssl::x509::store::X509Store, openssl::error::ErrorStack> {
    let mut store = X509StoreBuilder::new()?;
    if ca_certs.is_dir() {
        store
            .add_lookup(X509Lookup::hash_dir())?
            .add_dir(&ca_certs.to_string_lossy(), SslFiletype::PEM)?;
    } else {
        store
            .add_lookup(X509Lookup::file())?
            .load_cert_file(ca_certs, SslFiletype::PEM)?;
    }
    Ok(store.build())
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

    fn paths_under(root: &Path) -> Paths {
        Paths {
            cloud_dir: root.to_path_buf(),
            ..Paths::default()
        }
    }

    #[test]
    fn no_ssl_directory_means_the_system_trust_store_and_no_client_cert() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            fetch_ssl_details(&paths_under(root.path())),
            SslDetails::default()
        );
    }

    #[test]
    fn a_key_without_a_certificate_is_dropped_as_upstream_drops_it() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data/ssl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("key.pem"), b"").unwrap();
        assert_eq!(
            fetch_ssl_details(&paths_under(root.path())),
            SslDetails::default()
        );
    }

    #[test]
    fn the_instance_directory_wins_over_the_shared_one() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("data/ssl");
        let instance = root.path().join("instance/data/ssl");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&instance).unwrap();
        std::fs::write(shared.join("cert.pem"), b"").unwrap();
        std::fs::write(shared.join("key.pem"), b"").unwrap();
        std::fs::write(instance.join("cert.pem"), b"").unwrap();

        let details = fetch_ssl_details(&paths_under(root.path()));
        // The two searches are independent, so a certificate from the instance
        // directory pairs with a key from the shared one.
        assert_eq!(details.cert_file, Some(instance.join("cert.pem")));
        assert_eq!(details.key_file, Some(shared.join("key.pem")));
    }

    #[test]
    fn a_connector_without_details_is_the_system_one() {
        assert!(build(&SslDetails::default()).is_ok());
    }

    #[test]
    fn an_unreadable_ca_bundle_is_an_error_not_a_silent_downgrade() {
        let details = SslDetails {
            ca_certs: Some(PathBuf::from("/nonexistent/ca.pem")),
            ..SslDetails::default()
        };
        assert!(build(&details).is_err());
    }
}
