# cloud-init-rs

Canonical's cloud-init port to rust

## Description

Enhancement tagged bug canonical/cloud-init#4626 asks for a rust port of cloud-init.

As well as the security and speed capabilites for which Rust is known for, it is also common to see broken servers where somebody modifies the "system" python3 installation and this damages how cloud-init works, which makes the system almost unusable for cloud environments such as Azure.

Ubuntu is doing a transcition to Coreutils in rust, and also to Sudo in rust, and after working on an Azure agent port in Rust, it makes sense to start working on a cloud-init port to rust.

This project will try to become a drop-in replacement for the current Python cloud-init from Canonical.

## Status

Port status:

|Phase|Done|Phase|Done|
|-----|----|-----|----|
|0 — Foundations|89%|5 — Config modules|83%|
|1 — Config/CLI|86%|6 — Packaging|53%|
|2 — Stage engine|80%|7 — Security|39%|
|3 — Datasources|76%|8 — Distros|20%|
|4 — Networking	62%|9 — Production|6%|


Compatibility target: upstream cloud-init **26.1**.


**Rough equivalency to the Python implementation: ~60%.** The read-only CLI and the
boot path are at parity; 35 of upstream's 60 config modules and 8 datasources are
ported. That number counts ported units of work, not guaranteed behaviour — what is
ported is pinned byte-for-byte against Python by the differential suite (4259 cases,
0 failures).

Working today: every read-only command (`--version`, `features`, `status`, `query`,
`schema`, `clean`, `analyze`, `collect-logs`, `devel {render,make-mime}`), the
`init`/`modules`/`single`/`--all-stages` boot stages with their user-data pipeline
and module selection, 35 of upstream's 60 config modules — including the ones that
create the login (`users_groups`, `ssh`, `set_passwords`, `set_hostname`), manage
packages (`package_update_upgrade_install`, `apt_configure`) and grow the disk
(`growpart`, `resizefs`, `disk_setup`, `mounts`) — the `NoCloud`,
`ConfigDrive`, `OpenStack`, `GCE`, `Ec2`, `LXD` and `Azure` datasources, network
config v1/v2 parsing, netplan rendering and all five network activators, and the
`cloud-id`, `cloud-init-per`, `ds-identify` and
`cloud-init-generator` binaries.
Each is verified byte-for-byte against the packaged Python implementation.

There is also one command with no upstream counterpart, `devel verify-layout`. The
Rust and Python implementations share `/etc/cloud`, `/var/lib/cloud` and
`/run/cloud-init`, and switching between them does not migrate that state, so the
shared layout is written down as data in `packaging/filesystem-contract.toml` and
this command audits a real system against it — exit 0 for no drift, 1 for drift.
Three files are generated from that same contract and committed beside it, each
pinned by a unit test: the `systemd-tmpfiles` fragment, the SELinux file
contexts, and an AppArmor profile for the shipped binaries. `--dump-tmpfiles`,
`--dump-selinux-fc` and `--dump-apparmor` regenerate them.

Not yet implemented: the remaining 25 config modules (they are recorded no-ops),
the seven non-netplan network renderers, `devel hotplug-hook`, and Azure's
pre-provisioning poll loop. Non-Debian distros are table entries only.

## Building

Requires a stable Rust toolchain (pinned by `rust-toolchain.toml`; no nightly features
are used anywhere in the tree) and OpenSSL's development headers — the one C
dependency in the tree.

```sh
sudo apt install libssl-dev pkg-config
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

## Differential testing

Correctness is defined by agreement with the Python implementation, not by our own
expectations. If the packaged `cloud-init` is installed, the harness runs every ported
read-only command under both implementations and compares stdout and exit status.
The `ds-identify` cases compare against the packaged shell script instead, over
fixture roots handed to both sides through `PATH_ROOT`; the
`cloud-init-generator` cases likewise, in an unprivileged mount namespace so that
the fixture sits where the generator's hardcoded paths already point:

```sh
cargo build --workspace --bins --examples
sh tests/differential/run.sh target/debug
```

Note the `--bins --examples`: a plain `cargo build --workspace` leaves the example
binaries unbuilt and the harness then *silently* skips the cases that need them.
It exits 77 (skip) when Python cloud-init is absent. The same harness runs in CI.
