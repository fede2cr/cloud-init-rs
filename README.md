# cloud-init-rs

Canonical's cloud-init port to rust

## Description

Enhancement tagged bug canonical/cloud-init#4626 asks for a rust port of cloud-init.

As well as the security and speed capabilites for which Rust is known for, it is also common to see broken servers where somebody modifies the "system" python3 installation and this damages how cloud-init works, which makes the system almost unusable for cloud environments such as Azure.

Ubuntu is doing a transcition to Coreutils in rust, and also to Sudo in rust, and after working on an Azure agent port in Rust, it makes sense to start working on a cloud-init port to rust.

This project will try to become a drop-in replacement for the current Python cloud-init from Canonical.
