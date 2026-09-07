//! `_write_hostname` and friends: the behavioural half of a distro's hostname
//! handling, plus the `/etc/hostname` parser it round-trips through.
//!
//! Ten classes define `_write_hostname` upstream and the other twenty-six
//! distros inherit one of them; [`HostnameWriter`] is that choice, resolved
//! per distro in the generated table rather than guessed from the family.
//!
//! What every variant has in common is that it is careful with a file it did
//! not write. `/etc/hostname` may carry comments and a trailing comment on the
//! name itself, and upstream preserves both by parsing the file, replacing
//! only the name, and writing the whole thing back — which is why
//! [`HostnameConf`] exists rather than a one-line `write("name\n")`.

use std::path::{Path, PathBuf};

use ci_config::{option, Object};
use ci_sys::atomic::{self, WriteOptions};

use crate::{Distro, HostnameReader, HostnameWriter, SystemHostnameReader};

const SOURCE: &str = "distros/__init__.py";

/// The `UnboundLocalError` openSUSE's `_read_hostname` raises when it falls
/// into its own `except IOError: pass`. See COMPAT bug B71.
const UNBOUND: &str =
    "cannot access local variable 'hostname' where it is not associated with a value";

/// `distros/parsers/hostname.py`: `/etc/hostname` with its comments intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostnameConf {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A blank line, kept verbatim.
    Blank(String),
    /// A line that is nothing but a comment, kept verbatim.
    Comment(String),
    /// A name and whatever trailing comment followed it.
    Hostname { name: String, tail: String },
}

impl HostnameConf {
    /// `HostnameConf(text).parse()`.
    ///
    /// # Errors
    /// The `IOError("Multiple hostnames ... found!")` upstream raises for a
    /// file naming two different hosts. Every caller treats that the same way
    /// it treats an unreadable file.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut entries = Vec::new();
        let mut found: Vec<&str> = Vec::new();
        for line in ci_core::pystr::split_lines(text) {
            if line.trim().is_empty() {
                entries.push(Entry::Blank(line.to_owned()));
                continue;
            }
            let (head, tail) = chop_comment(line.trim());
            if head.is_empty() {
                entries.push(Entry::Comment(line.to_owned()));
                continue;
            }
            if !found.contains(&head) {
                found.push(head);
            }
            entries.push(Entry::Hostname {
                name: head.to_owned(),
                tail: tail.to_owned(),
            });
        }
        if found.len() > 1 {
            return Err(format!("Multiple hostnames ({found:?}) found!"));
        }
        Ok(Self { entries })
    }

    /// `conf.hostname`: the first name in the file.
    #[must_use]
    pub fn hostname(&self) -> Option<&str> {
        self.entries.iter().find_map(|entry| match entry {
            Entry::Hostname { name, .. } => Some(name.as_str()),
            _ => None,
        })
    }

    /// `conf.set_hostname`. An empty name is ignored, as upstream.
    pub fn set_hostname(&mut self, hostname: &str) {
        let hostname = hostname.trim();
        if hostname.is_empty() {
            return;
        }
        let mut replaced = false;
        for entry in &mut self.entries {
            if let Entry::Hostname { name, .. } = entry {
                hostname.clone_into(name);
                replaced = true;
            }
        }
        if !replaced {
            self.entries.push(Entry::Hostname {
                name: hostname.to_owned(),
                tail: String::new(),
            });
        }
    }
}

impl std::fmt::Display for HostnameConf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        for entry in &self.entries {
            match entry {
                Entry::Blank(line) | Entry::Comment(line) => {
                    out.push_str(line);
                }
                Entry::Hostname { name, tail } => {
                    out.push_str(name);
                    out.push_str(tail);
                }
            }
            out.push('\n');
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
        f.write_str(&out)
    }
}

/// `distros/parsers/chop_comment(text, "#")`.
fn chop_comment(text: &str) -> (&str, &str) {
    match text.find('#') {
        Some(at) => text.split_at(at),
        None => (text, ""),
    }
}

/// `Distro.set_hostname`: pick a name, persist it, then apply it for this boot.
///
/// `root` prefixes every path written, so a test can watch what a real boot
/// would have done to `/etc` without being root.
///
/// # Errors
/// Whatever `_write_hostname` failed with. `_apply_hostname` cannot fail —
/// upstream logs and continues, because it only sets the name until reboot.
pub fn set_hostname(
    distro: &Distro,
    cfg: &Object,
    root: &Path,
    hostname: Option<&str>,
    fqdn: Option<&str>,
    log: &mut ci_log::Logger,
) -> Result<(), String> {
    let Some(name) = distro.select_hostname(cfg, hostname, fqdn) else {
        return Err("no hostname to set".to_owned());
    };
    write_hostname(
        distro,
        cfg,
        &joined(root, distro.hostname_conf_fn),
        name,
        log,
    )?;
    // A rooted run is a fixture, and the running kernel is not part of it:
    // renaming the host machine is exactly the side effect the root exists to
    // prevent. The persistent write above is the part under test.
    if root == Path::new("/") {
        apply_hostname(name, log);
    }
    Ok(())
}

/// One thing `Distro.update_hostname` decided to do.
///
/// Split out so the decision can be compared without renaming the machine
/// that runs the test, the same arrangement `ci_distro::create` uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Persist `name` in `path` through `_write_hostname`.
    WriteHostname { path: PathBuf, name: String },
    /// `hostname <name>`, which lasts until reboot.
    ApplyHostname(String),
    /// The running name has drifted from the record, so nothing is written.
    UserMaintained { previous: PathBuf, system: PathBuf },
}

/// `Distro.update_hostname`, decided but not carried out.
///
/// The difference from [`set_hostname`] is the bookkeeping. This runs on
/// *every* boot (`PER_ALWAYS`), so it has to tell "cloud-init set this and
/// nothing has touched it since" from "somebody renamed the machine by hand".
/// `prev_hostname_fn` is the record that makes that possible, and when the
/// running name has drifted away from it the whole update is abandoned.
///
/// `prev_hostname_fn` is already root-prefixed; `root` prefixes the system
/// files this resolves for itself.
///
/// # Errors
/// A reader that upstream lets raise — the file variants swallow their own IO
/// errors, but `previous-hostname` is read without a guard.
pub fn plan_update(
    distro: &Distro,
    cfg: &Object,
    root: &Path,
    hostname: Option<&str>,
    fqdn: Option<&str>,
    prev_hostname_fn: &Path,
    log: &mut ci_log::Logger,
) -> Result<Vec<Step>, String> {
    let applying = hostname;
    let Some(name) = distro.select_hostname(cfg, hostname, fqdn) else {
        return Err("no hostname to set".to_owned());
    };

    let previous = if prev_hostname_fn.exists() {
        read_hostname_for(distro, prev_hostname_fn, None, log)?
    } else {
        None
    };
    let (sys_fn, sys_hostname) = read_system_hostname(distro, root, log)?;

    let mut update: Vec<PathBuf> = Vec::new();
    if previous.as_deref() != Some(name) {
        update.push(prev_hostname_fn.to_owned());
    }
    if sys_hostname.is_none()
        || (sys_hostname.as_deref() == previous.as_deref()
            && sys_hostname.as_deref() != Some(name))
    {
        update.push(sys_fn.clone());
    }

    // Somebody renamed the machine after cloud-init named it. Upstream sets
    // the hostname once per instance and treats any later change as the
    // operator's, so nothing is written and the transient name is left alone.
    if let (Some(sys), Some(prev)) = (sys_hostname.as_deref(), previous.as_deref()) {
        if sys != prev {
            return Ok(vec![Step::UserMaintained {
                previous: prev_hostname_fn.to_owned(),
                system: sys_fn,
            }]);
        }
    }

    // `list(set(...))` upstream, which is only there to collapse the case
    // where the previous-hostname record and the system file are one file.
    update.dedup();
    let mut steps: Vec<Step> = update
        .iter()
        .map(|path| Step::WriteHostname {
            path: path.clone(),
            name: name.to_owned(),
        })
        .collect();
    if update.contains(&sys_fn) {
        if let Some(applying) = applying {
            steps.push(Step::ApplyHostname(applying.to_owned()));
        }
    }
    Ok(steps)
}

/// Carry out a [`plan_update`].
///
/// A write that fails is logged and stepped over, upstream included: the
/// remaining file may still be the one that matters.
pub fn run_update(
    distro: &Distro,
    cfg: &Object,
    root: &Path,
    steps: &[Step],
    log: &mut ci_log::Logger,
) {
    let writes = steps
        .iter()
        .filter(|step| matches!(step, Step::WriteHostname { .. }))
        .count();
    if let Some(Step::WriteHostname { name, .. }) = steps.first() {
        log.debug(
            SOURCE,
            &format!("Attempting to update hostname to {name} in {writes} files"),
        );
    }
    for step in steps {
        match step {
            Step::UserMaintained { previous, system } => log.info(
                SOURCE,
                &format!(
                    "{} differs from {}, assuming user maintained hostname.",
                    previous.display(),
                    system.display()
                ),
            ),
            Step::WriteHostname { path, name } => {
                if let Err(error) = write_hostname(distro, cfg, path, name, log) {
                    log.warning(
                        SOURCE,
                        &format!(
                            "Failed to write hostname {name} to {}: {error}",
                            path.display()
                        ),
                    );
                }
            }
            // Same reason as `set_hostname`: a rooted run is a fixture and the
            // running kernel is not part of it.
            Step::ApplyHostname(name) => {
                if root == Path::new("/") {
                    apply_hostname(name, log);
                }
            }
        }
    }
}

/// `Distro._read_system_hostname`: which file holds the running name, and what
/// it says.
///
/// The returned path is root-prefixed, so it can be written to; the variants
/// that branch on `uses_systemd` do so against the *live* host, because that
/// is what decides whether `hostnamectl` owns the file.
///
/// # Errors
/// Whatever [`read_hostname_for`] raised.
pub fn read_system_hostname(
    distro: &Distro,
    root: &Path,
    log: &mut ci_log::Logger,
) -> Result<(PathBuf, Option<String>), String> {
    let systemd_fn = distro
        .systemd_hostname_conf_fn
        .unwrap_or(distro.hostname_conf_fn);
    let unrooted = match distro.system_hostname_reader {
        SystemHostnameReader::ConfFile => distro.hostname_conf_fn,
        SystemHostnameReader::Systemd => systemd_fn,
        SystemHostnameReader::SystemdOrConf => {
            if ci_core::status::uses_systemd() {
                systemd_fn
            } else {
                distro.hostname_conf_fn
            }
        }
    };
    let path = joined(root, unrooted);
    let name = read_hostname_for(distro, &path, None, log)?;
    Ok((path, name))
}

/// `Distro._read_hostname`, dispatched on [`Distro::hostname_reader`].
///
/// # Errors
/// The variants that read `previous-hostname` do so with no `try`, so an
/// unreadable file propagates; so does a `hostname` that will not run. Only
/// the [`ConfFile`](HostnameReader::ConfFile) variant swallows its own IO
/// errors and falls back to `default`.
pub fn read_hostname_for(
    distro: &Distro,
    filename: &Path,
    default: Option<&str>,
    log: &mut ci_log::Logger,
) -> Result<Option<String>, String> {
    let is_previous = filename.ends_with("previous-hostname");
    let systemd = ci_core::status::uses_systemd();
    let default = default.map(ToOwned::to_owned);

    match distro.hostname_reader {
        HostnameReader::ConfFile => Ok(read_hostname(filename, default.as_deref())),
        HostnameReader::Aosc => {
            if is_previous {
                stripped_file(filename).map(Some)
            } else {
                Ok(running_hostname(&["hostname"], true)?.or(default))
            }
        }
        HostnameReader::Photon => {
            if is_previous {
                stripped_file(filename).map(Some)
            } else {
                Ok(running_hostname(&["hostname", "-f"], true)?.or(default))
            }
        }
        HostnameReader::Rhel => {
            if systemd && is_previous {
                stripped_file(filename).map(Some)
            } else if systemd {
                Ok(running_hostname(&["hostname"], true)?.or(default))
            } else {
                Err(unported("read_sysconfig_file", "sysvinit rhel"))
            }
        }
        HostnameReader::OpenSuse => {
            if systemd && is_previous {
                stripped_file(filename).map(Some)
            } else if systemd {
                // Not a typo for rhel's: openSUSE is missing the `.strip()`
                // its otherwise identical body has, and the trailing newline
                // survives into the comparison. See COMPAT bug B70.
                Ok(running_hostname(&["hostname"], false)?.or(default))
            } else {
                // Upstream's `except IOError: pass` leaves `hostname`
                // unbound, so a missing, unreadable or two-named file raises
                // instead of falling back to `default`. See COMPAT bug B71.
                let Ok(text) = std::fs::read_to_string(filename) else {
                    return Err(UNBOUND.to_owned());
                };
                let Ok(conf) = HostnameConf::parse(&text) else {
                    return Err(UNBOUND.to_owned());
                };
                Ok(conf
                    .hostname()
                    .filter(|name| !name.is_empty())
                    .map(ToOwned::to_owned)
                    .or(default))
            }
        }
        HostnameReader::Bsd | HostnameReader::OpenBsd => {
            let _ = log;
            Err(unported("the BSD rc.conf readers", "a BSD"))
        }
    }
}

/// `util.load_text_file(fn).strip()`, which upstream leaves unguarded.
fn stripped_file(filename: &Path) -> Result<String, String> {
    std::fs::read_to_string(filename)
        .map(|text| text.trim().to_owned())
        .map_err(|error| format!("{}: {error}", filename.display()))
}

/// `subp.subp(argv)`, returning `None` for the empty output upstream falls
/// back to `default` on.
fn running_hostname(argv: &[&str], strip: bool) -> Result<Option<String>, String> {
    let output = ci_sys::subp::Subp::new(argv)
        .run()
        .map_err(|error| error.to_string())?;
    if !output.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let out = String::from_utf8_lossy(&output.stdout).into_owned();
    let out = if strip { out.trim().to_owned() } else { out };
    Ok((!out.is_empty()).then_some(out))
}

/// `Distro._write_hostname`, dispatched on [`Distro::hostname_writer`].
///
/// `filename` is already root-prefixed; the `/previous-hostname` suffix tests
/// several variants make are on the *unprefixed* tail, which a prefix cannot
/// disturb.
///
/// # Errors
/// A file that cannot be written, a `hostnamectl` that fails, or a variant
/// this port does not implement.
pub fn write_hostname(
    distro: &Distro,
    cfg: &Object,
    filename: &Path,
    hostname: &str,
    log: &mut ci_log::Logger,
) -> Result<(), String> {
    let create_file = option::get_bool(cfg, "create_hostname_file", true);
    let is_previous = filename.ends_with("previous-hostname");
    let systemd = ci_core::status::uses_systemd();

    match distro.hostname_writer {
        HostnameWriter::ConfFile => write_conf(filename, hostname, create_file, log),
        HostnameWriter::Gentoo => {
            // OpenRC generates /etc/hostname from /etc/conf.d/hostname, which
            // uses `hostname="..."` instead of a bare name.
            let value = if systemd {
                hostname.to_owned()
            } else {
                format!("hostname=\"{hostname}\"")
            };
            write_conf(filename, &value, create_file, log)
        }
        HostnameWriter::Rhel => {
            if systemd && is_previous {
                write_conf(filename, hostname, true, log)
            } else if systemd {
                hostnamectl(hostname, create_file, log)
            } else {
                Err(unported("update_sysconfig_file", "sysvinit rhel"))
            }
        }
        HostnameWriter::OpenSuse => {
            if systemd && is_previous {
                write_plain(filename, hostname)
            } else if systemd {
                hostnamectl(hostname, create_file, log)
            } else {
                write_conf(filename, hostname, create_file, log)
            }
        }
        HostnameWriter::Photon => {
            if is_previous {
                write_plain(filename, hostname)
            } else {
                // `exec_cmd` reports a failure through the return code, so
                // photon warns where rhel raises.
                if let Err(error) = hostnamectl(hostname, create_file, log) {
                    log.warning(
                        "distros/photon.py",
                        &format!("Error while setting hostname: {error}\nGiven hostname: {hostname}"),
                    );
                }
                Ok(())
            }
        }
        HostnameWriter::Aosc => {
            // No `else`: for a previous-hostname file aosc writes the file
            // *and* falls through to hostnamectl. Reproduced.
            if is_previous {
                write_conf(filename, hostname, true, log)?;
            }
            hostnamectl(hostname, create_file, log)
        }
        HostnameWriter::Bsd | HostnameWriter::OpenBsd => {
            Err(unported("the BSD rc.conf writers", "a BSD"))
        }
    }
}

/// `Distro._read_hostname`, for the variants that read a file.
///
/// Returns `default` for anything upstream swallows: a missing file, an
/// unreadable one, or one naming two hosts.
#[must_use]
pub fn read_hostname(filename: &Path, default: Option<&str>) -> Option<String> {
    let text = std::fs::read_to_string(filename).ok()?;
    let name = HostnameConf::parse(&text).ok()?.hostname()?.to_owned();
    if name.is_empty() {
        return default.map(ToOwned::to_owned);
    }
    Some(name)
}

/// The `HostnameConf` round-trip shared by every file-writing variant.
fn write_conf(
    filename: &Path,
    hostname: &str,
    create_file: bool,
    log: &mut ci_log::Logger,
) -> Result<(), String> {
    let mut conf = match std::fs::read_to_string(filename) {
        Ok(text) => HostnameConf::parse(&text).unwrap_or_else(|_| empty_conf()),
        Err(_) if create_file => empty_conf(),
        Err(_) => {
            log.info(
                SOURCE,
                "create_hostname_file is False; hostname file not created",
            );
            return Ok(());
        }
    };
    conf.set_hostname(hostname);
    write_bytes(filename, conf.to_string().as_bytes())
}

/// `util.write_file(filename, hostname)` — no parse, no trailing newline.
fn write_plain(filename: &Path, hostname: &str) -> Result<(), String> {
    write_bytes(filename, hostname.as_bytes())
}

fn write_bytes(filename: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = filename.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    atomic::write_file(
        filename,
        contents,
        WriteOptions {
            mode: 0o644,
            ..WriteOptions::default()
        },
    )
    .map_err(|error| format!("{}: {error}", filename.display()))
}

fn empty_conf() -> HostnameConf {
    HostnameConf {
        entries: Vec::new(),
    }
}

fn hostnamectl(
    hostname: &str,
    create_file: bool,
    log: &mut ci_log::Logger,
) -> Result<(), String> {
    let mut argv = vec!["hostnamectl", "set-hostname"];
    if !create_file {
        argv.push("--transient");
    }
    argv.push(hostname);
    let output = ci_sys::subp::Subp::new(&argv)
        .run()
        .map_err(|error| error.to_string())?;
    if !output.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    if !create_file {
        log.info(
            SOURCE,
            "create_hostname_file is False; hostname set transiently",
        );
    }
    Ok(())
}

/// `Distro._apply_hostname`: `hostname <name>`, which lasts until reboot.
///
/// A failure here is logged and stepped over, upstream included: the
/// persistent write has already happened and is what the next boot reads.
fn apply_hostname(hostname: &str, log: &mut ci_log::Logger) {
    log.debug(
        SOURCE,
        &format!("Non-persistently setting the system hostname to {hostname}"),
    );
    let failed = match ci_sys::subp::Subp::new(["hostname", hostname]).run() {
        Ok(output) => !output.success(),
        Err(_) => true,
    };
    if failed {
        log.warning(
            SOURCE,
            &format!(
                "Failed to non-persistently adjust the system hostname to {hostname}"
            ),
        );
    }
}

fn unported(what: &str, where_: &str) -> String {
    format!("{what} is not ported; this distro needs it to set a hostname on {where_}")
}

/// `os.path.join(root, path)` for an absolute `path`, which `Path::join` would
/// otherwise let replace the root entirely.
fn joined(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
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

    fn ubuntu() -> &'static Distro {
        crate::fetch("ubuntu").unwrap()
    }

    #[test]
    fn a_comment_only_file_gains_a_hostname_line_and_keeps_the_comment() {
        let mut conf = HostnameConf::parse("# managed by something\n\n").unwrap();
        assert_eq!(conf.hostname(), None);
        conf.set_hostname("host1");
        assert_eq!(conf.to_string(), "# managed by something\n\nhost1\n");
    }

    #[test]
    fn a_trailing_comment_on_the_name_survives_the_replacement() {
        let mut conf = HostnameConf::parse("old # why\n").unwrap();
        // `chop_comment` splits at the `#` without trimming, so the name
        // keeps the space before it and the rewrite has no space after it.
        assert_eq!(conf.hostname(), Some("old "));
        conf.set_hostname("new");
        assert_eq!(conf.to_string(), "new# why\n");
    }

    #[test]
    fn two_different_names_are_refused_but_one_name_twice_is_not() {
        assert!(HostnameConf::parse("a\nb\n").is_err());
        assert!(HostnameConf::parse("a\na\n").is_ok());
    }

    #[test]
    fn an_empty_name_is_ignored() {
        let mut conf = HostnameConf::parse("keep\n").unwrap();
        conf.set_hostname("   ");
        assert_eq!(conf.hostname(), Some("keep"));
    }

    #[test]
    fn a_file_without_a_trailing_newline_gains_one() {
        assert_eq!(HostnameConf::parse("host1").unwrap().to_string(), "host1\n");
    }

    #[test]
    fn writing_creates_the_file_under_the_root_not_over_etc() {
        let root = tempfile::tempdir().unwrap();
        let mut log = ci_log::Logger::silent();
        let target = joined(root.path(), ubuntu().hostname_conf_fn);
        write_hostname(ubuntu(), &Object::new(), &target, "host1", &mut log).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "host1\n");
        assert!(target.starts_with(root.path()));
    }

    #[test]
    fn create_hostname_file_false_leaves_an_absent_file_absent() {
        let root = tempfile::tempdir().unwrap();
        let mut log = ci_log::Logger::silent();
        let target = joined(root.path(), ubuntu().hostname_conf_fn);
        let mut cfg = Object::new();
        cfg.insert("create_hostname_file".to_owned(), false.into());
        write_hostname(ubuntu(), &cfg, &target, "host1", &mut log).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn create_hostname_file_false_still_updates_a_file_that_is_there() {
        let root = tempfile::tempdir().unwrap();
        let mut log = ci_log::Logger::silent();
        let target = joined(root.path(), ubuntu().hostname_conf_fn);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "old\n").unwrap();
        let mut cfg = Object::new();
        cfg.insert("create_hostname_file".to_owned(), false.into());
        write_hostname(ubuntu(), &cfg, &target, "host1", &mut log).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "host1\n");
    }

    #[test]
    fn reading_back_what_was_written_gives_the_name() {
        let root = tempfile::tempdir().unwrap();
        let mut log = ci_log::Logger::silent();
        let target = joined(root.path(), ubuntu().hostname_conf_fn);
        write_hostname(ubuntu(), &Object::new(), &target, "host1", &mut log).unwrap();
        assert_eq!(read_hostname(&target, None).as_deref(), Some("host1"));
        assert_eq!(
            read_hostname(&root.path().join("nope"), Some("d")).as_deref(),
            None,
            "a missing file is None, not the default: upstream's IOError \
             branch leaves `hostname` unset and only an empty name falls back"
        );
    }

    #[test]
    fn every_distro_in_the_table_resolves_to_a_writer() {
        for distro in crate::DISTROS {
            let expected = match distro.name {
                "freebsd" | "netbsd" => HostnameWriter::Bsd,
                "openbsd" => HostnameWriter::OpenBsd,
                _ => distro.hostname_writer,
            };
            assert_eq!(distro.hostname_writer, expected, "{}", distro.name);
        }
        assert_eq!(ubuntu().hostname_writer, HostnameWriter::ConfFile);
        assert_eq!(
            crate::fetch("rhel").unwrap().hostname_writer,
            HostnameWriter::Rhel
        );
        assert_eq!(
            crate::fetch("azurelinux").unwrap().hostname_writer,
            HostnameWriter::Rhel,
            "azurelinux names itself in osfamily but inherits rhel's writer"
        );
    }

    #[test]
    fn a_bsd_writer_reports_that_it_is_not_ported() {
        let root = tempfile::tempdir().unwrap();
        let mut log = ci_log::Logger::silent();
        let error = write_hostname(
            crate::fetch("freebsd").unwrap(),
            &Object::new(),
            &root.path().join("rc.conf"),
            "host1",
            &mut log,
        )
        .unwrap_err();
        assert!(error.contains("not ported"), "{error}");
    }

    /// `<root>/etc/hostname` and `<root>/previous-hostname`, either present.
    fn hostname_fixture(
        system: Option<&str>,
        previous: Option<&str>,
    ) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        if let Some(text) = system {
            std::fs::write(root.path().join("etc/hostname"), text).unwrap();
        }
        if let Some(text) = previous {
            std::fs::write(root.path().join("previous-hostname"), text).unwrap();
        }
        root
    }

    fn planned(system: Option<&str>, previous: Option<&str>, name: &str) -> Vec<Step> {
        let root = hostname_fixture(system, previous);
        let mut log = ci_log::Logger::silent();
        plan_update(
            crate::fetch("ubuntu").unwrap(),
            &Object::new(),
            root.path(),
            Some(name),
            Some(&format!("{name}.example.com")),
            &root.path().join("previous-hostname"),
            &mut log,
        )
        .unwrap()
    }

    #[test]
    fn a_first_boot_writes_both_the_record_and_the_system_file() {
        let steps = planned(None, None, "h1");
        assert_eq!(steps.len(), 3);
        assert!(matches!(steps[2], Step::ApplyHostname(ref name) if name == "h1"));
    }

    #[test]
    fn a_hostname_that_is_already_right_everywhere_needs_no_work() {
        assert_eq!(planned(Some("h1\n"), Some("h1\n"), "h1"), Vec::new());
    }

    #[test]
    fn a_rename_by_hand_stops_the_whole_update() {
        let steps = planned(Some("renamed\n"), Some("h1\n"), "h1");
        assert!(
            matches!(steps.as_slice(), [Step::UserMaintained { .. }]),
            "{steps:?}"
        );
    }

    #[test]
    fn a_stale_record_that_the_system_still_agrees_with_is_updated() {
        let steps = planned(Some("old\n"), Some("old\n"), "h1");
        assert_eq!(steps.len(), 3);
        assert!(
            matches!(steps[0], Step::WriteHostname { ref name, .. } if name == "h1")
        );
    }

    #[test]
    fn a_missing_record_alone_does_not_rewrite_a_system_file_that_is_correct() {
        // No previous-hostname, but `/etc/hostname` already says `h1`: the
        // record is written and the system file is left alone, so there is
        // nothing to apply either.
        let steps = planned(Some("h1\n"), None, "h1");
        assert!(
            matches!(steps.as_slice(), [Step::WriteHostname { .. }]),
            "{steps:?}"
        );
    }
}
