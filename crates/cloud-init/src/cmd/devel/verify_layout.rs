//! `cloud-init devel verify-layout` — audit a system against the filesystem
//! contract.
//!
//! This has no upstream counterpart, which is the point. The Rust and Python
//! implementations share `/var/lib/cloud`, `/run/cloud-init` and `/etc/cloud`,
//! and `update-alternatives` lets an admin move between them at any time. A
//! mode or an owner that drifts during one of those switches outlives it, and
//! nothing in either implementation would notice. So the contract is written
//! down in `packaging/filesystem-contract.toml` and this command checks a real
//! system against it.
//!
//! It is meant to be run three ways: by hand when something looks wrong, as a
//! package test after install/switch/revert (PLAN.md §6.7), and in CI against a
//! system that was provisioned by *Python* cloud-init — which is what makes
//! "same directories, same permissions" a tested property rather than a claim.

use std::path::PathBuf;

use clap::Args as ClapArgs;

#[derive(Debug, ClapArgs)]
// Four independent command-line flags, not four pieces of state: clap owns the
// shape, and folding them into an enum would change the surface.
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    /// Audit this root instead of `/`. Contract paths are joined onto it, so a
    /// fixture tree can be checked without touching the live system.
    #[arg(long, value_name = "PATH", default_value = "/")]
    pub root: PathBuf,

    /// Print the `systemd-tmpfiles` fragment generated from the contract and
    /// exit, instead of auditing anything.
    #[arg(long)]
    pub dump_tmpfiles: bool,

    /// Print the `SELinux` file-context (`.fc`) rules generated from the
    /// contract and exit, instead of auditing anything.
    #[arg(long)]
    pub dump_selinux_fc: bool,

    /// Print the `AppArmor` profile generated from the contract and exit,
    /// instead of auditing anything.
    #[arg(long)]
    pub dump_apparmor: bool,

    /// Print every contract path that was checked, not only the problems.
    #[arg(long, short)]
    pub verbose: bool,
}

/// Exit 0 when the system matches the contract, 1 when it does not, and 2 when
/// the contract itself could not be read — three distinct outcomes, because a
/// package test that cannot tell "clean" from "could not check" is worthless.
pub fn run(args: &Args) -> u8 {
    let entries = match ci_core::layout::contract() {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };

    if args.dump_tmpfiles {
        print!("{}", ci_core::layout::tmpfiles_d(&entries));
        return 0;
    }

    if args.dump_selinux_fc {
        print!("{}", ci_core::layout::selinux_fc(&entries));
        return 0;
    }

    if args.dump_apparmor {
        print!("{}", ci_core::layout::apparmor_profile(&entries));
        return 0;
    }

    if args.verbose {
        for entry in &entries {
            println!(
                "checking {} ({}, {:04o}, {}:{})",
                entry.path,
                if entry.presence == ci_core::layout::Presence::Required {
                    "required"
                } else {
                    "optional"
                },
                entry.mode,
                entry.owners.join("|"),
                entry.groups.join("|"),
            );
        }
    }

    let findings = ci_core::layout::verify(&args.root, &entries);
    if findings.is_empty() {
        println!("{} paths checked, no drift found", entries.len());
        return 0;
    }

    for finding in &findings {
        println!("{finding}");
    }
    println!("\n{} problem(s) found", findings.len());
    1
}
