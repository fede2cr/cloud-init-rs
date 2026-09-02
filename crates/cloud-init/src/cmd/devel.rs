//! `cloud-init devel` — development helpers.
//!
//! `net-convert` and `hotplug-hook` depend on subsystems that land in Phases 3
//! and 4.

mod make_mime;

use std::path::PathBuf;

use clap::{Args as ClapArgs, Subcommand};

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: DevelCommand,
}

#[derive(Debug, Subcommand)]
pub enum DevelCommand {
    /// Render a template file using instance-data values.
    Render(RenderArgs),
    /// Generate multi-part mime messages for user-data.
    #[command(name = "make-mime")]
    MakeMime(make_mime::Args),
}

#[derive(Debug, ClapArgs)]
pub struct RenderArgs {
    /// Path to the user-data file to render.
    pub user_data: PathBuf,

    /// Optional path to instance-data.json file.
    #[arg(long, short = 'i')]
    pub instance_data: Option<PathBuf>,

    /// Add verbose messages during template render.
    #[arg(long, short = 'd')]
    pub debug: bool,
}

pub fn run(args: &Args) -> u8 {
    match &args.command {
        DevelCommand::Render(render) => run_render(render),
        DevelCommand::MakeMime(mime) => make_mime::run(mime),
    }
}

fn run_render(args: &RenderArgs) -> u8 {
    let paths = ci_core::Paths::read();
    let instance_data_fn =
        super::instance_data_path(&paths, args.instance_data.as_deref());

    if !instance_data_fn.exists() {
        eprintln!(
            "Missing instance-data.json file: {}",
            instance_data_fn.display()
        );
        return 1;
    }

    let Ok(raw) = ci_sys::path::read_text_capped(
        &instance_data_fn,
        ci_sys::path::DEFAULT_MAX_BYTES,
    ) else {
        eprintln!(
            "Error loading instance-data.json file: {}",
            instance_data_fn.display()
        );
        return 1;
    };
    let Ok(instance_data) = serde_json::from_str::<serde_json::Value>(&raw) else {
        eprintln!(
            "Error loading instance-data.json file: {}",
            instance_data_fn.display()
        );
        return 1;
    };

    let Ok(Some(payload)) = ci_sys::path::read_text_optional(
        &args.user_data,
        ci_sys::path::DEFAULT_MAX_BYTES,
    ) else {
        eprintln!("Missing user-data file: {}", args.user_data.display());
        return 1;
    };

    let params = ci_template::convert_jinja_instance_data_with_aliases(&instance_data);
    match ci_template::render_string(&payload, &params) {
        Ok(rendered) if !rendered.is_empty() => {
            print!("{rendered}");
            0
        }
        Ok(_) => {
            eprintln!(
                "Unable to render user-data file: {}",
                args.user_data.display()
            );
            1
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}
