//! Datasource discovery: `cloudinit/sources/*`.
//!
//! Upstream's `DataSource` is one object playing two roles — it probes for its
//! cloud and then holds whatever it found. Those are split here: a [`Probe`]
//! decides whether its cloud is present and crawls it, and the result is a
//! plain [`Datasource`] value carrying the base class's derived properties. The
//! observable behaviour is the same, and it keeps the crawl from being able to
//! mutate a datasource that has already been handed out.

pub mod azure;
pub mod cache;
pub mod configdrive;
pub mod dmi;
pub mod ec2;
pub mod event;
pub mod gce;
pub mod helpers;
pub mod instance_data;
pub mod lxd;
pub mod nocloud;
pub mod none;
pub mod openstack;
pub mod search;
pub mod types;

pub use search::{
    find_source, list_sources, parse_cmdline_or_dmi, probe_for_class, NotFound,
};
pub use types::{
    normalize_pubkey_data, Context, Datasource, Dep, DsMode, Probe, METADATA_UNKNOWN,
};
