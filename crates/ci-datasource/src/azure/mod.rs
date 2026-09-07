//! Port of `sources/azure/`: the pieces the Azure datasource is assembled from.

pub mod certs;
pub mod crawl;
pub mod ds;
pub mod errors;
pub mod host;
pub mod identity;
pub mod imds;
pub mod kvp;
pub mod netcfg;
pub mod ovf;
pub mod probe;
pub mod wire;
