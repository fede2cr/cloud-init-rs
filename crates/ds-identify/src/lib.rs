//! A Rust port of cloud-init's `ds-identify` shell script.
//!
//! `ds-identify` runs as a systemd generator, before anything else in
//! cloud-init. It looks at DMI, filesystem labels, the kernel command line and
//! a handful of seed directories, decides which datasources are plausible, and
//! writes `/run/cloud-init/cloud.cfg` with a narrowed `datasource_list` -- or
//! disables cloud-init entirely by its exit code.
//!
//! The port is a transliteration, not a redesign. Where the script's behaviour
//! is surprising (grep-based config "parsing", a `datasource_list: [  ]` that
//! means "found nothing", `get_single_line_flow_sequence` discarding its own
//! work) the port reproduces it and says so in a comment, because the whole
//! value of this binary is agreeing with the shell script it replaces.
//!
//! Two things are deliberately *not* ported, both of which execute arbitrary
//! code as root:
//! * `get_environment`, which sources `$PATH_DI_ENV` as shell;
//! * the `DI_MAIN=<program>` fallthrough, which `exec`s an arbitrary program.
//!
//! Both are marked "testing only - NOT use for production code" upstream, and
//! both are reachable by anything that can write one environment variable into
//! the generator's environment. See COMPAT.md.

pub mod checks;
pub mod config;
pub mod glob;
pub mod log;
pub mod paths;
pub mod read;
pub mod shell;

use std::fmt::Write as _;
use std::io::Write as _;

use crate::config::Policy;
use crate::log::Log;
use crate::paths::Paths;
use crate::read::FsInfo;
use crate::shell::split_words;

/// `DS_FOUND` / `DS_NOT_FOUND` / `DS_MAYBE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsCheck {
    Found,
    NotFound,
    Maybe,
}

/// Everything the script keeps in `DI_*` globals.
#[derive(Debug)]
pub struct Info {
    pub paths: Paths,
    pub log: Log,
    pub uname_kernel_name: String,
    pub uname_kernel_version: String,
    pub uname_machine: String,
    pub virt: String,
    pub container: bool,
    pub kernel_cmdline: String,
    pub dmi_sys_vendor: String,
    pub dmi_board_name: String,
    pub dmi_chassis_asset_tag: String,
    pub dmi_product_name: String,
    pub dmi_product_serial: String,
    pub dmi_product_uuid: String,
    pub pid1_product_name: String,
    pub fs: FsInfo,
    pub dsname: String,
    pub dslist: String,
    pub policy: Policy,
    /// `_RET_excfg`: extra config a check wants folded into the result.
    pub excfg: String,
    /// `_IS_IBM_CLOUD`, computed at most once.
    pub(crate) ibm_cloud: Option<bool>,
    /// `STATE_FLOPPY_PROBED`.
    pub(crate) floppy_probed: Option<bool>,
}

impl Info {
    #[must_use]
    pub fn new(paths: Paths, log: Log) -> Self {
        Self {
            paths,
            log,
            uname_kernel_name: String::new(),
            uname_kernel_version: String::new(),
            uname_machine: String::new(),
            virt: String::new(),
            container: false,
            kernel_cmdline: String::new(),
            dmi_sys_vendor: String::new(),
            dmi_board_name: String::new(),
            dmi_chassis_asset_tag: String::new(),
            dmi_product_name: String::new(),
            dmi_product_serial: String::new(),
            dmi_product_uuid: String::new(),
            pid1_product_name: String::new(),
            fs: FsInfo::default(),
            dsname: String::new(),
            dslist: String::new(),
            policy: config::parse_policy(""),
            excfg: String::new(),
            ibm_cloud: None,
            floppy_probed: None,
        }
    }

    /// `read_uname_info` followed by `set_run_path`.
    pub fn read_uname(&mut self) {
        let (name, version, machine) = read::read_uname_info(&mut self.log);
        self.uname_kernel_name = name;
        self.uname_kernel_version = version;
        self.uname_machine = machine;
    }

    /// `read_virt` and `read_kernel_cmdline`, which `_main` does before
    /// `collect_info` so that `is_disabled` can run early.
    pub fn read_virt_and_cmdline(&mut self) {
        let kernel_name = self.uname_kernel_name.clone();
        self.virt = read::detect_virt(&mut self.log, &self.paths, &kernel_name);
        self.container = read::is_container(&self.virt);
        self.kernel_cmdline = read::read_kernel_cmdline(&self.paths, self.container);
    }

    /// `collect_info`.
    pub fn collect(&mut self) {
        self.pid1_product_name = read::read_pid1_product_name(&self.paths);

        let cmdline = self.kernel_cmdline.clone();
        let result = config::read_config(&mut self.log, &self.paths, &cmdline);
        self.dsname = result.dsname;
        self.policy = result.policy;

        let dsname = self.dsname.clone();
        self.dslist =
            config::read_datasource_list(&mut self.log, &self.paths, &dsname, &cmdline);

        for field in [
            "sys_vendor",
            "board_name",
            "chassis_asset_tag",
            "product_name",
            "product_serial",
            "product_uuid",
        ] {
            let value = read::get_dmi_field(&mut self.log, &self.paths, field);
            match field {
                "sys_vendor" => self.dmi_sys_vendor = value,
                "board_name" => self.dmi_board_name = value,
                "chassis_asset_tag" => self.dmi_chassis_asset_tag = value,
                "product_name" => self.dmi_product_name = value,
                "product_serial" => self.dmi_product_serial = value,
                _ => self.dmi_product_uuid = value,
            }
        }

        self.fs = read::read_fs_info(&mut self.log, self.container);
    }

    /// `_print_info`, in the script's field order.
    #[must_use]
    pub fn print_info(&self) -> String {
        let mut out = String::new();
        let fields: [(&str, &str); 20] = [
            ("DMI_PRODUCT_NAME", &self.dmi_product_name),
            ("DMI_SYS_VENDOR", &self.dmi_sys_vendor),
            ("DMI_PRODUCT_SERIAL", &self.dmi_product_serial),
            ("DMI_PRODUCT_UUID", &self.dmi_product_uuid),
            ("PID_1_PRODUCT_NAME", &self.pid1_product_name),
            ("DMI_CHASSIS_ASSET_TAG", &self.dmi_chassis_asset_tag),
            ("DMI_BOARD_NAME", &self.dmi_board_name),
            ("FS_LABELS", &self.fs.labels),
            ("ISO9660_DEVS", &self.fs.iso9660_devs),
            ("KERNEL_CMDLINE", &self.kernel_cmdline),
            ("VIRT", &self.virt),
            ("UNAME_KERNEL_NAME", &self.uname_kernel_name),
            ("UNAME_KERNEL_VERSION", &self.uname_kernel_version),
            ("UNAME_MACHINE", &self.uname_machine),
            ("DSNAME", &self.dsname),
            ("DSLIST", &self.dslist),
            ("MODE", &self.policy.mode),
            ("ON_FOUND", &self.policy.found),
            ("ON_MAYBE", &self.policy.maybe),
            ("ON_NOTFOUND", &self.policy.notfound),
        ];
        for (name, value) in fields {
            let _ = writeln!(out, "{name}={value}");
        }
        let _ = writeln!(out, "pid={} ppid={}", std::process::id(), ppid());
        let _ = writeln!(out, "is_container={}", self.container);
        out
    }

    /// `write_result`: the lines, two-space indented under `di_report:` when
    /// the mode says to report rather than act.
    fn write_result(&mut self, lines: &[String]) -> bool {
        let mut body = String::new();
        let pre = if self.policy.mode == "report" {
            body.push_str("di_report:\n");
            "  "
        } else {
            ""
        };
        for line in lines {
            body.push_str(pre);
            body.push_str(line);
            body.push('\n');
        }
        let runcfg = self.paths.run_ci_cfg.clone();
        let wrote = std::fs::File::create(&runcfg)
            .and_then(|mut f| f.write_all(body.as_bytes()))
            .is_ok();
        if !wrote {
            self.log
                .error(&format!("failed to write to {}", runcfg.display()));
        }
        wrote
    }

    /// `found(ds1 [ds2 ...] [-- extra])`.
    fn found(&mut self, list: &[String], extra: &[String]) {
        let mut joined = list.join(", ");
        let padded = format!(" {joined} ");
        let has_none = padded.contains(" None, ") || padded.ends_with(" None ");
        if !has_none && !joined.is_empty() {
            // `${list:+${list}, None}` leaves an *empty* list empty, which is
            // how `record_notfound` writes "nothing was found" as an empty
            // flow sequence.
            joined.push_str(", None");
        }
        let mut lines = vec![format!("datasource_list: [ {joined} ]")];
        // `[ $# -eq 1 ] && [ -z "$1" ]`: a single empty extra line is dropped.
        if !(extra.len() == 1 && extra.first().is_some_and(String::is_empty)) {
            lines.extend(extra.iter().cloned());
        }
        self.write_result(&lines);
    }

    /// `record_notfound`.
    fn record_notfound(&mut self) {
        if self.policy.mode == "report" {
            self.found(&[], &[]);
        } else if self.policy.mode == "search" {
            let msg = format!(
                "# reporting not found result. notfound={}.",
                self.policy.notfound
            );
            // A local `DI_MODE=report` so the empty list lands under
            // `di_report:` and cloud-init does not read it as "search nothing".
            let saved = std::mem::replace(&mut self.policy.mode, "report".to_owned());
            self.found(&[], &[msg]);
            self.policy.mode = saved;
        }
    }

    /// `manual_clean_and_existing`.
    fn manual_clean_and_existing(&self) -> bool {
        self.paths
            .in_var_lib_cloud("/instance/manual-clean")
            .is_file()
    }

    /// `is_disabled`.
    ///
    /// The marker file path is hardcoded upstream -- it is *not* prefixed with
    /// `$PATH_ROOT` and does not honour `$PATH_ETC_CLOUD`, unlike every other
    /// path in the script. Reproduced verbatim so the port and the script
    /// agree; recorded as a bug in COMPAT.md.
    fn is_disabled(&mut self) -> bool {
        if std::path::Path::new("/etc/cloud/cloud-init.disabled").is_file() {
            self.log
                .debug(1, "disabled by marker file /etc/cloud/cloud-init.disabled");
            return true;
        }
        if std::env::var("KERNEL_CMDLINE").as_deref() == Ok("cloud-init=disabled") {
            self.log
                .debug(1, "disabled by KERNEL_CMDLINE environment variable");
            return true;
        }
        if self.kernel_cmdline.contains("cloud-init=disabled") {
            self.log
                .debug(1, "disabled by kernel command line cloud-init=disabled");
            return true;
        }
        false
    }
}

fn ppid() -> u32 {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    text.lines()
        .find_map(|l| l.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// `${acc:+${acc}${CR}}${new}`: a newline separator only when both are present.
fn append_excfg(acc: &mut String, new: &str) {
    if !acc.is_empty() {
        acc.push('\n');
    }
    acc.push_str(new);
}

/// `_main`.
#[allow(clippy::too_many_lines)]
pub fn run_main(info: &mut Info, args: &[String]) -> i32 {
    let ret_dis = 1;
    let ret_en = 0;

    let uptime = read::read_uptime(&info.paths);
    let joined = args.join(" ");
    // `debug 1 "[up ${_RET}s]" "ds-identify $*"`: the trailing space when there
    // are no arguments is upstream's, and the log is compared byte for byte.
    info.log
        .debug(1, &format!("[up {uptime}s] ds-identify {joined}"));
    info.read_virt_and_cmdline();
    if info.is_disabled() {
        return 2;
    }
    info.collect();

    let dump = info.print_info();
    if info.log.is_stderr() {
        eprint!("{dump}");
    } else {
        for line in dump.lines() {
            info.log.write(line);
        }
    }

    match info.policy.mode.as_str() {
        "disabled" => {
            info.log
                .debug(1, &format!("mode=disabled. returning {ret_dis}"));
            return ret_dis;
        }
        "enabled" => {
            info.log
                .debug(1, &format!("mode=enabled. returning {ret_en}"));
            return ret_en;
        }
        _ => {}
    }

    if !info.dsname.is_empty() {
        let dsname = info.dsname.clone();
        info.log
            .debug(1, &format!("datasource '{dsname}' specified."));
        info.found(&[dsname], &[]);
        return 0;
    }

    if info.manual_clean_and_existing() {
        info.log.debug(
            1,
            "manual_cache_clean enabled. Not writing datasource_list.",
        );
        info.write_result(&["# manual_cache_clean.".to_owned()]);
        return 0;
    }

    let dslist: Vec<String> = split_words(&info.dslist)
        .into_iter()
        .map(str::to_owned)
        .collect();
    if dslist.len() == 1
        || (dslist.len() == 2 && dslist.get(1).map(String::as_str) == Some("None"))
    {
        let all = info.dslist.clone();
        info.log.debug(
            1,
            &format!("single entry in datasource_list ({all}) use that."),
        );
        if dslist.len() == 1 {
            let only = dslist.first().cloned().unwrap_or_default();
            // Note: no `None` is appended here, unlike every other path.
            info.write_result(&[format!("datasource_list: [ {only} ]")]);
        } else {
            info.found(&dslist, &[]);
        }
        return 0;
    }

    let (mut found, mut maybe) = (Vec::new(), Vec::new());
    let (mut exfound_cfg, mut exmaybe_cfg) = (String::new(), String::new());
    for ds in &dslist {
        info.log.debug(
            2,
            &format!("Checking for datasource '{ds}' via 'dscheck_{ds}'"),
        );
        info.excfg.clear();
        let Some(result) = checks::dscheck(info, ds) else {
            info.log.warn(&format!(
                "No check method 'dscheck_{ds}' for datasource '{ds}'"
            ));
            continue;
        };
        match result {
            DsCheck::Found => {
                info.log
                    .debug(1, &format!("check for '{ds}' returned found"));
                append_excfg(&mut exfound_cfg, &info.excfg);
                found.push(ds.clone());
            }
            DsCheck::Maybe => {
                info.log
                    .debug(1, &format!("check for '{ds}' returned maybe"));
                append_excfg(&mut exmaybe_cfg, &info.excfg);
                maybe.push(ds.clone());
            }
            DsCheck::NotFound => {
                info.log
                    .debug(2, &format!("check for '{ds}' returned not-found[1]"));
            }
        }
    }

    info.log.debug(
        2,
        &format!("found={} maybe={}", found.join(" "), maybe.join(" ")),
    );

    if !found.is_empty() {
        if found.len() == 1 {
            let one = found.first().cloned().unwrap_or_default();
            info.log
                .debug(1, &format!("Found single datasource: {one}"));
        } else {
            info.log.debug(
                1,
                &format!(
                    "Found {} datasources found={}: {}",
                    found.len(),
                    info.policy.found,
                    found.join(" ")
                ),
            );
            if info.policy.found == "first" {
                found.truncate(1);
            }
        }
        let extra = vec![exfound_cfg];
        info.found(&found, &extra);
        return 0;
    }

    if !maybe.is_empty() && info.policy.maybe != "none" {
        info.log.debug(
            1,
            &format!(
                "{} datasources returned maybe: {}",
                maybe.len(),
                maybe.join(" ")
            ),
        );
        let extra = vec![exmaybe_cfg];
        info.found(&maybe, &extra);
        return 0;
    }

    info.record_notfound();

    let basemsg = format!(
        "No ds found [mode={}, notfound={}].",
        info.policy.mode, info.policy.notfound
    );
    let (msg, ret) = match (info.policy.mode.as_str(), info.policy.notfound.as_str()) {
        ("report", "disabled") => (
            format!("{basemsg} Would disable cloud-init [{ret_dis}]"),
            ret_en,
        ),
        ("report", "enabled") => (
            format!("{basemsg} Would enable cloud-init [{ret_en}]"),
            ret_en,
        ),
        ("search", "disabled") => (
            format!("{basemsg} Disabled cloud-init [{ret_dis}]"),
            ret_dis,
        ),
        ("search", "enabled") => {
            (format!("{basemsg} Enabled cloud-init [{ret_en}]"), ret_en)
        }
        _ => {
            info.log.error("Unexpected result");
            (String::new(), 3)
        }
    };
    info.log.debug(1, &msg);
    ret
}

/// `main`: the cached-result short circuit around [`run_main`].
pub fn run(args: &[String]) -> i32 {
    read::ensure_sane_path();

    let mut paths = Paths::from_env();
    let mut log = Log::from_env();
    let (name, version, machine) = read::read_uname_info(&mut log);
    paths.set_run_path(&name);
    if log.is_unset() {
        log.set_path(&paths.run_ci.join("ds-identify.log"));
    }

    let mut info = Info::new(paths, log);
    info.uname_kernel_name = name;
    info.uname_kernel_version = version;
    info.uname_machine = machine;

    if !info.paths.run_ci.is_dir() {
        let _ = std::fs::create_dir_all(&info.paths.run_ci);
    }

    let forced = args.first().map(String::as_str) == Some("--force");
    if !forced && info.paths.run_ci_cfg.is_file() && info.paths.run_di_result.is_file()
    {
        if let Ok(text) = std::fs::read_to_string(&info.paths.run_di_result) {
            let cached = shell::read_line(&text);
            if matches!(cached.as_str(), "0" | "1" | "2") {
                info.log.debug(
                    2,
                    &format!("used cached result {cached}. pass --force to re-run."),
                );
                return cached.parse().unwrap_or(3);
            }
            info.log.debug(
                1,
                &format!("previous run returned unexpected '{cached}'. Re-running."),
            );
        } else {
            let path = info.paths.run_di_result.display().to_string();
            info.log
                .error(&format!("failed to read result from {path}!"));
        }
    }

    let ret = run_main(&mut info, args);
    let _ = std::fs::write(&info.paths.run_di_result, format!("{ret}\n"));
    let uptime = read::read_uptime(&info.paths);
    info.log
        .debug(1, &format!("[up {uptime}s] returning {ret}"));
    ret
}

/// `DI_MAIN=print_info`.
///
/// Note what this does *not* do: `print_info` calls `read_uname_info` and
/// `collect_info` only, so `DI_VIRT` and `DI_KERNEL_CMDLINE` are still empty
/// when the DMI and filesystem readers consult them. That is why `print_info`
/// reports `is_container=false` and runs `blkid` even inside a container.
pub fn run_print_info() -> i32 {
    let mut paths = Paths::from_env();
    let mut log = Log::from_env();
    let (name, version, machine) = read::read_uname_info(&mut log);
    paths.set_run_path(&name);
    let mut info = Info::new(paths, log);
    info.uname_kernel_name = name;
    info.uname_kernel_version = version;
    info.uname_machine = machine;
    info.collect();
    print!("{}", info.print_info());
    0
}
