//! Network configuration: the model, the two config formats, and rendering.
//!
//! Upstream's `cloudinit.net` is a large package that both *describes* network
//! configuration and *applies* it. This crate is mostly the description half —
//! parsing v1 and v2 configs into a common state and rendering that state back
//! out.
//!
//! A few modules are the exception, and each says so in its own header:
//! [`renderers`] and [`activators`] probe the machine to answer *which*
//! renderer and activator a boot would use, [`sysfs`] and [`netinfo`] read what
//! interfaces and addresses exist, [`netops`] and [`ephemeral`] change them for
//! as long as it takes to fetch metadata, and [`activators`] makes a rendered
//! config take effect on the running kernel.

pub mod activators;
pub mod cmdline;
pub mod dhcp;
pub mod ephemeral;
pub mod ip;
pub mod netinfo;
pub mod netops;
pub mod netplan;
pub mod renderer;
pub mod renderers;
pub mod state;
pub mod sysfs;
pub mod udev;
