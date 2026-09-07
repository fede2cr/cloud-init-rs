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

    /// The `CONTENT_START` signal, sent before the first part of a walk.
    fn start(&mut self) {}

    /// The `CONTENT_END` signal, sent after the last part of a walk. Only a
    /// handler that accumulates across parts has anything to do here.
    fn finish(&mut self, ctx: &Context) -> Result<(), Error> {
        let _ = ctx;
        Ok(())
    }
}

/// `util.write_file`, which creates the parent directory; `ci_sys` does not.
fn write_at_mode(path: &Path, contents: &[u8], mode: u32) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error(format!("Could not create {}: {e}", dir.display())))?;
    }
    ci_sys::write_file(path, contents, ci_sys::WriteOptions::mode(mode))
        .map_err(|e| Error(format!("Could not write {}: {e}", path.display())))
}

fn write_executable(path: &Path, contents: &[u8]) -> Result<(), Error> {
    write_at_mode(path, contents, 0o700)
}

/// `ShellScriptPartHandler`: drop the script in the instance's scripts dir.
#[derive(Debug)]
pub struct ShellScript(Lookup);

impl Default for ShellScript {
    fn default() -> Self {
        Self(Lookup::Scripts)
    }
}

impl ShellScript {
    /// `script_path`: vendor-data scripts land in their own directory.
    #[must_use]
    pub fn at(path: Lookup) -> Self {
        Self(path)
    }
}

impl PartHandler for ShellScript {
    fn types(&self) -> &'static [&'static str] {
        &["text/x-shellscript"]
    }

    fn handle(&mut self, ctx: &Context, part: &Part) -> Result<(), Error> {
        let dir = ctx.paths.instance_path(self.0);
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

/// `CLOUD_PREFIX`.
const CLOUD_PREFIX: &str = "#cloud-config";
/// `JSONP_PREFIX`.
const JSONP_PREFIX: &str = "#cloud-config-jsonp";
/// `MERGED_PART_SCHEMA_ERROR_PREFIX`.
const SCHEMA_ERROR_PREFIX: &str = "# Cloud-config part ignored SCHEMA_ERROR: ";
/// The strategy used when neither the part nor its headers name one. Not
/// `mergers.DEF_MERGE_TYPE`: parts used to be concatenated into one YAML
/// document and loaded once, where a later key simply won, so this handler
/// replaces rather than merges dictionaries to keep that behaviour.
const DEFAULT_MERGE_TYPE: &str = "dict(replace)+list()+str()";

/// Why a part did not make it into the merged config. The split is what reaches
/// disk: upstream records the parts that failed with a `ValueError` in the
/// written file and drops the rest silently.
#[derive(Debug)]
enum Rejected {
    /// Upstream's `except ValueError`.
    Value(String),
    /// Upstream's `except Exception`.
    Other(String),
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(msg) | Self::Other(msg) => f.write_str(msg),
        }
    }
}

/// `CloudConfigPartHandler`: fold every cloud-config part into one document.
///
/// The result is only written on [`PartHandler::finish`], since a part can
/// still patch or replace what earlier parts contributed.
#[derive(Debug)]
pub struct CloudConfig {
    path: Lookup,
    buf: Option<ci_config::Value>,
    file_names: Vec<String>,
    error_file_names: Vec<String>,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self::at(Lookup::CloudConfig)
    }
}

impl CloudConfig {
    /// `cloud_config_path`: vendor-data is merged into its own file.
    pub fn at(path: Lookup) -> Self {
        Self {
            path,
            buf: None,
            file_names: Vec::new(),
            error_file_names: Vec::new(),
        }
    }

    /// `_reset`, which leaves `error_file_names` alone; see docs/COMPAT.md.
    fn reset(&mut self) {
        self.file_names.clear();
        self.buf = None;
    }

    /// `_write_cloud_config`. An empty file is written when nothing merged,
    /// so the path exists either way.
    fn write(&self, ctx: &Context) -> Result<(), Error> {
        let mut file_lines = Vec::new();
        if !self.file_names.is_empty() {
            file_lines.push(format!("# from {} files", self.file_names.len()));
            for name in &self.file_names {
                let name = if name.is_empty() { "?" } else { name.as_str() };
                file_lines.push(format!("# {name}"));
            }
            file_lines.push(String::new());
        }
        for name in &self.error_file_names {
            file_lines.push(format!("{SCHEMA_ERROR_PREFIX}{name}"));
        }

        let contents = match &self.buf {
            Some(buf) => {
                let mut lines = vec![CLOUD_PREFIX.to_owned(), String::new()];
                lines.extend(file_lines);
                lines.push(ci_core::yamlfmt::dumps(buf));
                lines.join("\n")
            }
            None => String::new(),
        };
        write_at_mode(
            &ctx.paths.instance_path(self.path),
            contents.as_bytes(),
            0o600,
        )
    }

    /// `_extract_mergers` followed by the merge itself.
    fn merge_part(&mut self, part: &Part) -> Result<(), Rejected> {
        // A payload that will not decode raises UnicodeDecodeError upstream,
        // which is a ValueError.
        let text = part
            .text()
            .ok_or_else(|| Rejected::Value("payload is not text".to_owned()))?;
        // `util.load_yaml(payload)` allows only a mapping and answers None for
        // anything else, including a parse failure.
        let mut payload = ci_config::load_yaml(text, ci_config::Limits::default())
            .ok()
            .and_then(|value| match value {
                ci_config::Value::Object(map) => Some(map),
                _ => None,
            })
            .ok_or_else(|| Rejected::Value("empty cloud config".to_owned()))?;

        let mut specs =
            ci_config::merge::take_mergers(&mut payload).map_err(|e| rejected(&e))?;
        if let Some(header) = part.merge_type.as_deref() {
            specs.extend(
                ci_config::merge::string_extract_mergers(header)
                    .map_err(|e| rejected(&e))?,
            );
        }
        if specs.is_empty() {
            specs = ci_config::merge::string_extract_mergers(DEFAULT_MERGE_TYPE)
                .map_err(|e| rejected(&e))?;
        }
        let mergers =
            ci_config::Mergers::construct(&specs).map_err(|e| rejected(&e))?;

        let source = self.buf.take().unwrap_or(ci_config::Value::Null);
        self.buf = Some(mergers.merge(source, ci_config::Value::Object(payload)));
        Ok(())
    }

    /// `_merge_patch`.
    fn merge_patch(&mut self, part: &Part) -> Result<(), Rejected> {
        let text = part
            .text()
            .ok_or_else(|| Rejected::Value("payload is not text".to_owned()))?;
        let body = text.trim_start();
        let body = body.strip_prefix(JSONP_PREFIX).unwrap_or(body);
        let patch =
            ci_config::jsonpatch::Patch::parse(body).map_err(|e| rejected_patch(&e))?;
        let doc = self.buf.take().unwrap_or(ci_config::Value::Null);
        match patch.apply(&doc) {
            Ok(patched) => {
                self.buf = Some(patched);
                Ok(())
            }
            // Upstream assigns the result, so a patch that fails leaves the
            // buffer as it was rather than emptying it.
            Err(e) => {
                self.buf = Some(doc);
                Err(rejected_patch(&e))
            }
        }
    }
}

/// `string_extract_mergers` raises `ValueError`; the rest of the merger factory
/// raises `ImportError` or `KeyError`, which upstream only logs.
fn rejected(error: &ci_config::merge::MergeError) -> Rejected {
    let message = error.to_string();
    match error {
        ci_config::merge::MergeError::BadFormat(_) => Rejected::Value(message),
        _ => Rejected::Other(message),
    }
}

fn rejected_patch(error: &ci_config::jsonpatch::PatchError) -> Rejected {
    let message = error.to_string();
    if error.is_value_error() {
        Rejected::Value(message)
    } else {
        Rejected::Other(message)
    }
}

impl PartHandler for CloudConfig {
    fn types(&self) -> &'static [&'static str] {
        &["text/cloud-config", "text/cloud-config-jsonp"]
    }

    fn handle(&mut self, _ctx: &Context, part: &Part) -> Result<(), Error> {
        // An empty buffer, or one left over from a patch that ran before any
        // part merged, starts again from a mapping.
        if self.buf.is_none() || self.file_names.is_empty() {
            self.buf = Some(ci_config::Value::Object(ci_config::Object::new()));
        }
        let outcome = if part.content_type == "text/cloud-config-jsonp" {
            self.merge_patch(part)
        } else {
            self.merge_part(part)
        };
        match outcome {
            Ok(()) => {
                let name = part.filename.replace(['\n', '\r', '\t'], " ");
                self.file_names.push(name.trim().to_owned());
                Ok(())
            }
            Err(rejected) => {
                // The newline scrubbing above is not reached on this path
                // upstream, so a rejected part's name is only stripped.
                if let Rejected::Value(_) = rejected {
                    self.error_file_names.push(part.filename.trim().to_owned());
                }
                Err(Error(format!(
                    "Failed at merging in cloud config part from {}: {rejected}",
                    part.filename
                )))
            }
        }
    }

    fn start(&mut self) {
        self.reset();
    }

    fn finish(&mut self, ctx: &Context) -> Result<(), Error> {
        let result = self.write(ctx);
        self.reset();
        result
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

/// Handlers a rendered jinja payload may be dispatched to. Each is asked for
/// its [`PartHandler::types`], the way upstream indexes them by `list_types`.
type SubHandlers<'a> = [&'a mut dyn PartHandler];

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
        // The sub-handler is chosen by the rendered payload's type, but the
        // type handed to it is still `text/jinja2`, which is what upstream
        // passes on. The cloud-config handler branches on it; see
        // docs/COMPAT.md.
        let rendered_part = Part {
            content_type: part.content_type.clone(),
            filename: part.filename.clone(),
            payload: rendered.into_bytes(),
            launch_index: part.launch_index,
            merge_type: part.merge_type.clone(),
        };
        for handler in sub_handlers.iter_mut() {
            if handler.types().contains(&subtype) {
                return handler.handle(ctx, &rendered_part);
            }
        }
        Err(Error(format!(
            "Ignoring jinja template for {}. Could not find supported sub-handler for type {subtype}",
            part.filename
        )))
    }
}

/// What `walker_callback` did with a part. The caller logs it, so this crate
/// stays free of the log.
#[derive(Debug)]
pub enum Outcome {
    Handled,
    /// The type is in `disabled_handlers`.
    Excluded,
    /// No handler claims the type. The string is the excerpt upstream puts in
    /// the warning; `None` means the payload was empty and upstream only
    /// logged `Empty payload of type ...` at debug level.
    Unhandled(Option<String>),
    Failed(Error),
}

/// `_do_handlers`' handler table.
///
/// The handlers upstream imports from the `handlers/` directories are absent:
/// registering one means importing a Python module (COMPAT.md deviation 69).
#[derive(Debug)]
pub struct Set {
    script: ShellScript,
    per_boot: ShellScriptByFreq,
    per_instance: ShellScriptByFreq,
    per_once: ShellScriptByFreq,
    boothook: Box<dyn PartHandler>,
    cloud_config: CloudConfig,
    excluded: Vec<String>,
}

impl Set {
    fn new(script_path: Lookup, cloud_config_path: Lookup) -> Self {
        Self {
            script: ShellScript::at(script_path),
            per_boot: ShellScriptByFreq::new(ScriptFrequency::PerBoot),
            per_instance: ShellScriptByFreq::new(ScriptFrequency::PerInstance),
            per_once: ShellScriptByFreq::new(ScriptFrequency::PerOnce),
            boothook: Box::new(BootHook),
            cloud_config: CloudConfig::at(cloud_config_path),
            excluded: Vec::new(),
        }
    }

    /// `_default_handlers`.
    #[must_use]
    pub fn user_data() -> Self {
        Self::new(Lookup::Scripts, Lookup::CloudConfig)
    }

    /// `_default_vendordata_handlers`.
    #[must_use]
    pub fn vendor_data() -> Self {
        Self::new(Lookup::VendorScripts, Lookup::VendorCloudConfig)
    }

    /// `_default_vendordata2_handlers`.
    #[must_use]
    pub fn vendor_data2() -> Self {
        Self::new(Lookup::VendorScripts, Lookup::Vendor2CloudConfig)
    }

    /// `disabled_handlers`, which names content types rather than handlers.
    #[must_use]
    pub fn excluding(mut self, types: Vec<String>) -> Self {
        self.excluded = types;
        self
    }

    /// Replace the boot-hook handler, for a caller that must not execute one.
    #[must_use]
    pub fn with_boot_hooks(mut self, handler: Box<dyn PartHandler>) -> Self {
        self.boothook = handler;
        self
    }

    /// `CONTENT_START`.
    pub fn start(&mut self) {
        self.cloud_config.start();
    }

    /// One turn of `walker_callback`.
    pub fn handle(&mut self, ctx: &Context, part: &Part) -> Outcome {
        let ctype = part.content_type.as_str();
        if self.excluded.iter().any(|t| t == ctype) {
            return Outcome::Excluded;
        }
        let result = if ctype == "text/jinja2" {
            let mut subs: Vec<&mut dyn PartHandler> = vec![
                &mut self.cloud_config,
                &mut self.script,
                self.boothook.as_mut(),
            ];
            JinjaTemplate::handle_with(ctx, part, &mut subs)
        } else {
            let mut handlers: [&mut dyn PartHandler; 6] = [
                &mut self.cloud_config,
                &mut self.script,
                &mut self.per_boot,
                &mut self.per_instance,
                &mut self.per_once,
                self.boothook.as_mut(),
            ];
            match handlers.iter_mut().find(|h| h.types().contains(&ctype)) {
                Some(handler) => handler.handle(ctx, part),
                None if part.payload.is_empty() => return Outcome::Unhandled(None),
                None => return Outcome::Unhandled(Some(excerpt(&part.payload, 24))),
            }
        };
        match result {
            Ok(()) => Outcome::Handled,
            Err(e) => Outcome::Failed(e),
        }
    }

    /// `CONTENT_END`.
    pub fn finish(&mut self, ctx: &Context) -> Result<(), Error> {
        self.cloud_config.finish(ctx)
    }
}

/// `_extract_first_or_bytes`: the first line, capped at `size` characters.
fn excerpt(payload: &[u8], size: usize) -> String {
    let head = payload
        .get(..payload.len().min(4 * size))
        .unwrap_or(payload);
    let text = String::from_utf8_lossy(head).replace('\u{fffd}', "");
    let first = text.split('\n').next().unwrap_or("");
    first.chars().take(size).collect()
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
            merge_type: None,
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

        ShellScript::default()
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

        let mut script = ShellScript::default();
        let mut subs: Vec<&mut dyn PartHandler> = vec![&mut script];
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

    /// A cloud-config part, which is only written once the walk ends.
    fn config_part(filename: &str, payload: &str) -> Part {
        let mut part = part(filename, payload);
        part.content_type = "text/cloud-config".to_owned();
        part
    }

    fn merged(ctx: &Context) -> String {
        std::fs::read_to_string(ctx.paths.instance_path(Lookup::CloudConfig)).unwrap()
    }

    #[test]
    fn cloud_config_parts_are_written_once_the_walk_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut handler = CloudConfig::default();

        handler.start();
        handler
            .handle(&ctx, &config_part("a.yaml", "foo: 1\n"))
            .unwrap();
        assert!(!ctx.paths.instance_path(Lookup::CloudConfig).exists());
        handler.finish(&ctx).unwrap();

        assert_eq!(
            merged(&ctx),
            "#cloud-config\n\n# from 1 files\n# a.yaml\n\n---\nfoo: 1\n...\n"
        );
        assert_eq!(mode(&ctx.paths.instance_path(Lookup::CloudConfig)), 0o600);
    }

    #[test]
    fn a_walk_with_no_cloud_config_writes_an_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut handler = CloudConfig::default();
        handler.start();
        handler.finish(&ctx).unwrap();
        assert_eq!(merged(&ctx), "");
    }

    #[test]
    fn a_later_cloud_config_part_replaces_keys_rather_than_merging_them() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut handler = CloudConfig::default();

        handler.start();
        handler
            .handle(&ctx, &config_part("a", "foo: {a: 1}\nkeep: 1\n"))
            .unwrap();
        handler
            .handle(&ctx, &config_part("b", "foo: {b: 2}\n"))
            .unwrap();
        handler.finish(&ctx).unwrap();

        assert!(merged(&ctx).contains("    b: 2"), "{}", merged(&ctx));
        assert!(!merged(&ctx).contains("a: 1"), "{}", merged(&ctx));
        assert!(merged(&ctx).contains("keep: 1"), "{}", merged(&ctx));
    }

    #[test]
    fn a_merge_type_header_overrides_the_replacing_default() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut handler = CloudConfig::default();
        let mut second = config_part("b", "foo: {b: 2}\n");
        second.merge_type = Some("dict(recurse_dict)".to_owned());

        handler.start();
        handler
            .handle(&ctx, &config_part("a", "foo: {a: 1}\n"))
            .unwrap();
        handler.handle(&ctx, &second).unwrap();
        handler.finish(&ctx).unwrap();

        assert!(merged(&ctx).contains("a: 1"), "{}", merged(&ctx));
        assert!(merged(&ctx).contains("b: 2"), "{}", merged(&ctx));
    }

    #[test]
    fn a_part_rejected_with_a_value_error_is_named_in_the_written_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut handler = CloudConfig::default();

        handler.start();
        handler
            .handle(&ctx, &config_part("good", "foo: 1\n"))
            .unwrap();
        // Nothing but comments, so `load_yaml` answers None.
        handler
            .handle(&ctx, &config_part(" bad ", "# only a comment\n"))
            .unwrap_err();
        handler.finish(&ctx).unwrap();

        assert!(
            merged(&ctx).contains("# Cloud-config part ignored SCHEMA_ERROR: bad"),
            "{}",
            merged(&ctx)
        );
    }

    #[test]
    fn a_patch_applies_to_what_earlier_parts_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut handler = CloudConfig::default();
        let mut patch = config_part(
            "p",
            r#"#cloud-config-jsonp
[{"op": "add", "path": "/added", "value": 2}]"#,
        );
        patch.content_type = "text/cloud-config-jsonp".to_owned();

        handler.start();
        handler.handle(&ctx, &config_part("a", "foo: 1\n")).unwrap();
        handler.handle(&ctx, &patch).unwrap();
        handler.finish(&ctx).unwrap();

        assert!(merged(&ctx).contains("added: 2"), "{}", merged(&ctx));
        assert!(merged(&ctx).contains("foo: 1"), "{}", merged(&ctx));
    }

    #[test]
    fn a_patch_that_conflicts_leaves_the_buffer_alone_and_is_not_named() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut handler = CloudConfig::default();
        let mut patch = config_part("p", r#"[{"op": "remove", "path": "/nope"}]"#);
        patch.content_type = "text/cloud-config-jsonp".to_owned();

        handler.start();
        handler.handle(&ctx, &config_part("a", "foo: 1\n")).unwrap();
        handler.handle(&ctx, &patch).unwrap_err();
        handler.finish(&ctx).unwrap();

        // A conflict is not a ValueError, so the part is dropped without being
        // recorded, and what it failed to patch survives.
        assert!(merged(&ctx).contains("foo: 1"), "{}", merged(&ctx));
        assert!(!merged(&ctx).contains("SCHEMA_ERROR"), "{}", merged(&ctx));
    }

    #[test]
    fn a_malformed_patch_is_a_value_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path());
        let mut handler = CloudConfig::default();
        let mut patch = config_part("p", "#cloud-config-jsonp\nnot json");
        patch.content_type = "text/cloud-config-jsonp".to_owned();

        handler.start();
        handler.handle(&ctx, &patch).unwrap_err();
        handler.finish(&ctx).unwrap();

        assert!(merged(&ctx).contains("SCHEMA_ERROR: p"), "{}", merged(&ctx));
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}
