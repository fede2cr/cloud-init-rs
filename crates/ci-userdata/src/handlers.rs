//! Port of `cloudinit/handlers/*`: what each user-data part turns into on disk.
//!
//! Upstream registers handler objects by content type and walks the parts once,
//! calling `handle_part` for each. The registry is the same idea here, but the
//! parts come from [`crate::process`] rather than an `email` message tree.

use std::fmt;
use std::path::{Path, PathBuf};

use ci_core::paths::{Lookup, Paths};

use crate::types::type_from_starts_with;
use crate::Part;

/// `FN_ALLOWED`. Everything outside this set is dropped from a filename, which
/// is also what keeps a part from escaping its directory: `/` is replaced and
/// no other separator survives.
const FN_ALLOWED: &str =
    "_-.()0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// `util.clean_filename`.
pub fn clean_filename(name: &str) -> String {
    name.replace('/', "_")
        .chars()
        .filter(|c| FN_ALLOWED.contains(*c))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// `util.dos2unix`: only rewrite when the *first* line ends `\r\n`.
pub fn dos2unix(contents: &[u8]) -> Vec<u8> {
    let Some(pos) = contents.iter().position(|b| *b == b'\n') else {
        return contents.to_vec();
    };
    if pos == 0 || contents.get(pos - 1) != Some(&b'\r') {
        return contents.to_vec();
    }
    let mut out = Vec::with_capacity(contents.len());
    let mut idx = 0;
    while let Some(byte) = contents.get(idx) {
        if *byte == b'\r' && contents.get(idx + 1) == Some(&b'\n') {
            idx += 1;
            continue;
        }
        out.push(*byte);
        idx += 1;
    }
    out
}

/// `util.strip_prefix_suffix`, prefix half only.
fn strip_prefix(contents: &[u8], prefix: &[u8]) -> Vec<u8> {
    contents.strip_prefix(prefix).unwrap_or(contents).to_vec()
}

/// What a handler needs from the running instance.
#[derive(Debug, Clone)]
pub struct Context {
    pub paths: Paths,
    pub instance_id: Option<String>,
}

/// A part could not be handled. Upstream logs and continues; the caller decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// `handlers.Handler`.
pub trait PartHandler: fmt::Debug {
    /// The content types this handler claims, via `Handler.list_types`.
    fn types(&self) -> &'static [&'static str];

    fn handle(&mut self, ctx: &Context, part: &Part) -> Result<(), Error>;
}

/// `util.write_file`, which creates the parent directory; `ci_sys` does not.
fn write_executable(path: &Path, contents: &[u8]) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error(format!("Could not create {}: {e}", dir.display())))?;
    }
    ci_sys::write_file(path, contents, ci_sys::WriteOptions::mode(0o700))
        .map_err(|e| Error(format!("Could not write {}: {e}", path.display())))
}

/// `ShellScriptPartHandler`: drop the script in the instance's scripts dir.
#[derive(Debug, Default)]
pub struct ShellScript;

impl PartHandler for ShellScript {
    fn types(&self) -> &'static [&'static str] {
        &["text/x-shellscript"]
    }

    fn handle(&mut self, ctx: &Context, part: &Part) -> Result<(), Error> {
        let dir = ctx.paths.instance_path(Lookup::Scripts);
        write_executable(
            &dir.join(clean_filename(&part.filename)),
            &dos2unix(&part.payload),
        )
    }
}

/// The frequency a `text/x-shellscript-per-*` part runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptFrequency {
    PerBoot,
    PerInstance,
    PerOnce,
}

impl ScriptFrequency {
    /// `path_map`.
    fn folder(self) -> &'static str {
        match self {
            Self::PerBoot => "per-boot",
            Self::PerInstance => "per-instance",
            Self::PerOnce => "per-once",
        }
    }

    /// `get_mime_type_by_frequency`.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::PerBoot => "text/x-shellscript-per-boot",
            Self::PerInstance => "text/x-shellscript-per-instance",
            Self::PerOnce => "text/x-shellscript-per-once",
        }
    }
}

/// `ShellScriptByFreqPartHandler`.
#[derive(Debug)]
pub struct ShellScriptByFreq(ScriptFrequency);

impl ShellScriptByFreq {
    pub fn new(frequency: ScriptFrequency) -> Self {
        Self(frequency)
    }
}

impl PartHandler for ShellScriptByFreq {
    fn types(&self) -> &'static [&'static str] {
        match self.0 {
            ScriptFrequency::PerBoot => &["text/x-shellscript-per-boot"],
            ScriptFrequency::PerInstance => &["text/x-shellscript-per-instance"],
            ScriptFrequency::PerOnce => &["text/x-shellscript-per-once"],
        }
    }

    fn handle(&mut self, ctx: &Context, part: &Part) -> Result<(), Error> {
        // Not the instance dir: these outlive the instance.
        let dir = ctx.paths.cpath(Lookup::Scripts).join(self.0.folder());
        write_executable(
            &dir.join(clean_filename(&part.filename)),
            &dos2unix(&part.payload),
        )
    }
}

/// `BootHookPartHandler`: write the hook, then run it immediately.
#[derive(Debug, Default)]
pub struct BootHook;

impl BootHook {
    /// `_write_part`. Public because writing and running are worth separating:
    /// a caller that only wants the file on disk should not have to run it.
    pub fn write_part(ctx: &Context, part: &Part) -> Result<PathBuf, Error> {
        let Some(instance_id) = ctx.instance_id.as_deref() else {
            return Err(Error("No instance id, cannot write boothook".to_owned()));
        };
        let dir = ctx.paths.instance_path_for(instance_id, Lookup::BootHooks);
        let path = dir.join(clean_filename(&part.filename));
        let stripped = strip_prefix(&dos2unix(&part.payload), b"#cloud-boothook");
        let start = stripped
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(stripped.len());
        write_executable(&path, stripped.get(start..).unwrap_or(&[]))?;
        Ok(path)
    }
}

impl PartHandler for BootHook {
    fn types(&self) -> &'static [&'static str] {
        &["text/cloud-boothook"]
    }

    fn handle(&mut self, ctx: &Context, part: &Part) -> Result<(), Error> {
        let path = Self::write_part(ctx, part)?;
        let mut cmd = ci_sys::Subp::new([path.as_os_str()]).inherit_env();
        if let Some(instance_id) = ctx.instance_id.as_deref() {
            cmd = cmd.env("INSTANCE_ID", instance_id);
        }
        cmd.run()
            .map_err(|e| {
                Error(format!(
                    "Boothooks script {} execution error: {e}",
                    path.display()
                ))
            })
            .map(|_| ())
    }
}

/// `JinjaTemplatePartHandler`: render, then hand the result to whichever
/// handler claims the rendered payload's type.
#[derive(Debug, Default)]
pub struct JinjaTemplate;

impl JinjaTemplate {
    /// `render_jinja_payload_from_file`.
    fn render(ctx: &Context, part: &Part) -> Result<String, Error> {
        let payload = part
            .text()
            .ok_or_else(|| Error("Jinja template is not valid UTF-8".to_owned()))?;
        if !matches!(
            ci_template::detect_template(payload),
            Ok((ci_template::TemplateKind::Jinja, _))
        ) {
            return Err(Error("Payload is not a jinja template".to_owned()));
        }
        let vars_file = ctx.paths.run_path(Lookup::InstanceDataSensitive);
        let text = std::fs::read_to_string(&vars_file).map_err(|e| {
            Error(format!(
                "Cannot render jinja template vars. Instance data not yet present at {}: {e}",
                vars_file.display()
            ))
        })?;
        let data: ci_config::Value = serde_json::from_str(&text)
            .map_err(|e| Error(format!("Loading Jinja instance data failed: {e}")))?;
        let vars = ci_template::convert_jinja_instance_data_with_aliases(&data);
        ci_template::render_string(payload, &vars).map_err(|e| {
            Error(format!(
                "Ignoring jinja template for {}: {e}",
                part.filename
            ))
        })
    }
}

/// Handlers a rendered jinja payload may be dispatched to, by content type.
type SubHandlers<'a> = [(&'static str, &'a mut dyn PartHandler)];

impl JinjaTemplate {
    /// `handle_part`, with the sub-handler table passed in rather than stored,
    /// so the borrow checker does not need a second copy of every handler.
    pub fn handle_with(
        ctx: &Context,
        part: &Part,
        sub_handlers: &mut SubHandlers<'_>,
    ) -> Result<(), Error> {
        let rendered = Self::render(ctx, part)?;
        if rendered.is_empty() {
            return Ok(());
        }
        let subtype =
            type_from_starts_with(rendered.as_bytes(), None).ok_or_else(|| {
                Error(format!(
                "Ignoring jinja template for {}. Could not find supported sub-handler",
                part.filename
            ))
            })?;
        let rendered_part = Part {
            content_type: subtype.to_owned(),
            filename: part.filename.clone(),
            payload: rendered.into_bytes(),
            launch_index: part.launch_index,
        };
        for (ctype, handler) in sub_handlers.iter_mut() {
            if *ctype == subtype {
                return handler.handle(ctx, &rendered_part);
            }
        }
        Err(Error(format!(
            "Ignoring jinja template for {}. Could not find supported sub-handler for type {subtype}",
            part.filename
        )))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn context(root: &Path) -> Context {
        let paths = Paths {
            cloud_dir: root.join("cloud"),
            run_dir: root.join("run"),
            ..Default::default()
        };
        Context {
            paths,
            instance_id: Some("i-1".to_owned()),
        }
    }

    fn part(filename: &str, payload: &str) -> Part {
        Part {
            content_type: "text/x-shellscript".to_owned(),
            filename: filename.to_owned(),
            payload: payload.as_bytes().to_vec(),
            launch_index: None,
        }
    }

    #[test]
    fn a_filename_keeps_only_allowed_characters() {
        assert_eq!(clean_filename("part-001"), "part-001");
        assert_eq!(clean_filename("a b/c"), "ab_c");
        assert_eq!(clean_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(clean_filename("réunion.sh"), "runion.sh");
    }

    #[test]
    fn dos2unix_only_rewrites_when_the_first_line_is_crlf() {
        assert_eq!(dos2unix(b"a\r\nb\r\n"), b"a\nb\n");
        // First line is bare LF, so upstream leaves the rest alone.
        assert_eq!(dos2unix(b"a\nb\r\n"), b"a\nb\r\n");
        assert_eq!(dos2unix(b"\r\n"), b"\n");
        assert_eq!(dos2unix(b"\n\r\n"), b"\n\r\n");
        assert_eq!(dos2unix(b"no newline"), b"no newline");
    }

    #[test]
    fn a_shell_script_lands_in_the_instance_scripts_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        std::os::unix::fs::symlink("instances/i-1", ctx.paths.instance_link()).ok();

        ShellScript
            .handle(&ctx, &part("part-001", "#!/bin/sh\necho hi\n"))
            .unwrap();

        let path = ctx.paths.instance_path(Lookup::Scripts).join("part-001");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        assert_eq!(mode(&path), 0o700);
    }

    #[test]
    fn a_per_boot_script_lands_outside_the_instance_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());

        ShellScriptByFreq::new(ScriptFrequency::PerBoot)
            .handle(&ctx, &part("run.sh", "#!/bin/sh\n"))
            .unwrap();

        let path = ctx
            .paths
            .cpath(Lookup::Scripts)
            .join("per-boot")
            .join("run.sh");
        assert!(path.is_file(), "{}", path.display());
    }

    #[test]
    fn every_frequency_maps_to_its_own_folder_and_type() {
        for (freq, folder) in [
            (ScriptFrequency::PerBoot, "per-boot"),
            (ScriptFrequency::PerInstance, "per-instance"),
            (ScriptFrequency::PerOnce, "per-once"),
        ] {
            assert_eq!(freq.folder(), folder);
            assert_eq!(freq.content_type(), format!("text/x-shellscript-{folder}"));
            assert_eq!(ShellScriptByFreq::new(freq).types(), [freq.content_type()]);
        }
    }

    #[test]
    fn a_boothook_is_written_without_its_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut hook = part("part-001", "#cloud-boothook\n#!/bin/true\n");
        hook.content_type = "text/cloud-boothook".to_owned();

        let path = BootHook::write_part(&ctx, &hook).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "#!/bin/true\n");
        assert_eq!(mode(&path), 0o700);
    }

    #[test]
    fn a_boothook_without_an_instance_id_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = context(tmp.path());
        ctx.instance_id = None;
        let err =
            BootHook::write_part(&ctx, &part("p", "#cloud-boothook\n")).unwrap_err();
        assert!(err.0.contains("No instance id"), "{err}");
    }

    #[test]
    fn a_jinja_part_renders_then_dispatches_by_rendered_type() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        std::os::unix::fs::symlink("instances/i-1", ctx.paths.instance_link()).ok();
        let vars = ctx.paths.run_path(Lookup::InstanceDataSensitive);
        std::fs::create_dir_all(vars.parent().unwrap()).unwrap();
        std::fs::write(&vars, r#"{"v1": {"greeting": "hi"}}"#).unwrap();

        let mut jinja = part(
            "part-001",
            "## template: jinja\n#!/bin/sh\necho {{ v1.greeting }}\n",
        );
        jinja.content_type = "text/jinja2".to_owned();

        let mut script = ShellScript;
        let mut subs: Vec<(&'static str, &mut dyn PartHandler)> =
            vec![("text/x-shellscript", &mut script)];
        JinjaTemplate::handle_with(&ctx, &jinja, &mut subs).unwrap();

        let path = ctx.paths.instance_path(Lookup::Scripts).join("part-001");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
    }

    #[test]
    fn a_jinja_part_without_instance_data_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut jinja = part("part-001", "## template: jinja\n#!/bin/sh\n");
        jinja.content_type = "text/jinja2".to_owned();
        let err = JinjaTemplate::render(&ctx, &jinja).unwrap_err();
        assert!(err.0.contains("Instance data not yet present"), "{err}");
    }

    #[test]
    fn a_non_jinja_payload_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let err = JinjaTemplate::render(&ctx, &part("p", "#!/bin/sh\n")).unwrap_err();
        assert!(err.0.contains("not a jinja template"), "{err}");
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}
