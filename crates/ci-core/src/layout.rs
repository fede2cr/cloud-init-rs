//! The filesystem contract: what cloud-init's paths must look like.
//!
//! `packaging/filesystem-contract.toml` is embedded here at build time and is
//! the single source of truth for every mode, owner and group cloud-init owns
//! at runtime. Its header explains *why* it exists; this module is the reader,
//! the auditor and the generator for the files derived from it.
//!
//! Four consumers:
//!
//! * [`verify`] audits a live (or fixture) root and reports drift. It backs
//!   `cloud-init devel verify-layout`, which is both an admin diagnostic and
//!   the package test that runs after an install, a switch and a revert.
//! * [`tmpfiles_d`] renders the `systemd-tmpfiles` fragment that creates the
//!   state directories at boot. The committed copy under `systemd/tmpfiles.d/`
//!   is compared against this function by a test, so the two cannot drift.
//! * [`selinux_fc`] renders the file-context rules of the `SELinux` policy
//!   module, under the same committed-copy-plus-test arrangement.
//! * The parsed [`Entry`] list is what the runtime consults before it creates
//!   one of these paths itself.
//!
//! The parser is hand-written against a strict subset of TOML rather than
//! pulling in a TOML crate: the grammar is fifteen lines, the file is ours, and
//! a parser that rejects everything it does not recognise is a better fit for a
//! security contract than one that is liberal in what it accepts.

use std::fmt;
use std::path::{Path, PathBuf};

/// The contract text, embedded so a stripped-down install cannot lose it and
/// so the build fails rather than the audit silently passing.
pub const CONTRACT_TOML: &str =
    include_str!("../../../packaging/filesystem-contract.toml");

/// What a contract path is supposed to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
}

impl Kind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "dir" => Some(Self::Dir),
            "file" => Some(Self::File),
            "symlink" => Some(Self::Symlink),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::File => "file",
            Self::Symlink => "symlink",
        }
    }
}

/// Whether a provisioned system must have the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Required,
    Optional,
}

/// Who is supposed to have created it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Creator {
    Package,
    Runtime,
    Tmpfiles,
}

/// One `[[path]]` entry.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Absolute, without a trailing slash. A trailing `/*` means "every direct
    /// child of this directory".
    pub path: String,
    pub kind: Kind,
    /// Permission bits only; the file-type bits are described by `kind`.
    pub mode: u32,
    /// Acceptable owner names, in the order they were written. Where there is
    /// more than one, any of them satisfies the contract and the first is the
    /// one generated files use.
    pub owners: Vec<String>,
    pub groups: Vec<String>,
    pub presence: Presence,
    pub creator: Creator,
    pub selinux_type: Option<String>,
    pub note: Option<String>,
}

impl Entry {
    /// True where `path` ends in `/*`, meaning the entry describes the
    /// directory's children rather than the directory itself.
    pub fn is_children(&self) -> bool {
        self.path.ends_with("/*")
    }

    /// The directory whose children are described, for a `/*` entry.
    fn parent_of_children(&self) -> &str {
        self.path.strip_suffix("/*").unwrap_or(&self.path)
    }
}

/// A line number and a reason. The contract is ours, so a parse failure is a
/// build-time bug rather than user input, but it still says exactly where.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub line: usize,
    pub reason: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "filesystem-contract.toml:{}: {}", self.line, self.reason)
    }
}

impl std::error::Error for ParseError {}

/// Parse the embedded contract.
pub fn contract() -> Result<Vec<Entry>, ParseError> {
    parse(CONTRACT_TOML)
}

/// Parse contract text. Separated from [`contract`] so the tests can feed it
/// deliberately broken input.
pub fn parse(text: &str) -> Result<Vec<Entry>, ParseError> {
    let mut entries: Vec<Entry> = Vec::new();
    // Fields of the entry currently being read, as (key, value, line).
    let mut fields: Vec<(String, String, usize)> = Vec::new();
    let mut open: Option<usize> = None;

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let content = strip_comment(raw).trim();
        if content.is_empty() {
            continue;
        }

        if content == "[[path]]" {
            if let Some(start) = open.take() {
                entries.push(build(&fields, start)?);
                fields.clear();
            }
            open = Some(line);
            continue;
        }

        let Some(start) = open else {
            return Err(ParseError {
                line,
                reason: format!("expected `[[path]]` before `{content}`"),
            });
        };
        let _ = start;

        let (key, value) = split_assignment(content).ok_or_else(|| ParseError {
            line,
            reason: format!("not a `key = \"value\"` assignment: `{content}`"),
        })?;
        if fields.iter().any(|(k, _, _)| k == key) {
            return Err(ParseError {
                line,
                reason: format!("duplicate key `{key}`"),
            });
        }
        fields.push((key.to_owned(), value.to_owned(), line));
    }

    if let Some(start) = open {
        entries.push(build(&fields, start)?);
    }
    if entries.is_empty() {
        return Err(ParseError {
            line: 0,
            reason: "the contract is empty".to_owned(),
        });
    }
    Ok(entries)
}

/// `#` starts a comment, except inside a quoted value — which is why this
/// scans rather than calling `split_once('#')`. Contract notes contain `#`.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quoted = false;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b'#' if !quoted => return line.get(..index).unwrap_or(""),
            _ => {}
        }
    }
    line
}

/// `key = "value"`, with no escapes inside the value.
fn split_assignment(content: &str) -> Option<(&str, &str)> {
    let (key, rest) = content.split_once('=')?;
    let rest = rest.trim();
    let value = rest.strip_prefix('"')?.strip_suffix('"')?;
    if value.contains('"') || value.contains('\\') {
        return None;
    }
    Some((key.trim(), value))
}

fn build(
    fields: &[(String, String, usize)],
    start: usize,
) -> Result<Entry, ParseError> {
    const KNOWN: &[&str] = &[
        "path",
        "kind",
        "mode",
        "owner",
        "group",
        "presence",
        "creator",
        "selinux_type",
        "note",
    ];

    let get = |name: &str| {
        fields
            .iter()
            .find(|(k, _, _)| k == name)
            .map(|(_, v, _)| v.as_str())
    };
    let need = |name: &str| {
        get(name).ok_or_else(|| ParseError {
            line: start,
            reason: format!("missing required key `{name}`"),
        })
    };

    if let Some((key, _, line)) =
        fields.iter().find(|(k, _, _)| !KNOWN.contains(&k.as_str()))
    {
        return Err(ParseError {
            line: *line,
            reason: format!("unknown key `{key}`"),
        });
    }

    let path = need("path")?.to_owned();
    if !path.starts_with('/') || (path.ends_with('/') && path != "/") {
        return Err(ParseError {
            line: start,
            reason: format!("`path` must be absolute with no trailing slash: `{path}`"),
        });
    }

    let kind_str = need("kind")?;
    let kind = Kind::parse(kind_str).ok_or_else(|| ParseError {
        line: start,
        reason: format!("unknown kind `{kind_str}`"),
    })?;

    let mode_str = need("mode")?;
    if mode_str.len() != 4 {
        return Err(ParseError {
            line: start,
            reason: format!("`mode` must be four octal digits, got `{mode_str}`"),
        });
    }
    let mode = u32::from_str_radix(mode_str, 8).map_err(|_| ParseError {
        line: start,
        reason: format!("`mode` is not octal: `{mode_str}`"),
    })?;

    let presence_str = need("presence")?;
    let presence = match presence_str {
        "required" => Presence::Required,
        "optional" => Presence::Optional,
        other => {
            return Err(ParseError {
                line: start,
                reason: format!("unknown presence `{other}`"),
            })
        }
    };

    let creator_str = need("creator")?;
    let creator = match creator_str {
        "package" => Creator::Package,
        "runtime" => Creator::Runtime,
        "tmpfiles" => Creator::Tmpfiles,
        other => {
            return Err(ParseError {
                line: start,
                reason: format!("unknown creator `{other}`"),
            })
        }
    };

    let names = |value: &str| -> Vec<String> {
        value
            .split('|')
            .map(str::trim)
            .map(ToOwned::to_owned)
            .collect()
    };

    Ok(Entry {
        path,
        kind,
        mode,
        owners: names(need("owner")?),
        groups: names(need("group")?),
        presence,
        creator,
        selinux_type: get("selinux_type").map(ToOwned::to_owned),
        note: get("note").map(ToOwned::to_owned),
    })
}

/// One way a path on disk disagrees with the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    Missing,
    WrongKind {
        found: &'static str,
        want: &'static str,
    },
    WrongMode {
        found: u32,
        want: u32,
    },
    WrongOwner {
        found: u32,
        want: String,
    },
    WrongGroup {
        found: u32,
        want: String,
    },
    Unreadable(String),
}

/// A [`Problem`] with the path it was found on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: PathBuf,
    pub problem: Problem,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.problem {
            Problem::Missing => write!(f, "{path}: missing"),
            Problem::WrongKind { found, want } => {
                write!(f, "{path}: is a {found}, contract says {want}")
            }
            Problem::WrongMode { found, want } => {
                write!(f, "{path}: mode {found:04o}, contract says {want:04o}")
            }
            Problem::WrongOwner { found, want } => {
                write!(f, "{path}: owned by uid {found}, contract says {want}")
            }
            Problem::WrongGroup { found, want } => {
                write!(f, "{path}: group gid {found}, contract says {want}")
            }
            Problem::Unreadable(why) => write!(f, "{path}: {why}"),
        }
    }
}

/// Audit `root` against the contract.
///
/// `root` is `/` for a live system and a fixture directory in tests; every
/// contract path is joined onto it. The return is every disagreement found, in
/// contract order, so the caller can print them all rather than stopping at the
/// first — an admin fixing drift wants the whole list.
///
/// A `required` path that is missing is a finding. An `optional` path that is
/// missing is not: most of them only exist after a successful crawl.
pub fn verify(root: &Path, entries: &[Entry]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for entry in entries {
        if entry.is_children() {
            let parent = join(root, entry.parent_of_children());
            let Ok(children) = std::fs::read_dir(&parent) else {
                // The parent's own entry reports it if it should have existed.
                continue;
            };
            let mut paths: Vec<PathBuf> =
                children.filter_map(|c| c.ok().map(|c| c.path())).collect();
            paths.sort();
            for child in paths {
                check(&child, entry, root, &mut findings);
            }
        } else {
            check(&join(root, &entry.path), entry, root, &mut findings);
        }
    }
    findings
}

/// `Path::join` treats a leading `/` as "start over", which would silently
/// audit the live system instead of the fixture.
fn join(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

fn check(path: &Path, entry: &Entry, root: &Path, findings: &mut Vec<Finding>) {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    // `symlink_metadata`, not `metadata`: a contract entry describes the path
    // itself. Following the link would audit the target's mode and would let a
    // symlink planted in a shared directory hide a wrong one.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if entry.presence == Presence::Required {
                findings.push(Finding {
                    path: path.to_owned(),
                    problem: Problem::Missing,
                });
            }
            return;
        }
        Err(e) => {
            findings.push(Finding {
                path: path.to_owned(),
                problem: Problem::Unreadable(e.to_string()),
            });
            return;
        }
    };

    let found_kind = if meta.file_type().is_symlink() {
        "symlink"
    } else if meta.is_dir() {
        "dir"
    } else if meta.is_file() {
        "file"
    } else {
        "special file"
    };
    if found_kind != entry.kind.as_str() {
        findings.push(Finding {
            path: path.to_owned(),
            problem: Problem::WrongKind {
                found: found_kind,
                want: entry.kind.as_str(),
            },
        });
        return;
    }

    // A symlink's own mode is 0777 everywhere Linux is concerned and cannot be
    // changed, so checking it would only ever produce noise.
    if entry.kind != Kind::Symlink {
        let mode = meta.permissions().mode() & 0o7777;
        if mode != entry.mode {
            findings.push(Finding {
                path: path.to_owned(),
                problem: Problem::WrongMode {
                    found: mode,
                    want: entry.mode,
                },
            });
        }
    }

    // An unresolvable name is not reported as drift: the audit cannot tell the
    // difference between the wrong owner and a name this host does not know,
    // and guessing in either direction is worse than saying nothing.
    let allowed_uids: Vec<u32> = entry
        .owners
        .iter()
        .filter_map(|name| ci_sys::ids::uid_for_name(root, name))
        .collect();
    if !allowed_uids.is_empty() && !allowed_uids.contains(&meta.uid()) {
        findings.push(Finding {
            path: path.to_owned(),
            problem: Problem::WrongOwner {
                found: meta.uid(),
                want: entry.owners.join(" or "),
            },
        });
    }

    let group_ids: Vec<u32> = entry
        .groups
        .iter()
        .filter_map(|name| ci_sys::ids::gid_for_name(root, name))
        .collect();
    if !group_ids.is_empty() && !group_ids.contains(&meta.gid()) {
        findings.push(Finding {
            path: path.to_owned(),
            problem: Problem::WrongGroup {
                found: meta.gid(),
                want: entry.groups.join(" or "),
            },
        });
    }
}

/// Render the `systemd-tmpfiles` fragment.
///
/// Only `creator = "tmpfiles"` entries appear: those are the state directories
/// that must exist before the first stage runs. Files cloud-init writes itself
/// are deliberately absent — tmpfiles would create them empty, and an empty
/// `instance-data.json` is worse than a missing one.
///
/// `d` rather than `D`: the directory is created if absent and its mode fixed,
/// but its contents are never removed. `/var/lib/cloud` outlives reboots and
/// package operations by design.
pub fn tmpfiles_d(entries: &[Entry]) -> String {
    use std::fmt::Write as _;

    let mut out = String::from(
        "# Generated from packaging/filesystem-contract.toml. Do not edit.\n\
         #\n\
         # Regenerate with `cloud-init devel verify-layout --dump-tmpfiles`; a\n\
         # unit test fails if this file and the contract disagree.\n\
         #\n\
         # Type Path Mode User Group Age Argument\n",
    );
    for entry in entries {
        if entry.creator != Creator::Tmpfiles || entry.kind != Kind::Dir {
            continue;
        }
        if let Some(note) = &entry.note {
            out.push_str("# ");
            out.push_str(note);
            out.push('\n');
        }
        let owner = entry.owners.first().map_or("root", String::as_str);
        let group = entry.groups.first().map_or("root", String::as_str);
        let _ = writeln!(
            out,
            "d {} {:04o} {} {} - -",
            entry.path, entry.mode, owner, group
        );
    }
    out
}

/// Escape a literal path so it can appear in an `SELinux` file-context regex.
///
/// `.fc` left-hand sides are regular expressions, not globs, so an unescaped
/// `.` in `instance-data.json` would also match `instance-dataXjson`. Only the
/// metacharacters that can occur in a contract path are handled; `/` is not one
/// of them and must stay literal.
fn fc_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(
            c,
            '.' | '^'
                | '$'
                | '*'
                | '+'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Render the `SELinux` file-context rules generated from the contract.
///
/// This is the `.fc` half of the policy module of PLAN.md §6.3. It exists for
/// the same reason as [`tmpfiles_d`]: the labels a shared state tree must carry
/// are a property of the tree, not of whichever implementation created it, so
/// they are written down once and everything else is generated.
///
/// Two things are deliberate here. The file-type specifier is derived from
/// `kind`, so a path that is a symlink upstream (`/run/cloud-init/status.json`)
/// gets `-l` and is not relabelled as if it were a regular file — labelling a
/// symlink as its target's type is how a `restorecon -R` ends up silently
/// wrong. And a `/*` children entry becomes `/[^/]*` rather than `(/.*)?`,
/// because the contract's `/*` means *direct* children only; a recursive
/// pattern would claim paths the contract says nothing about.
///
/// Note what this does **not** do: it never invents a type. Every type named
/// here comes from the contract, which was read off a Python-provisioned
/// system, so the rules can only ever reproduce upstream's labelling — never a
/// more permissive one.
pub fn selinux_fc(entries: &[Entry]) -> String {
    use std::fmt::Write as _;

    let mut out = String::from(
        "# Generated from packaging/filesystem-contract.toml. Do not edit.\n\
         #\n\
         # Regenerate with `cloud-init devel verify-layout --dump-selinux-fc`; a\n\
         # unit test fails if this file and the contract disagree.\n\
         #\n\
         # These are the labels a Python-provisioned system carries. The Rust\n\
         # binaries are labelled by equivalence rather than by a rule here, so\n\
         # that they inherit whatever the base policy says about the Python\n\
         # ones:\n\
         #\n\
         #   semanage fcontext -a -e /usr/bin/cloud-init \\\n\
         #       /usr/libexec/cloud-init-rs/cloud-init\n\
         #\n\
         # Path Filetype Context\n",
    );
    for entry in entries {
        let Some(selinux_type) = &entry.selinux_type else {
            continue;
        };
        if let Some(note) = &entry.note {
            out.push_str("# ");
            out.push_str(note);
            out.push('\n');
        }
        let (pattern, spec) = if entry.is_children() {
            // The children of a directory, with no constraint on what they are:
            // `/var/lib/cloud/seed/*` holds files and directories both.
            (
                format!("{}/[^/]*", fc_escape(entry.parent_of_children())),
                "",
            )
        } else {
            let spec = match entry.kind {
                Kind::Dir => "-d",
                Kind::File => "--",
                Kind::Symlink => "-l",
            };
            (fc_escape(&entry.path), spec)
        };
        let _ = writeln!(
            out,
            "{pattern}\t{spec}\tgen_context(system_u:object_r:{selinux_type},s0)"
        );
    }
    out
}

/// The `/var/lib/cloud/instance` symlink, and the glob its target always
/// matches. `AppArmor` mediates the path the kernel resolved, so anything
/// *under* the link has to be written in terms of the target; the link's own
/// name is left alone, because the last component of a path is what a symlink
/// rule is about.
const INSTANCE_LINK_PREFIX: &str = "/var/lib/cloud/instance/";
const INSTANCE_TARGET_PREFIX: &str = "/var/lib/cloud/instances/*/";

/// The `AppArmor` path pattern for one contract entry.
///
/// A directory rule needs its trailing slash — `/var/lib/cloud` and
/// `/var/lib/cloud/` are two different rules to the parser, and only the second
/// one permits creating an entry in it. A `/*` children entry needs no special
/// handling: `AppArmor`'s `*` already means "one path component", which is what
/// the contract's `/*` means too.
fn apparmor_pattern(entry: &Entry) -> String {
    let path = match entry.path.strip_prefix(INSTANCE_LINK_PREFIX) {
        Some(rest) => format!("{INSTANCE_TARGET_PREFIX}{rest}"),
        None => entry.path.clone(),
    };
    match entry.kind {
        Kind::Dir => format!("{path}/"),
        Kind::File | Kind::Symlink => path,
    }
}

/// Render the `AppArmor` profile for the shipped binaries.
///
/// The third file generated from the contract, for the third time for the same
/// reason: the set of paths cloud-init is *permitted* to touch and the set it
/// is *audited* against are the same set, so they are written down once.
///
/// Two rules turn contract data into policy. `creator = "package"` entries are
/// administrator configuration that cloud-init reads and never writes, so they
/// are `r`; everything cloud-init creates itself is `rw`. And entries under
/// `/var/lib/cloud/instance/` are emitted under `/var/lib/cloud/instances/*/`,
/// because `AppArmor` mediates the resolved path — a rule on the symlink's own
/// name would sit in the profile looking correct and never match a thing.
///
/// The profile ships in complain mode; the header explains why at length, and
/// PLAN.md §6.3 makes enforce mode a Phase 7 gate.
pub fn apparmor_profile(entries: &[Entry]) -> String {
    use std::fmt::Write as _;

    let mut out = String::from(APPARMOR_PROLOGUE);
    for entry in entries {
        if let Some(note) = &entry.note {
            let _ = writeln!(out, "  # {note}");
        }
        let access = if entry.creator == Creator::Package {
            "r"
        } else {
            "rw"
        };
        let _ = writeln!(out, "  {} {access},", apparmor_pattern(entry));
    }
    out.push_str(APPARMOR_EPILOGUE);
    out
}

/// Everything above the generated contract rules.
const APPARMOR_PROLOGUE: &str = r#"# Generated from packaging/filesystem-contract.toml. Do not edit.
#
# Regenerate with `cloud-init devel verify-layout --dump-apparmor`; a
# unit test fails if this file and the contract disagree.
#
# Attached to the coexistence package's real binary paths under
# /usr/libexec/cloud-init-rs/ rather than to /usr/bin/cloud-init, so it
# confines this implementation whether or not update-alternatives currently
# points at it, and so it can never be applied to the Python one by accident.
#
# It ships in complain mode. cloud-init exists to run administrator-supplied
# code as root, and an enforcing profile written from the source rather than
# from evidence would deny something real on the first unusual cloud it met.
# PLAN.md 6.3 makes enforce mode a Phase 7 gate, after boots on real clouds
# have been audited. Complain mode is not a placeholder in the meantime:
# every access is logged, so the audit log becomes a measured inventory of
# what cloud-init actually touches, which is the only honest basis for the
# enforcing version.
#
# attach_disconnected is deliberately absent. The local stage runs early
# enough that some paths cannot be resolved to the namespace root; in
# complain mode that is a log entry rather than a failure, and leaving the
# flag off is what makes those cases visible instead of waving them through
# before anyone has read one.
#
# Child processes run unconfined. That is not a shortcut: runcmd, bootcmd
# and the scripts_* modules exist to run tenant-supplied programs as root, so
# there is no policy to write for them that would mean anything, and netplan,
# useradd, apt-get and ssh-keygen carry profiles of their own where the
# distribution ships them. Confining cloud-init itself is the part with
# content.

abi <abi/3.0>,

include <tunables/global>

profile cloud-init-rs /usr/libexec/cloud-init-rs/{cloud-init,cloud-id,cloud-init-per,ds-identify,cloud-init-generator} flags=(complain) {
  include <abstractions/base>
  include <abstractions/consoles>
  include <abstractions/nameservice>
  include <abstractions/openssl>
  include <abstractions/ssl_certs>

  # Ownership and mode work on files cloud-init does not own: authorized_keys
  # written into a user's home, the 0400 datasource cache, the 0600 instance
  # tree.
  capability chown,
  capability dac_override,
  capability dac_read_search,
  capability fowner,
  capability fsetid,

  # util.is_container reads PID 1's environment.
  ptrace (read) peer=unconfined,

  # A subprocess that outruns its deadline is killed; bootcmd caps a tenant
  # command at an hour.
  signal (send) set=(term, kill) peer=unconfined,
  signal (receive) peer=unconfined,

  # Metadata services: IMDS, the Azure wireserver, a NoCloud seedfrom URL, and
  # the resolver underneath them. netlink raw is what getaddrinfo uses to
  # enumerate interfaces.
  network inet stream,
  network inet6 stream,
  network inet dgram,
  network inet6 dgram,
  network netlink raw,

  # The LXD datasource's socket, the /run/cloud-init/share sockets that
  # --all-stages synchronises on, and sd_notify.
  network unix stream,
  network unix dgram,

  # --- the filesystem contract ------------------------------------------
  #
  # Generated from packaging/filesystem-contract.toml, so the paths cloud-init
  # may touch and the paths verify-layout audits it against cannot drift
  # apart. `creator = "package"` entries are administrator configuration and
  # are readable only; everything cloud-init creates itself is rw.
  #
  # Entries under /var/lib/cloud/instance/ appear here as
  # /var/lib/cloud/instances/*/ because AppArmor mediates the path the kernel
  # resolved, not the one the process asked for.

"#;

/// Everything below the generated contract rules.
const APPARMOR_EPILOGUE: &str = r"
  # --- everything outside the contract ----------------------------------

  # What identifies the machine: ds-identify reads DMI and the kernel command
  # line, the datasources read the NIC list and the hypervisor UUID.
  /proc/ r,
  /proc/cmdline r,
  /proc/meminfo r,
  /proc/mounts r,
  /proc/uptime r,
  /proc/1/environ r,
  /proc/@{pid}/{cmdline,mountinfo,mounts,stat} r,
  /proc/sys/kernel/{hostname,osrelease} r,
  /proc/sys/kernel/random/boot_id r,
  /sys/class/dmi/id/* r,
  /sys/class/net/ r,
  /sys/class/net/** r,
  /sys/devices/** r,
  /sys/hypervisor/uuid r,
  /run/systemd/system/ r,
  /etc/os-release r,
  /usr/lib/os-release r,
  /etc/machine-id rw,
  /etc/cloud/cloud-init.disabled r,
  /dev/urandom r,

  # Hostname, hosts, timezone and locale.
  /etc/hostname rw,
  /etc/hosts rw,
  /etc/timezone rw,
  /etc/localtime rw,
  /etc/default/locale rw,
  /etc/locale.gen rw,
  /usr/share/zoneinfo/** r,

  # Network rendering, and the files the activators read back.
  /etc/netplan/ rw,
  /etc/netplan/*.yaml rw,
  /etc/network/interfaces.d/ rw,
  /etc/network/interfaces.d/* rw,
  /etc/NetworkManager/system-connections/ rw,
  /etc/NetworkManager/system-connections/* rw,
  /etc/systemd/network/ rw,
  /etc/systemd/network/* rw,
  /etc/sysconfig/network-scripts/ rw,
  /etc/sysconfig/network-scripts/* rw,
  /etc/resolv.conf rw,
  /run/netplan/** rw,
  /run/systemd/resolve/*.conf r,

  # Accounts and SSH. cc_users_groups reads the databases its child useradd
  # writes, cc_ssh replaces the host keys, and ssh_util writes authorized_keys
  # into homes cloud-init does not own.
  /etc/{passwd,group,shadow,gshadow} r,
  /etc/sudoers.d/ rw,
  /etc/sudoers.d/* rw,
  /etc/doas.conf rw,
  /etc/ssh/sshd_config r,
  /etc/ssh/sshd_config.d/ rw,
  /etc/ssh/sshd_config.d/*.conf rw,
  /etc/ssh/ssh_host_* rw,
  /root/.ssh/ rw,
  /root/.ssh/authorized_keys* rw,
  /home/*/.ssh/ rw,
  /home/*/.ssh/authorized_keys* rw,

  # Packages, repositories, certificates and syslog.
  /etc/apt/ rw,
  /etc/apt/** rw,
  /etc/ca-certificates.conf rw,
  /usr/local/share/ca-certificates/ rw,
  /usr/local/share/ca-certificates/** rw,
  /etc/rsyslog.d/ rw,
  /etc/rsyslog.d/* rw,

  # Storage. cc_mounts rewrites fstab; cc_disk_setup, cc_growpart and
  # cc_resizefs inspect block devices before handing them to a child.
  /etc/fstab rw,
  /dev/ r,
  /dev/disk/{,**} r,
  /dev/sd[a-z]* r,
  /dev/vd[a-z]* r,
  /dev/nvme[0-9]*n[0-9]*{,p[0-9]*} r,
  /dev/sr[0-9]* r,
  /mnt/ rw,
  /media/{,**} rw,

  # Seeds and per-cloud state that is not cloud-init's own.
  /var/lib/waagent/{,**} r,
  /var/lib/hyperv/.kvp_pool_* rw,

  # The instance directory holds more than the contract names: the handler,
  # script and semaphore trees, and the marker files that decide whether the
  # next boot is a new instance.
  /var/lib/cloud/instances/*/{,**} rw,
  /var/lib/cloud/{handlers,scripts,seed,sem}/{,**} rw,

  # ci_sys::atomic writes `.<name>.<pid>.<seq>.tmp` beside its target and
  # renames over it, so every directory cloud-init writes into needs the
  # sibling as well as the file.
  /etc/.*.tmp rw,
  /etc/*/.*.tmp rw,
  /run/cloud-init/.*.tmp rw,
  /var/lib/cloud/**/.*.tmp rw,
  /var/log/.*.tmp rw,

  # A mounted seed medium, and the scratch trees mount_cb makes for it.
  /tmp/ rw,
  /tmp/** rwlk,
  /var/tmp/ rw,
  /var/tmp/** rwlk,

  # --- child processes --------------------------------------------------
  #
  # Ux rather than ux, so the environment is scrubbed on the way in.
  # ci_sys::subp clears it too; the two agreeing is worth more than either
  # on its own. The generator runs ds-identify, which is one of ours, so that
  # one stays in the profile -- and nothing else may match that path, because
  # two overlapping rules with different exec transitions is a parse error,
  # not a precedence question.
  /usr/libexec/cloud-init-rs/* Px,
  /{,usr/}{,s}bin/* Ux,
  /usr/local/{,s}bin/* Ux,
  /usr/lib/cloud-init/* Ux,
  /snap/bin/* Ux,
  /var/lib/cloud/instances/*/scripts/* Ux,
  /var/lib/cloud/scripts/*/* Ux,

  include if exists <local/cloud-init-rs>
}
";

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_contract_parses() {
        let entries = contract().unwrap();
        assert!(entries.len() > 30, "{}", entries.len());
    }

    #[test]
    fn every_contract_path_is_unique() {
        let entries = contract().unwrap();
        let mut seen: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate path in the contract");
    }

    /// The whole point of the file: the two paths that must never be
    /// world-readable, and the one that must be.
    #[test]
    fn the_sensitive_paths_are_owner_only() {
        let entries = contract().unwrap();
        let mode = |path: &str| {
            entries
                .iter()
                .find(|e| e.path == path)
                .unwrap_or_else(|| panic!("{path} is not in the contract"))
                .mode
        };
        assert_eq!(mode("/run/cloud-init/instance-data-sensitive.json"), 0o600);
        assert_eq!(mode("/var/lib/cloud/instance/user-data.txt"), 0o600);
        assert_eq!(mode("/run/cloud-init/instance-data.json"), 0o644);
    }

    #[test]
    fn the_committed_tmpfiles_fragment_matches_the_contract() {
        let generated = tmpfiles_d(&contract().unwrap());
        let committed = include_str!("../../../systemd/tmpfiles.d/cloud-init-rs.conf");
        assert_eq!(
            generated, committed,
            "systemd/tmpfiles.d/cloud-init-rs.conf is stale; \
             run `cloud-init devel verify-layout --dump-tmpfiles > \
             systemd/tmpfiles.d/cloud-init-rs.conf`"
        );
    }

    #[test]
    fn the_committed_selinux_fc_matches_the_contract() {
        let generated = selinux_fc(&contract().unwrap());
        let committed = include_str!("../../../selinux/cloud-init-rs.fc");
        assert_eq!(
            generated, committed,
            "selinux/cloud-init-rs.fc is stale; \
             run `cloud-init devel verify-layout --dump-selinux-fc > \
             selinux/cloud-init-rs.fc`"
        );
    }

    #[test]
    fn the_committed_apparmor_profile_matches_the_contract() {
        let generated = apparmor_profile(&contract().unwrap());
        let committed = include_str!("../../../apparmor/cloud-init-rs");
        assert_eq!(
            generated, committed,
            "apparmor/cloud-init-rs is stale; \
             run `cloud-init devel verify-layout --dump-apparmor > \
             apparmor/cloud-init-rs`"
        );
    }

    /// The three rules that turn contract data into policy, and the one that
    /// would be silently wrong if it were left out: a rule naming the
    /// `/var/lib/cloud/instance` symlink can never match, because `AppArmor`
    /// mediates the path the kernel resolved.
    #[test]
    fn apparmor_rules_follow_the_instance_symlink_and_the_creator_column() {
        let profile = apparmor_profile(&contract().unwrap());

        assert!(
            profile.contains("\n  /var/lib/cloud/instances/*/user-data.txt rw,\n"),
            "{profile}"
        );
        assert!(
            !profile.contains("/var/lib/cloud/instance/user-data.txt"),
            "{profile}"
        );
        // The link itself is still named: creating a symlink is mediated on
        // the link's own path, not on its target.
        assert!(
            profile.contains("\n  /var/lib/cloud/instance rw,\n"),
            "{profile}"
        );

        // Administrator configuration is read-only; state is not.
        assert!(
            profile.contains("\n  /etc/cloud/cloud.cfg r,\n"),
            "{profile}"
        );
        assert!(
            profile.contains("\n  /run/cloud-init/instance-data.json rw,\n"),
            "{profile}"
        );

        // Directory rules carry the trailing slash that makes them directory
        // rules; without it the parser reads them as regular files.
        assert!(profile.contains("\n  /var/lib/cloud/ rw,\n"), "{profile}");
        assert!(
            profile.contains("\n  /var/lib/cloud/instances/*/ rw,\n"),
            "{profile}"
        );
    }

    /// Complain mode is the shipped state and is load-bearing for PLAN §6.3;
    /// a change to enforce is a Phase 7 decision, not an edit.
    #[test]
    fn the_apparmor_profile_ships_in_complain_mode() {
        let profile = apparmor_profile(&contract().unwrap());
        let header = profile
            .lines()
            .find(|l| l.starts_with("profile "))
            .expect("no profile header");
        assert!(header.ends_with("flags=(complain) {"), "{header}");
        // Named in the comments as a decision, never as a flag.
        assert!(!header.contains("attach_disconnected"), "{header}");
        assert!(
            header.starts_with("profile cloud-init-rs /usr/libexec/cloud-init-rs/{"),
            "{header}"
        );
        assert_eq!(
            profile.matches('{').count(),
            profile.matches('}').count(),
            "unbalanced braces in the generated profile"
        );
    }

    #[test]
    fn fc_patterns_escape_regex_metacharacters_and_carry_a_file_type() {
        let fc = selinux_fc(&contract().unwrap());

        // A dot in a filename must not be left as "any character".
        assert!(
            fc.contains("/run/cloud-init/instance-data\\.json\t--\t"),
            "{fc}"
        );
        // Symlinks are labelled as symlinks, not as their targets.
        assert!(fc.contains("/run/cloud-init/status\\.json\t-l\t"), "{fc}");
        // Directories carry -d.
        assert!(fc.contains("/var/lib/cloud\t-d\t"), "{fc}");
        // A `/*` entry becomes a direct-children pattern, not a recursive one.
        assert!(fc.contains("/[^/]*\t\tgen_context("), "{fc}");
        assert!(!fc.contains("(/.*)?"), "{fc}");

        // Every rule names a type that the contract named.
        for line in fc.lines().filter(|l| !l.starts_with('#')) {
            assert!(
                line.contains("gen_context(system_u:object_r:cloud_"),
                "{line}"
            );
        }
    }

    #[test]
    fn a_comment_inside_a_quoted_value_is_not_a_comment() {
        let entries = parse(
            "[[path]]\n\
             path = \"/x\"\n\
             kind = \"file\"\n\
             mode = \"0600\"\n\
             owner = \"root\"\n\
             group = \"root\"\n\
             presence = \"optional\"\n\
             creator = \"runtime\"\n\
             note = \"a # b\"  # trailing\n",
        )
        .unwrap();
        assert_eq!(entries[0].note.as_deref(), Some("a # b"));
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let err = parse(
            "[[path]]\n\
             path = \"/x\"\n\
             kind = \"file\"\n\
             mode = \"0600\"\n\
             owner = \"root\"\n\
             group = \"root\"\n\
             presence = \"optional\"\n\
             creator = \"runtime\"\n\
             modee = \"0644\"\n",
        )
        .unwrap_err();
        assert!(err.reason.contains("unknown key `modee`"), "{err}");
    }

    #[test]
    fn a_three_digit_mode_is_refused() {
        let err = parse(
            "[[path]]\n\
             path = \"/x\"\n\
             kind = \"file\"\n\
             mode = \"600\"\n\
             owner = \"root\"\n\
             group = \"root\"\n\
             presence = \"optional\"\n\
             creator = \"runtime\"\n",
        )
        .unwrap_err();
        assert!(err.reason.contains("four octal digits"), "{err}");
    }

    #[test]
    fn alternative_owners_are_split_on_a_pipe() {
        let entries = contract().unwrap();
        let log = entries
            .iter()
            .find(|e| e.path == "/var/log/cloud-init.log")
            .unwrap();
        assert_eq!(log.owners, vec!["root", "syslog"]);
    }

    fn fixture_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(dir.path().join("etc/passwd"), "root:x:0:0::/root:/bin/sh\n")
            .unwrap();
        std::fs::write(dir.path().join("etc/group"), "root:x:0:\n").unwrap();
        dir
    }

    fn one(text: &str) -> Vec<Entry> {
        parse(text).unwrap()
    }

    #[test]
    fn a_missing_required_path_is_reported() {
        let dir = fixture_root();
        let entries = one("[[path]]\n\
             path = \"/var/lib/cloud\"\n\
             kind = \"dir\"\n\
             mode = \"0755\"\n\
             owner = \"root\"\n\
             group = \"root\"\n\
             presence = \"required\"\n\
             creator = \"tmpfiles\"\n");
        let findings = verify(dir.path(), &entries);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].problem, Problem::Missing);
    }

    #[test]
    fn a_missing_optional_path_is_not() {
        let dir = fixture_root();
        let entries = one("[[path]]\n\
             path = \"/run/cloud-init/status.json\"\n\
             kind = \"file\"\n\
             mode = \"0644\"\n\
             owner = \"root\"\n\
             group = \"root\"\n\
             presence = \"optional\"\n\
             creator = \"runtime\"\n");
        assert!(verify(dir.path(), &entries).is_empty());
    }

    #[test]
    fn a_world_readable_secret_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fixture_root();
        let target = dir.path().join("run/cloud-init");
        std::fs::create_dir_all(&target).unwrap();
        let file = target.join("instance-data-sensitive.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
            .unwrap();

        let entries = one("[[path]]\n\
             path = \"/run/cloud-init/instance-data-sensitive.json\"\n\
             kind = \"file\"\n\
             mode = \"0600\"\n\
             owner = \"root\"\n\
             group = \"root\"\n\
             presence = \"optional\"\n\
             creator = \"runtime\"\n");
        let findings = verify(dir.path(), &entries);
        assert_eq!(
            findings[0].problem,
            Problem::WrongMode {
                found: 0o644,
                want: 0o600
            }
        );
    }

    #[test]
    fn a_file_where_a_directory_belongs_is_reported() {
        let dir = fixture_root();
        std::fs::create_dir_all(dir.path().join("var/lib")).unwrap();
        std::fs::write(dir.path().join("var/lib/cloud"), "").unwrap();
        let entries = one("[[path]]\n\
             path = \"/var/lib/cloud\"\n\
             kind = \"dir\"\n\
             mode = \"0755\"\n\
             owner = \"root\"\n\
             group = \"root\"\n\
             presence = \"required\"\n\
             creator = \"tmpfiles\"\n");
        let findings = verify(dir.path(), &entries);
        assert_eq!(
            findings[0].problem,
            Problem::WrongKind {
                found: "file",
                want: "dir"
            }
        );
    }

    /// A symlink where a directory belongs must be caught, not followed: that
    /// is how a shared state directory gets redirected.
    #[test]
    fn a_symlink_standing_in_for_a_directory_is_not_followed() {
        let dir = fixture_root();
        std::fs::create_dir_all(dir.path().join("var/lib")).unwrap();
        std::fs::create_dir_all(dir.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink("../../elsewhere", dir.path().join("var/lib/cloud"))
            .unwrap();
        let entries = one("[[path]]\n\
             path = \"/var/lib/cloud\"\n\
             kind = \"dir\"\n\
             mode = \"0755\"\n\
             owner = \"root\"\n\
             group = \"root\"\n\
             presence = \"required\"\n\
             creator = \"tmpfiles\"\n");
        let findings = verify(dir.path(), &entries);
        assert_eq!(
            findings[0].problem,
            Problem::WrongKind {
                found: "symlink",
                want: "dir"
            }
        );
    }

    #[test]
    fn a_children_entry_audits_each_child() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fixture_root();
        let instances = dir.path().join("var/lib/cloud/instances");
        std::fs::create_dir_all(instances.join("iid-good")).unwrap();
        std::fs::create_dir_all(instances.join("iid-bad")).unwrap();
        // Explicit, so the test does not depend on the runner's umask.
        std::fs::set_permissions(
            instances.join("iid-good"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(
            instances.join("iid-bad"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();

        // Names the fixture's passwd/group do not know, so the audit skips
        // ownership and this test is about the mode alone: the test process is
        // not root and so cannot chown the fixture.
        let entries = one("[[path]]\n\
             path = \"/var/lib/cloud/instances/*\"\n\
             kind = \"dir\"\n\
             mode = \"0755\"\n\
             owner = \"nosuchuser\"\n\
             group = \"nosuchgroup\"\n\
             presence = \"optional\"\n\
             creator = \"runtime\"\n");
        let findings = verify(dir.path(), &entries);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].path.ends_with("iid-bad"), "{:?}", findings[0]);
    }

    #[test]
    fn an_unresolvable_owner_name_is_not_treated_as_drift() {
        let dir = fixture_root();
        let target = dir.path().join("var/log");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("cloud-init.log"), "").unwrap();
        // `syslog` is not in the fixture's passwd, so the only resolvable name
        // is root, and the file is owned by whoever ran the test.
        let entries = one("[[path]]\n\
             path = \"/var/log/cloud-init.log\"\n\
             kind = \"file\"\n\
             mode = \"0644\"\n\
             owner = \"nosuchuser\"\n\
             group = \"nosuchgroup\"\n\
             presence = \"optional\"\n\
             creator = \"runtime\"\n");
        let findings = verify(dir.path(), &entries);
        assert!(
            findings.iter().all(|f| !matches!(
                f.problem,
                Problem::WrongOwner { .. } | Problem::WrongGroup { .. }
            )),
            "{findings:?}"
        );
    }
}
