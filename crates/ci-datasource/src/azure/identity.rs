//! Port of `sources/azure/identity.py`. Azure identifies itself through DMI:
//! a fixed chassis asset tag says the platform is Azure, and the system UUID
//! becomes the VM ID that IMDS reports back as `compute.vmId`.

use ci_core::uuid::Uuid;

/// `ChassisAssetTag.AZURE_CLOUD`.
pub const AZURE_CLOUD_TAG: &str = "7783-7084-3265-9085-8269-3286-77";

/// `byte_swap_system_uuid`.
///
/// Azure writes the first three UUID fields little-endian. SMBIOS 2.6 made that
/// mandatory, but gen1 VMs present SMBIOS 2.3, where Linux and dmidecode follow
/// RFC 4122 and read them big-endian; the swap undoes that disagreement.
#[must_use]
pub fn byte_swap_system_uuid(
    system_uuid: &str,
    log: &mut ci_log::Logger,
) -> Option<String> {
    let Some(parsed) = Uuid::parse(system_uuid) else {
        log.error(
            "identity.py",
            &format!("Failed to parse system uuid: '{system_uuid}'"),
        );
        return None;
    };
    Some(parsed.byte_swapped().to_string())
}

/// `is_vm_gen1`: gen2 guests boot UEFI, gen1 legacy BIOS.
#[must_use]
pub fn is_vm_gen1() -> bool {
    is_vm_gen1_at(std::path::Path::new("/"))
}

/// The root is a parameter so the firmware probe can be tested and driven by
/// the differential without a matching host.
#[must_use]
pub fn is_vm_gen1_at(root: &std::path::Path) -> bool {
    !root.join("sys/firmware/efi").exists() && !root.join("dev/efi").exists()
}

/// `convert_system_uuid_to_vm_id`.
#[must_use]
pub fn convert_system_uuid_to_vm_id(
    system_uuid: &str,
    gen1: bool,
    log: &mut ci_log::Logger,
) -> Option<String> {
    if gen1 {
        return byte_swap_system_uuid(system_uuid, log);
    }
    Some(system_uuid.to_owned())
}

/// `query_system_uuid`. Kernels older than 4.15 report it upper-case.
#[must_use]
pub fn query_system_uuid(log: &mut ci_log::Logger) -> Option<String> {
    let Some(system_uuid) = crate::dmi::read_dmi_data("system-uuid", log) else {
        log.error("identity.py", "failed to read system-uuid");
        return None;
    };
    let system_uuid = system_uuid.to_lowercase();
    log.debug("identity.py", &format!("Read product uuid: {system_uuid}"));
    Some(system_uuid)
}

/// `query_vm_id`.
#[must_use]
pub fn query_vm_id(log: &mut ci_log::Logger) -> Option<String> {
    let system_uuid = query_system_uuid(log)?;
    convert_system_uuid_to_vm_id(&system_uuid, is_vm_gen1(), log)
}

/// `ChassisAssetTag.query_system`: `Some` only for the one tag Azure sets.
#[must_use]
pub fn query_chassis_asset_tag(log: &mut ci_log::Logger) -> Option<&'static str> {
    let asset_tag = crate::dmi::read_dmi_data("chassis-asset-tag", log);
    classify_chassis_asset_tag(asset_tag.as_deref(), log)
}

/// `ChassisAssetTag(asset_tag)`, split out so the tag can come from somewhere
/// other than this host's DMI.
#[must_use]
pub fn classify_chassis_asset_tag(
    asset_tag: Option<&str>,
    log: &mut ci_log::Logger,
) -> Option<&'static str> {
    if asset_tag == Some(AZURE_CLOUD_TAG) {
        log.debug(
            "identity.py",
            &format!("Azure chassis asset tag: '{AZURE_CLOUD_TAG}' (AZURE_CLOUD)"),
        );
        return Some(AZURE_CLOUD_TAG);
    }
    log.debug(
        "identity.py",
        &format!("Non-Azure chassis asset tag: {}", repr(asset_tag)),
    );
    None
}

/// `%r` of an optional string, which is how both diagnostic lines render it.
fn repr(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_owned(), |tag| format!("'{tag}'"))
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
        byte_swap_system_uuid, classify_chassis_asset_tag,
        convert_system_uuid_to_vm_id, is_vm_gen1_at, repr, AZURE_CLOUD_TAG,
    };

    #[test]
    fn only_the_exact_azure_tag_is_recognised() {
        let mut log = ci_log::Logger::silent();
        assert_eq!(
            classify_chassis_asset_tag(Some(AZURE_CLOUD_TAG), &mut log),
            Some(AZURE_CLOUD_TAG)
        );
        assert!(classify_chassis_asset_tag(None, &mut log).is_none());
        assert!(classify_chassis_asset_tag(Some(""), &mut log).is_none());
        assert!(classify_chassis_asset_tag(
            Some("7783-7084-3265-9085-8269-3286-7"),
            &mut log
        )
        .is_none());
    }

    #[test]
    fn a_gen1_uuid_is_byte_swapped_and_a_gen2_one_is_not() {
        let mut log = ci_log::Logger::silent();
        let uuid = "12345678-1234-5678-1234-567812345678";
        assert_eq!(
            convert_system_uuid_to_vm_id(uuid, true, &mut log).unwrap(),
            "78563412-3412-7856-1234-567812345678"
        );
        assert_eq!(
            convert_system_uuid_to_vm_id(uuid, false, &mut log).unwrap(),
            uuid
        );
    }

    #[test]
    fn an_unparseable_system_uuid_yields_nothing() {
        let mut log = ci_log::Logger::silent();
        assert!(byte_swap_system_uuid("not-a-uuid", &mut log).is_none());
    }

    #[test]
    fn either_firmware_marker_makes_the_vm_gen2() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(is_vm_gen1_at(root));

        std::fs::create_dir_all(root.join("sys/firmware/efi")).unwrap();
        assert!(!is_vm_gen1_at(root));

        std::fs::remove_dir(root.join("sys/firmware/efi")).unwrap();
        std::fs::create_dir_all(root.join("dev")).unwrap();
        std::fs::write(root.join("dev/efi"), b"").unwrap();
        assert!(!is_vm_gen1_at(root));
    }

    #[test]
    fn an_absent_asset_tag_reprs_as_python_none() {
        assert_eq!(repr(None), "None");
        assert_eq!(repr(Some("x")), "'x'");
    }
}
