//! Dump the files the part handlers write, as JSON.
//!
//! Reads a user-data blob on stdin; argv[1] is a scratch root. Exists so the
//! handlers can be diffed against Python; see tests/differential/handlers.py.
//!
//! Boot hooks are written but never executed, matching the Python driver.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use ci_core::paths::{Lookup, Paths};
use ci_userdata::handlers::{
    BootHook, Context, Error as HandlerError, Outcome, PartHandler,
};
use ci_userdata::Part;

const INSTANCE_ID: &str = "i-1";

/// A boot hook that is written but never run.
///
/// Upstream executes the hook the moment it is written. This has to be safe on
/// a live host, and the write is the part being compared, so drop the exec.
#[derive(Debug)]
struct WriteOnlyBootHook;

impl PartHandler for WriteOnlyBootHook {
    fn types(&self) -> &'static [&'static str] {
        &["text/cloud-boothook"]
    }

    fn handle(&mut self, ctx: &Context, part: &Part) -> Result<(), HandlerError> {
        BootHook::write_part(ctx, part).map(|_| ())
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        eprintln!("usage: dump-handlers <root>");
        std::process::exit(2);
    };
    let root = PathBuf::from(root);

    let mut blob = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut blob) {
        eprintln!("read: {e}");
        std::process::exit(1);
    }

    let ctx = match build_tree(&root) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("setup: {e}");
            std::process::exit(1);
        }
    };
    let processed = match ci_userdata::process(&blob, &ctx.paths) {
        Ok(processed) => processed,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let mut set = ci_userdata::handlers::Set::user_data()
        .with_boot_hooks(Box::new(WriteOnlyBootHook));

    set.start();
    for part in &processed.parts {
        match set.handle(&ctx, part) {
            Outcome::Failed(e) => eprintln!("{}: {e}", part.filename),
            Outcome::Handled | Outcome::Excluded | Outcome::Unhandled(_) => {}
        }
    }
    if let Err(e) = set.finish(&ctx) {
        eprintln!("{e}");
    }

    match serde_json::to_string_pretty(&snapshot(&root)) {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("encode: {e}");
            std::process::exit(1);
        }
    }
}

fn build_tree(root: &Path) -> std::io::Result<Context> {
    let cloud_dir = root.join("cloud");
    let run_dir = root.join("run");
    std::fs::create_dir_all(cloud_dir.join("instances").join(INSTANCE_ID))?;
    std::fs::create_dir_all(&run_dir)?;
    let link = cloud_dir.join("instance");
    if !link.exists() {
        std::os::unix::fs::symlink(Path::new("instances").join(INSTANCE_ID), &link)?;
    }
    let paths = Paths {
        cloud_dir,
        run_dir,
        ..Default::default()
    };
    std::fs::write(
        paths.run_path(Lookup::InstanceDataSensitive),
        r#"{"v1": {"greeting": "hi"}, "ds": {"meta-data": {"a": "b"}}}"#,
    )?;
    Ok(Context {
        paths,
        instance_id: Some(INSTANCE_ID.to_owned()),
    })
}

fn snapshot(root: &Path) -> Vec<serde_json::Value> {
    let mut files = Vec::new();
    collect(root, root, &mut files);
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    files
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<serde_json::Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect(root, &path, out);
        } else if meta.is_file() {
            let content = std::fs::read(&path).unwrap_or_default();
            out.push(serde_json::json!({
                "path": path.strip_prefix(root).unwrap_or(&path).to_string_lossy(),
                "mode": format!("0o{:o}", meta.permissions().mode() & 0o7777),
                "content": String::from_utf8_lossy(&content),
            }));
        }
    }
}
