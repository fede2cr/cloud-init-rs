//! `cloud-init` command line entry point.
//!
//! Phase 1 implements the read-only subset of the CLI: everything that inspects
//! already-collected state without mutating the system. Boot stages arrive in
//! Phase 2.

// A binary crate exports no API, so `unreachable_pub` has nothing to protect here;
// `pub` is used only so clap's derives can name the types across modules.
#![allow(unreachable_pub)]

mod cmd;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cmd::Cli::parse();
    ExitCode::from(cmd::dispatch(&cli))
}
