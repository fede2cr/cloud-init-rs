//! Port of `sources/helpers/ec2.py`.
//!
//! The EC2 metadata service is a tree walked over HTTP: a directory answers
//! with one child name per line, a name ending in `/` is another directory,
//! and anything else is a leaf whose body is the value. [`Materializer`] is
//! that walk, and [`decode_leaf`] is upstream's `MetadataLeafDecoder`.

use ci_config::{Object, Value};

/// `SKIP_USERDATA_CODES`.
const NOT_FOUND: u16 = 404;

/// Deepest nesting followed before the walk gives up.
///
/// Upstream recurses without a bound. The tree is described by the endpoint
/// being crawled, so a service that answers every directory with one more
/// directory recurses until the interpreter's stack runs out; see
/// docs/COMPAT.md.
const MAX_DEPTH: usize = 16;

/// Most entries followed at one level, for the same reason as [`MAX_DEPTH`].
const MAX_ENTRIES: usize = 1024;

/// What the materializer fetches through.
///
/// Upstream binds `read_file_or_url` to the datasource's `headers_cb`, which
/// re-reads the `IMDSv2` token on every request and can refresh it mid-crawl.
/// That is why this takes `&mut self`.
pub trait Caller {
    /// The URL's body, or the error that stopped it.
    fn fetch(&mut self, url: &str) -> Result<Vec<u8>, ci_url::Error>;
}

/// `MetadataLeafDecoder.__call__`.
#[must_use]
pub fn decode_leaf(field: &str, blob: &[u8]) -> Value {
    if blob.is_empty() {
        return Value::String(String::new());
    }
    let Ok(text) = std::str::from_utf8(blob) else {
        // Upstream returns the raw `bytes`, which `json_dumps` then renders
        // through `json_serialize_default`.
        return Value::String(format!("ci-b64:{}", ci_core::b64::encode(blob)));
    };
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        // A leaf that looks like an object is one; a parse failure is only
        // warned about upstream, and falls through to the string below.
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            return value;
        }
        let _ = field;
    }
    if text.contains('\n') {
        return Value::Array(
            text.lines()
                .map(|line| Value::String(line.to_owned()))
                .collect(),
        );
    }
    Value::String(text.to_owned())
}

/// One line of a directory listing, as `MetadataMaterializer._parse` sorts it.
enum Entry {
    Child(String),
    /// `(field name, resource path)` — the two differ only for the numbered
    /// `public-keys` entries.
    Leaf(String, String),
}

fn parse_listing(blob: &[u8]) -> Vec<Entry> {
    let text = String::from_utf8_lossy(blob);
    let mut entries = Vec::new();
    for field in text.lines().take(MAX_ENTRIES) {
        let field = field.trim();
        let name = field.strip_suffix('/').unwrap_or(field);
        if field.is_empty() || name.is_empty() {
            continue;
        }
        if name == "security-credentials" {
            continue;
        }
        if field.ends_with('/') {
            if !entries
                .iter()
                .any(|entry| matches!(entry, Entry::Child(child) if child == name))
            {
                entries.push(Entry::Child(name.to_owned()));
            }
            continue;
        }
        // `public-keys` lists `0=my-key`, whose value lives at `0/openssh-key`.
        match field.split_once('=') {
            Some((ident, rest)) if ident.parse::<i64>().is_ok() => {
                entries
                    .push(Entry::Leaf(rest.to_owned(), format!("{ident}/openssh-key")));
            }
            _ => entries.push(Entry::Leaf(name.to_owned(), name.to_owned())),
        }
    }
    entries
}

struct Materializer<'a, C: Caller + ?Sized> {
    caller: &'a mut C,
    log: &'a mut ci_log::Logger,
}

impl<C: Caller + ?Sized> Materializer<'_, C> {
    fn walk(&mut self, blob: &[u8], base_url: &str, depth: usize) -> Object {
        let mut joined = Object::new();
        if depth >= MAX_DEPTH {
            self.log.warning(
                "ec2.py",
                &format!("Stopped crawling below {base_url}: nesting too deep"),
            );
            return joined;
        }

        let entries = parse_listing(blob);
        let mut leaves = Vec::new();
        for entry in entries {
            match entry {
                Entry::Child(name) => {
                    let mut child_url = ci_url::url::combine_url(base_url, &[&name]);
                    if !child_url.ends_with('/') {
                        child_url.push('/');
                    }
                    let Ok(child_blob) = self.caller.fetch(&child_url) else {
                        continue;
                    };
                    let child = self.walk(&child_blob, &child_url, depth + 1);
                    joined.insert(name, Value::Object(child));
                }
                Entry::Leaf(field, resource) => leaves.push((field, resource)),
            }
        }

        for (field, resource) in leaves {
            let leaf_url = ci_url::url::combine_url(base_url, &[&resource]);
            let Ok(leaf_blob) = self.caller.fetch(&leaf_url) else {
                continue;
            };
            if joined.contains_key(&field) {
                self.log.warning(
                    "ec2.py",
                    &format!("Duplicate key found in results from {base_url}"),
                );
                continue;
            }
            joined.insert(field.clone(), decode_leaf(&field, &leaf_blob));
        }
        joined
    }
}

/// `_get_instance_metadata`: crawl one tree and return it, or an empty map.
fn crawl(
    tree: &str,
    api_version: &str,
    address: &str,
    caller: &mut (impl Caller + ?Sized),
    log: &mut ci_log::Logger,
) -> Object {
    let md_url = ci_url::url::combine_url(address, &[api_version, tree]);
    match caller.fetch(&md_url) {
        Ok(blob) => Materializer { caller, log }.walk(&blob, &md_url, 0),
        Err(e) => {
            log.warning(
                "ec2.py",
                &format!("Failed fetching {tree} from url {md_url}: {e}"),
            );
            Object::new()
        }
    }
}

/// `get_instance_metadata`. The trailing `/` on the tree is load-bearing.
pub fn instance_metadata(
    api_version: &str,
    address: &str,
    caller: &mut (impl Caller + ?Sized),
    log: &mut ci_log::Logger,
) -> Object {
    crawl("meta-data/", api_version, address, caller, log)
}

/// `get_instance_identity`.
pub fn instance_identity(
    api_version: &str,
    address: &str,
    caller: &mut (impl Caller + ?Sized),
    log: &mut ci_log::Logger,
) -> Object {
    crawl(
        "dynamic/instance-identity",
        api_version,
        address,
        caller,
        log,
    )
}

/// `get_instance_userdata`. A 404 means there is none, which is not an error.
pub fn instance_userdata(
    api_version: &str,
    address: &str,
    caller: &mut (impl Caller + ?Sized),
    log: &mut ci_log::Logger,
) -> Vec<u8> {
    let ud_url = ci_url::url::combine_url(address, &[api_version, "user-data"]);
    match caller.fetch(&ud_url) {
        Ok(blob) => blob,
        Err(e) if e.code == Some(NOT_FOUND) => Vec::new(),
        Err(e) => {
            log.warning(
                "ec2.py",
                &format!("Failed fetching userdata from url {ud_url}: {e}"),
            );
            Vec::new()
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

    struct Tree(std::collections::HashMap<String, &'static str>);

    impl Tree {
        fn new(pairs: &[(&str, &'static str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(path, body)| ((*path).to_owned(), *body))
                    .collect(),
            )
        }
    }

    impl Caller for Tree {
        fn fetch(&mut self, url: &str) -> Result<Vec<u8>, ci_url::Error> {
            let path = url.trim_start_matches("http://md/");
            self.0
                .get(path)
                .map(|body| body.as_bytes().to_vec())
                .ok_or(ci_url::Error {
                    url: url.to_owned(),
                    code: Some(404),
                    message: "404 Client Error".to_owned(),
                    tls: false,
                })
        }
    }

    fn crawled(tree: &Tree) -> Object {
        let mut tree = Tree(tree.0.clone());
        let mut log = ci_log::Logger::silent();
        instance_metadata("latest", "http://md", &mut tree, &mut log)
    }

    #[test]
    fn a_directory_entry_becomes_a_nested_object() {
        let tree = Tree::new(&[
            ("latest/meta-data/", "instance-id\nplacement/\n"),
            ("latest/meta-data/instance-id", "i-abc"),
            ("latest/meta-data/placement/", "availability-zone\n"),
            ("latest/meta-data/placement/availability-zone", "us-east-1a"),
        ]);

        let md = crawled(&tree);

        assert_eq!(md["instance-id"], Value::String("i-abc".to_owned()));
        assert_eq!(
            md["placement"]["availability-zone"],
            Value::String("us-east-1a".to_owned())
        );
    }

    #[test]
    fn a_numbered_public_key_is_read_from_its_openssh_key_resource() {
        let tree = Tree::new(&[
            ("latest/meta-data/", "public-keys/\n"),
            ("latest/meta-data/public-keys/", "0=my-key\n"),
            ("latest/meta-data/public-keys/0/openssh-key", "ssh-rsa AAAA"),
        ]);

        let md = crawled(&tree);

        assert_eq!(
            md["public-keys"]["my-key"],
            Value::String("ssh-rsa AAAA".to_owned())
        );
    }

    #[test]
    fn security_credentials_are_never_fetched() {
        let tree = Tree::new(&[
            ("latest/meta-data/", "iam/\n"),
            ("latest/meta-data/iam/", "security-credentials/\ninfo\n"),
            ("latest/meta-data/iam/info", "{}"),
        ]);

        let md = crawled(&tree);

        assert!(md["iam"].as_object().unwrap().contains_key("info"));
        assert!(!md["iam"]
            .as_object()
            .unwrap()
            .contains_key("security-credentials"));
    }

    #[test]
    fn a_leaf_that_looks_like_an_object_is_parsed_as_one() {
        assert_eq!(
            decode_leaf("info", br#"{"Code": "Success"}"#)["Code"],
            "Success"
        );
    }

    #[test]
    fn a_multi_line_leaf_becomes_a_list_and_a_single_line_stays_a_string() {
        assert_eq!(
            decode_leaf("keys", b"one\ntwo"),
            Value::Array(vec!["one".into(), "two".into()])
        );
        assert_eq!(decode_leaf("keys", b"one"), Value::String("one".to_owned()));
    }

    #[test]
    fn an_undecodable_leaf_is_carried_as_base64() {
        assert_eq!(
            decode_leaf("blob", &[0xff, 0xfe]),
            Value::String("ci-b64://4=".to_owned())
        );
    }

    #[test]
    fn a_tree_that_nests_forever_is_abandoned_rather_than_followed() {
        struct Endless;
        impl Caller for Endless {
            fn fetch(&mut self, _url: &str) -> Result<Vec<u8>, ci_url::Error> {
                Ok(b"deeper/\n".to_vec())
            }
        }

        let mut log = ci_log::Logger::silent();
        let md = instance_metadata("latest", "http://md", &mut Endless, &mut log);

        let mut depth = 0;
        let mut node = &Value::Object(md);
        while let Some(next) = node.get("deeper") {
            depth += 1;
            node = next;
        }
        assert_eq!(depth, MAX_DEPTH);
    }

    #[test]
    fn missing_userdata_reads_as_empty_rather_than_failing() {
        let mut tree = Tree::new(&[]);
        let mut log = ci_log::Logger::silent();

        assert!(
            instance_userdata("latest", "http://md", &mut tree, &mut log).is_empty()
        );
    }
}
