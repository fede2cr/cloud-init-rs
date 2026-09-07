//! `cloudinit.net.udev`: the persistent-interface-naming rules.
//!
//! One function that matters and three one-line helpers around it. The
//! helpers are folded in here, because their only content upstream is an
//! `assert` on the caller's capitalisation, which the constants below satisfy
//! by construction.

/// `udev.generate_udev_rule`: pin `interface` to the card with `mac`.
///
/// An absent or empty driver becomes udev's `?*`, i.e. "any driver, but there
/// must be one" — which is how the rule avoids matching virtual devices.
#[must_use]
pub fn generate_udev_rule(interface: &str, mac: &str, driver: Option<&str>) -> String {
    let driver = match driver {
        Some(driver) if !driver.is_empty() => driver,
        _ => "?*",
    };
    format!(
        "SUBSYSTEM==\"net\", ACTION==\"add\", DRIVERS==\"{driver}\", \
         ATTR{{address}}==\"{mac}\", NAME=\"{interface}\"\n"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_rule_names_one_card_by_its_mac() {
        assert_eq!(
            generate_udev_rule("eth0", "ff:ee:dd:cc:bb:aa", Some("virtio_net")),
            "SUBSYSTEM==\"net\", ACTION==\"add\", DRIVERS==\"virtio_net\", \
             ATTR{address}==\"ff:ee:dd:cc:bb:aa\", NAME=\"eth0\"\n"
        );
    }

    #[test]
    fn no_driver_matches_any_driver() {
        let wildcard = generate_udev_rule("eth0", "ff:ee:dd:cc:bb:aa", None);
        assert!(wildcard.contains("DRIVERS==\"?*\""));
        // Upstream's `if not driver` catches the empty string too, which is
        // what an interface with a `driver` key and no value produces.
        assert_eq!(
            generate_udev_rule("eth0", "ff:ee:dd:cc:bb:aa", Some("")),
            wildcard
        );
    }
}
