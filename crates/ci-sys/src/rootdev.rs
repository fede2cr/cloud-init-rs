//! Port of `util.rootdev_from_cmdline`: the `root=` token as a device path.
//!
//! Its two callers, `cc_growpart` and `cc_resizefs`, each drive the machine
//! through a trait so that a differential can script it, so the two probes
//! this needs arrive through [`DevProbe`] rather than off the real filesystem.
//!
//! The value comes from the kernel command line, which on a cloud instance is
//! attacker-influenced on some platforms; it is only ever used to name a
//! device, never interpolated into a shell.

/// The two machine questions `rootdev_from_cmdline` asks.
pub trait DevProbe {
    /// `os.path.exists(path)`.
    fn exists(&mut self, path: &str) -> bool;

    /// `util.find_devs_with(criteria)`.
    fn find_devs_with(&mut self, criteria: &str) -> Vec<String>;
}

/// `util.rootdev_from_cmdline`.
pub fn rootdev_from_cmdline(probe: &mut dyn DevProbe, cmdline: &str) -> Option<String> {
    let found = cmdline
        .split_whitespace()
        .find_map(|token| token.strip_prefix("root="))?;

    if found.starts_with("/dev/") {
        return Some(found.to_owned());
    }
    if let Some(label) = found.strip_prefix("LABEL=") {
        return Some(format!("/dev/disk/by-label/{label}"));
    }
    if let Some(uuid) = found.strip_prefix("UUID=") {
        return Some(format!("/dev/disk/by-uuid/{}", uuid.to_lowercase()));
    }
    if let Some(partuuid) = found.strip_prefix("PARTUUID=") {
        let path = format!("/dev/disk/by-partuuid/{}", partuuid.to_lowercase());
        if probe.exists(&path) {
            return Some(path);
        }
        if let Some(first) = probe.find_devs_with(found).first() {
            return Some(first.clone());
        }
        // Deliberately the path that does not exist: upstream returns the
        // name the device would have had.
        return Some(path);
    }
    Some(format!("/dev/{found}"))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Probe {
        exists: Vec<String>,
        devs: Vec<String>,
    }

    impl DevProbe for Probe {
        fn exists(&mut self, path: &str) -> bool {
            self.exists.iter().any(|have| have == path)
        }

        fn find_devs_with(&mut self, _criteria: &str) -> Vec<String> {
            self.devs.clone()
        }
    }

    fn parse(cmdline: &str) -> Option<String> {
        rootdev_from_cmdline(&mut Probe::default(), cmdline)
    }

    #[test]
    fn a_command_line_without_root_names_nothing() {
        assert_eq!(parse("ro quiet console=ttyS0"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn the_three_by_id_forms_become_their_dev_disk_links() {
        assert_eq!(parse("root=/dev/sda1 ro").unwrap(), "/dev/sda1");
        assert_eq!(parse("root=sda1").unwrap(), "/dev/sda1");
        assert_eq!(
            parse("root=LABEL=cloudimg").unwrap(),
            "/dev/disk/by-label/cloudimg"
        );
        // Only UUID and PARTUUID are lowercased; a label is taken as written.
        assert_eq!(parse("root=UUID=AB-CD").unwrap(), "/dev/disk/by-uuid/ab-cd");
    }

    #[test]
    fn a_partuuid_falls_back_to_blkid_and_then_to_the_missing_link() {
        let link = "/dev/disk/by-partuuid/12-34";
        let mut probe = Probe {
            exists: vec![link.to_owned()],
            devs: vec!["/dev/sdb1".to_owned()],
        };
        assert_eq!(
            rootdev_from_cmdline(&mut probe, "root=PARTUUID=12-34").unwrap(),
            link
        );

        let mut probe = Probe {
            exists: Vec::new(),
            devs: vec!["/dev/sdb1".to_owned()],
        };
        assert_eq!(
            rootdev_from_cmdline(&mut probe, "root=PARTUUID=12-34").unwrap(),
            "/dev/sdb1"
        );

        assert_eq!(
            rootdev_from_cmdline(&mut Probe::default(), "root=PARTUUID=12-34").unwrap(),
            link
        );
    }
}
