# cloud-init-rs

Canonical's cloud-init port to rust

## Description

Enhancement tagged bug canonical/cloud-init#4626 asks for a rust port of cloud-init.

As well as the security and speed capabilites for which Rust is known for, it is also common to see broken servers where somebody modifies the "system" python3 installation and this damages how cloud-init works, which makes the system almost unusable for cloud environments such as Azure.

Ubuntu is doing a transcition to Coreutils in rust, and also to Sudo in rust, and after working on an Azure agent port in Rust, it makes sense to start working on a cloud-init port to rust.

This project will try to become a drop-in replacement for the current Python cloud-init from Canonical.

## Status

Compatibility target: upstream cloud-init **26.1**. Phase 0 is complete and Phase 1 is
under way — see [PLAN.md](PLAN.md) for the full roadmap.

Working today: `cloud-init --version`, `features`, `status`, `query`, `devel render`,
plus the `cloud-id` and `cloud-init-per` binaries. Each is verified byte-for-byte
against the packaged Python implementation.

Not yet implemented: `schema`, `clean`, `analyze`, `collect-logs`,
`devel {make-mime,net-convert,hotplug-hook}`, and the boot stages themselves.

## Building

Requires a stable Rust toolchain (pinned by `rust-toolchain.toml`; no nightly features
are used anywhere in the tree).

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

## Differential testing

Correctness is defined by agreement with the Python implementation, not by our own
expectations. If the packaged `cloud-init` is installed, the harness runs every ported
read-only command under both implementations and compares stdout and exit status:

```sh
cargo build --workspace
sh tests/differential/run.sh target/debug
```

It exits 77 (skip) when Python cloud-init is absent. The same harness runs in CI.
