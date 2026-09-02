//! Port of `cloudinit.sources.canonical_cloud_id`.

pub const METADATA_UNKNOWN: &str = "unknown";

/// `(region prefix, cloud id, required cloud name)`.
///
/// Order matters: the first matching prefix wins, as in the upstream dict.
const REGION_PREFIX_MAP: [(&str, &str, &str); 3] = [
    ("cn-", "aws-china", "aws"),
    ("us-gov-", "aws-gov", "aws"),
    ("china", "azure-china", "azure"),
];

/// Canonical cloud identifier for a `(cloud_name, region, platform)` triple.
pub fn canonical_cloud_id(cloud_name: &str, region: &str, platform: &str) -> String {
    let cloud_name = if cloud_name.is_empty() {
        METADATA_UNKNOWN
    } else {
        cloud_name
    };
    let region = if region.is_empty() {
        METADATA_UNKNOWN
    } else {
        region
    };

    if region == METADATA_UNKNOWN {
        return if cloud_name == METADATA_UNKNOWN {
            platform.to_owned()
        } else {
            cloud_name.to_owned()
        };
    }

    for (prefix, cloud_id, valid_cloud) in REGION_PREFIX_MAP {
        if region.starts_with(prefix) && cloud_name == valid_cloud {
            return cloud_id.to_owned();
        }
    }

    if cloud_name == METADATA_UNKNOWN {
        platform.to_owned()
    } else {
        cloud_name.to_owned()
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

    #[test]
    fn partitions_map_to_dedicated_ids() {
        assert_eq!(canonical_cloud_id("aws", "cn-north-1", "ec2"), "aws-china");
        assert_eq!(canonical_cloud_id("aws", "us-gov-west-1", "ec2"), "aws-gov");
        assert_eq!(
            canonical_cloud_id("azure", "chinaeast", "azure"),
            "azure-china"
        );
    }

    #[test]
    fn prefix_only_applies_to_the_matching_cloud() {
        assert_eq!(canonical_cloud_id("azure", "cn-north-1", "azure"), "azure");
    }

    #[test]
    fn unknown_region_falls_back_to_cloud_then_platform() {
        assert_eq!(canonical_cloud_id("aws", METADATA_UNKNOWN, "ec2"), "aws");
        assert_eq!(
            canonical_cloud_id(METADATA_UNKNOWN, METADATA_UNKNOWN, "lxd"),
            "lxd"
        );
    }

    #[test]
    fn unknown_cloud_with_known_region_uses_platform() {
        assert_eq!(
            canonical_cloud_id(METADATA_UNKNOWN, "us-east-1", "ec2"),
            "ec2"
        );
    }
}
