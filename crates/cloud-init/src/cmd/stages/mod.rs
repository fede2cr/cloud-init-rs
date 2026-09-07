//! The boot stages: `cloud-init init` and `cloud-init modules`.
//!
//! Port of `cmd/main.main_init` and `cmd/main.main_modules`. The datasource
//! search and the instance directories are in place; consuming user data and
//! running modules are not, so a run that would reach them stops there.

mod network;
mod socket;
mod status_wrapper;

use std::path::PathBuf;

use ci_config::{Limits, Object};
use ci_core::semaphore::Frequency;
use ci_core::{Lookup, Paths};
use ci_datasource::{Context, Datasource, Dep, DsMode};
use ci_log::{Level, Logger};
use ci_report::events::{EventStack, Status};
use ci_report::Reporter;
use ci_userdata::handlers::Outcome as PartOutcome;
use clap::{Args, ValueEnum};

use status_wrapper::{Mode, Outcome};

/// Directories `_initial_subdirs` creates under `cloud_dir` and `run_dir`.
const CLOUD_SUBDIRS: [&str; 10] = [
    "scripts",
    "scripts/per-instance",
    "scripts/per-once",
    "scripts/per-boot",
    "scripts/vendor",
    "seed",
    "instances",
    "handlers",
    "sem",
    "data",
];

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Start in local mode (default: false).
    #[arg(long, short = 'l')]
    pub local: bool,

    /// Use additional yaml configuration files.
    #[arg(long = "file", short = 'f', value_name = "FILES")]
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ModulesArgs {
    /// Module configuration name to use (default: config).
    #[arg(long, short = 'm', value_enum, default_value_t = ModulesMode::Config)]
    pub mode: ModulesMode,

    /// Use additional yaml configuration files.
    #[arg(long = "file", short = 'f', value_name = "FILES")]
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ModulesMode {
    Init,
    Config,
    #[value(name = "final")]
    Final,
}

impl ModulesMode {
    fn name(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Config => "config",
            Self::Final => "final",
        }
    }
}

#[derive(Debug, Args)]
pub struct SingleArgs {
    /// Module name to run.
    #[arg(long, short = 'n')]
    pub name: String,

    /// Module frequency for this run.
    #[arg(long, value_enum)]
    pub frequency: Option<FreqShort>,

    /// Enable reporting.
    #[arg(long)]
    pub report: bool,

    /// Use additional yaml configuration files.
    #[arg(long = "file", short = 'f', value_name = "FILES")]
    pub files: Vec<PathBuf>,

    /// Any additional arguments to pass to this module.
    #[arg(value_name = "argument")]
    pub module_args: Vec<String>,
}

/// `main.FREQ_SHORT_NAMES`: the CLI spells the frequencies more briefly than
/// the config does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FreqShort {
    Instance,
    Always,
    Once,
}

impl FreqShort {
    fn frequency(self) -> Frequency {
        match self {
            Self::Instance => Frequency::Instance,
            Self::Always => Frequency::Always,
            Self::Once => Frequency::Once,
        }
    }
}

pub fn run_init(args: &InitArgs, force: bool) -> u8 {
    let mode = if args.local {
        Mode::InitLocal
    } else {
        Mode::Init
    };
    let (rname, rdesc) = if args.local {
        ("init-local", "searching for local datasources")
    } else {
        ("init-network", "searching for network datasources")
    };
    let mut reporter = Reporter::default();
    let mut stack = EventStack::new(rname, rdesc);
    // Nothing is configured to write yet, which is upstream's state too: only
    // the recoverable-error collector is attached until `setup_logging` runs.
    // The root scope's start event is lost with it, in both implementations.
    let mut logger = Logger::silent();
    stack.open(&mut reporter, &mut logger);
    // The wrapper resolves its own paths from the base config: upstream's
    // `read_cfg_paths` does not see `--file`.
    let code = status_wrapper::wrap(mode, &Paths::read(), &mut logger, |logger| {
        main_init(
            mode,
            &read_cfg(&args.files),
            &args.files,
            force,
            logger,
            &mut reporter,
            &mut stack,
        )
    });
    stack.close(&mut reporter, &mut logger);
    reporter.flush(&mut logger);
    logger.flush();
    code
}

pub fn run_modules(args: &ModulesArgs, force: bool) -> u8 {
    let mut reporter = Reporter::default();
    let mut stack = EventStack::new(
        &format!("modules-{}", args.mode.name()),
        &format!("running modules for {}", args.mode.name()),
    );
    let mut logger = Logger::silent();
    stack.open(&mut reporter, &mut logger);
    let mode = match args.mode {
        ModulesMode::Config => Some(Mode::ModulesConfig),
        ModulesMode::Final => Some(Mode::ModulesFinal),
        // `--mode=init` is not a boot stage, so upstream leaves it unwrapped.
        ModulesMode::Init => None,
    };
    let code = if let Some(mode) = mode {
        status_wrapper::wrap(mode, &Paths::read(), &mut logger, |logger| {
            main_modules(args, force, logger, &mut reporter, &mut stack)
        })
    } else {
        let errors =
            main_modules(args, force, &mut logger, &mut reporter, &mut stack).errors;
        u8::try_from(errors.len()).unwrap_or(u8::MAX)
    };
    stack.close(&mut reporter, &mut logger);
    reporter.flush(&mut logger);
    logger.flush();
    code
}

/// `main_single`: run one module by name, ignoring the section lists.
///
/// Not a boot stage, so it is not wrapped by `status_wrapper` and never touches
/// `status.json`. Reporting is off unless `--report` asks for it.
pub fn run_single(args: &SingleArgs, force: bool) -> u8 {
    let mut reporter = Reporter::default();
    let mut stack = EventStack::new(
        &format!("single/{}", args.name),
        &format!("running single module {}", args.name),
    )
    .with_reporting(args.report);
    let mut logger = Logger::silent();
    stack.open(&mut reporter, &mut logger);
    let code = main_single(args, force, &mut logger, &mut reporter, &mut stack);
    stack.close(&mut reporter, &mut logger);
    reporter.flush(&mut logger);
    logger.flush();
    code
}

fn main_single(
    args: &SingleArgs,
    force: bool,
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> u8 {
    // Stage 1
    let cfg = read_cfg(&args.files);
    let paths = Paths::from_config(&cfg);

    // Stage 2: a missing datasource is only maybe bad here, since it depends
    // on the module, so the run continues under `--force`.
    let distro = match distro(&cfg) {
        Ok(distro) => distro,
        Err(err) => {
            logger.error("main.py", &err);
            return 1;
        }
    };
    let datasource = check_cache("trust", &cfg, &paths, logger, reporter, parent);
    if datasource.is_none() {
        let msg = "Failed to fetch your datasource, likely bad things to come!";
        logger.warning("main.py", msg);
        eprintln!("{msg}");
        if !force {
            return 1;
        }
        // `_maybe_persist_instance_data` reaches for `init.datasource` and
        // upstream dies on the `RuntimeError` it raises (COMPAT.md B31).
        logger.error("main.py", DS_NOT_INITIALISED);
        return 1;
    }
    let Some(datasource) = datasource else {
        return 1;
    };
    maybe_persist_instance_data(&datasource, &cfg, &paths, logger);

    // Stage 3 and 4
    if !args.module_args.is_empty() {
        logger.debug(
            "main.py",
            &format!(
                "Using passed in arguments {}",
                ci_modules::repr_names(&args.module_args)
            ),
        );
    }
    if let Some(frequency) = args.frequency {
        logger.debug(
            "main.py",
            &format!(
                "Using passed in frequency {}",
                frequency.frequency().as_str()
            ),
        );
    }
    logger.reconfigure(&cfg);
    apply_reporting_cfg(&cfg, reporter, logger);
    welcome("single", logger);

    // Stage 5
    let mods = ci_modules::single(
        &args.name,
        &args.module_args,
        args.frequency.map(|f| f.frequency().as_str()),
        logger,
    );
    if mods.is_empty() {
        logger.warning(
            "main.py",
            &format!("Did not run {}, does it exist?", args.name),
        );
        return 1;
    }
    let failures = run_modules_list(
        &mods,
        &cfg,
        &paths,
        distro,
        Some(&datasource),
        logger,
        reporter,
        parent,
    );
    if failures.is_empty() {
        return 0;
    }
    logger.warning("main.py", &format!("Ran {} but it failed!", args.name));
    1
}

/// `main.all_stages`: run every boot stage in one process, gated by the socket
/// protocol `cloud-init-main.service` and its three siblings speak.
///
/// The per-stage argument sets are rebuilt from scratch rather than inherited,
/// so `--force` and `--debug` on the outer invocation do not reach the stages.
pub fn run_all_stages() -> u8 {
    let mut logger = Logger::silent();
    logger.info("main.py", "Running cloud-init in single process mode.");
    // Bound before anything else, so a unit that starts early is queued rather
    // than refused.
    let mut sync =
        match socket::SocketSync::bind(&["local", "network", "config", "final"]) {
            Ok(sync) => sync,
            Err(err) => {
                eprintln!("cloud-init: could not bind the stage sockets: {err}");
                return 1;
            }
        };
    socket::sd_notify("READY=1", &mut logger);

    let local = InitArgs {
        local: true,
        files: Vec::new(),
    };
    let network = InitArgs {
        local: false,
        files: Vec::new(),
    };
    sync.stage("local", &mut logger, |_| run_init(&local, false));
    sync.stage("network", &mut logger, |_| run_init(&network, false));
    for (name, mode) in [
        ("config", ModulesMode::Config),
        ("final", ModulesMode::Final),
    ] {
        let args = ModulesArgs {
            mode,
            files: Vec::new(),
        };
        sync.stage(name, &mut logger, |_| run_modules(&args, false));
    }
    let code = sync.finish(&mut logger);
    logger.flush();
    code
}

/// `Init.read_cfg`: `--file` configs outrank everything the system supplies.
fn read_cfg(files: &[PathBuf]) -> Object {
    ci_config::merger::read_cfg(files, ci_config::Limits::default())
}

#[allow(clippy::too_many_lines)]
fn main_init(
    mode: Mode,
    cfg: &Object,
    files: &[PathBuf],
    force: bool,
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> Outcome {
    let paths = Paths::from_config(cfg);
    logger.reconfigure(cfg);
    apply_reporting_cfg(cfg, reporter, logger);
    welcome(mode.bootstage_name(), logger);
    initialize_filesystem(&paths);

    if mode == Mode::InitLocal {
        // `purge_cache`: the boot-finished marker, so a re-run is not mistaken
        // for a completed boot.
        let _ = std::fs::remove_file(paths.boot_finished());
    }

    // `features.MANUAL_NETWORK_WAIT` is on and has no config switch, so the
    // network stage waits for connectivity unless the local stage decided
    // nothing left to run needs it. The wait resolves the distro, which is why
    // an unusable `system_info.distro` fails here rather than at the datasource
    // search the way it does in the local stage.
    let (existing, resolved) = if mode == Mode::InitLocal {
        (cache_trust(cfg, &paths, logger), distro(cfg))
    } else {
        let resolved = distro(cfg);
        if !paths.run_path(Lookup::SkipNetwork).exists() {
            logger.debug(
                "main.py",
                "Will wait for network connectivity before continuing",
            );
            // Upstream logs first and resolves the distro as it calls, so an
            // unusable one still leaves the line above in the log.
            if let Ok(distro) = &resolved {
                network::wait_for_network(distro, &system_info(cfg), logger);
            }
        }
        ("trust", resolved)
    };
    let distro = match resolved {
        Ok(distro) => distro,
        Err(err) => {
            return Outcome {
                datasource: None,
                errors: vec![err],
            }
        }
    };
    let cached = check_cache(existing, cfg, &paths, logger, reporter, parent);

    let dsmode = if mode == Mode::InitLocal {
        DsMode::Local
    } else {
        DsMode::Network
    };
    let ds_restored = cached.is_some();
    let Some(datasource) =
        cached.or_else(|| fetch(mode, cfg, &paths, logger, reporter, parent))
    else {
        // `check_if_fallback_is_allowed` is false for every datasource in this
        // build, so the link goes before the search gives up.
        if existing == "check" {
            let _ = std::fs::remove_file(paths.instance_link());
        }
        return no_datasource(mode, dsmode, force, cfg, &paths, distro, logger);
    };
    if ds_restored {
        maybe_persist_instance_data(&datasource, cfg, &paths, logger);
    }

    // The network stage stands down if the local stage already did the work.
    if mode == Mode::Init && datasource.dsmode != DsMode::Network {
        logger.debug(
            "main.py",
            &format!("[{dsmode}] Exiting. datasource {datasource} in local mode"),
        );
        return Outcome::default();
    }

    // Stage 6
    let reflected = match ci_core::instance::reflect(
        &paths,
        &datasource.instance_id,
        &datasource.record(),
        cfg,
    ) {
        Ok(reflected) => reflected,
        Err(err) => {
            return Outcome {
                datasource: Some(datasource.to_string()),
                errors: vec![err.to_string()],
            }
        }
    };
    write_to_cache(&datasource, cfg, &paths, logger);
    logger.debug(
        "main.py",
        &format!(
            "[{dsmode}] init will now be targeting instance id: {}. new={}",
            reflected.iid,
            if reflected.is_new_instance() {
                "True"
            } else {
                "False"
            },
        ),
    );

    // Upstream applies the network config before the local-mode exit, so a
    // datasource that only runs in the network stage still gets its config
    // written by the local one.
    if let Err(err) = apply_network_config(
        mode,
        cfg,
        &paths,
        distro,
        Some(&datasource),
        reflected.is_new_instance(),
        logger,
    ) {
        return Outcome {
            datasource: Some(datasource.to_string()),
            errors: vec![err],
        };
    }

    if mode == Mode::InitLocal {
        write_skip_network(Some(&datasource), &paths, logger);
    }

    if mode == Mode::InitLocal && datasource.dsmode != DsMode::Local {
        logger.debug(
            "main.py",
            &format!("[{dsmode}] Exiting. datasource {datasource} not in local mode."),
        );
        return Outcome {
            datasource: Some(datasource.to_string()),
            errors: Vec::new(),
        };
    }

    // Stage 7
    if let Err(err) =
        stage_seven(&datasource, &paths, cfg, files, logger, reporter, parent)
    {
        return Outcome {
            datasource: Some(datasource.to_string()),
            errors: vec![err],
        };
    }

    // Stage 8: the module config is re-read, so the cloud-config the handlers
    // just wrote — including anything the tenant sent — is in force from here.
    let modcfg = read_cfg(files);
    // Stage 9: `loggers.setup_logging(mods.cfg)`.
    logger.reconfigure(&modcfg);
    // Stage 10
    // `Modules` wraps the same `Init`, so the distro is the one resolved
    // above, not one re-read out of `modcfg`.
    let errors = run_module_section(
        "init",
        &modcfg,
        &paths,
        distro,
        Some(&datasource),
        logger,
        reporter,
        parent,
    );
    Outcome {
        datasource: Some(datasource.to_string()),
        errors,
    }
}

/// `cmd/main.py::run_module_section`, plus the parts of `Modules._run_modules`
/// that are not the module itself.
///
/// Every selected module is resolved, reported and semaphored exactly as it
/// would be upstream. Whether it then *does* anything depends on whether it
/// has been ported yet (COMPAT.md deviation 86); the semaphore is written
/// either way, so a module recorded here is one a later Python run would
/// consider done.
#[allow(clippy::too_many_arguments)]
fn run_module_section(
    action: &str,
    cfg: &Object,
    paths: &Paths,
    distro: &'static ci_distro::Distro,
    datasource: Option<&Datasource>,
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> Vec<String> {
    let section = format!("cloud_{action}_modules");
    let mods = ci_modules::section(cfg, &section, distro.name, logger);
    let failures = run_modules_list(
        &mods, cfg, paths, distro, datasource, logger, reporter, parent,
    );

    if mods.is_empty() {
        let msg = format!("No '{action}' modules to run under section '{section}'");
        eprintln!("{msg}");
        logger.debug("main.py", &msg);
        return Vec::new();
    }
    // `which_ran` holds every module attempted, failures included, so this
    // line reports the total rather than the successes. Upstream's count.
    logger.debug(
        "main.py",
        &format!(
            "Ran {} modules with {} failures",
            mods.len(),
            failures.len()
        ),
    );
    failures
}

/// `Modules._run_modules`.
#[allow(clippy::too_many_arguments)]
fn run_modules_list(
    mods: &[ci_modules::Details],
    cfg: &Object,
    paths: &Paths,
    distro: &'static ci_distro::Distro,
    datasource: Option<&Datasource>,
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> Vec<String> {
    let instance_id = datasource.map(|ds| ds.instance_id.as_str());
    let runners = ci_core::semaphore::Runners::new(
        paths.clone(),
        instance_id.map(ToOwned::to_owned),
    );
    // `Init.distro` reassigns `datasource.sys_cfg = self.cfg`, and `Modules.cfg`
    // is the same `_extract_cfg("restricted")`: nobody below this point sees
    // `system_info` through the config. It travels separately, because it is
    // where `default_user` lives and that is the account the boot creates.
    let mut restricted = cfg.clone();
    let system_info = restricted
        .shift_remove("system_info")
        .and_then(|v| match v {
            ci_config::Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();
    let public_keys = datasource.map_or_else(Vec::new, |ds| ds.public_ssh_keys(logger));
    let module_datasource = datasource.map(|ds| ci_modules::Datasource {
        class_name: ds.class_name,
        dsname: ds.dsname,
        instance_id: &ds.instance_id,
        metadata: &ds.metadata,
        sys_cfg: &restricted,
        public_keys: &public_keys,
    });
    let mut failures = Vec::new();
    for details in mods {
        logger.debug("modules.py", &ci_modules::running_message(details));
        let name = ci_modules::run_name(&details.name);
        let mut event =
            parent.child(&name, &ci_modules::description(&name, details.frequency));
        event.open(reporter, logger);
        let started = std::time::Instant::now();
        let outcome = runners.run(&name, details.frequency, false, || {
            // A module with no body yet still takes the semaphore. That is
            // the whole point of recording it: the boot it ran in is one a
            // Python cloud-init would not repeat.
            let Some(handler) = ci_modules::handler(details.module.name) else {
                return Ok(());
            };
            let mut args = ci_modules::Args {
                name: &details.name,
                cfg: &restricted,
                system_info: &system_info,
                args: &details.args,
                paths,
                root: std::path::Path::new("/"),
                distro,
                datasource: module_datasource,
                logger,
            };
            handler(&mut args)
        });
        match outcome {
            // Upstream formats the elapsed time to three decimals.
            Ok(Some(())) => {
                event.set_message(format!(
                    "{name} ran successfully and took {:.3} seconds",
                    started.elapsed().as_secs_f64()
                ));
            }
            Ok(None) => event.set_message(format!("{name} previously ran")),
            Err(err) => {
                logger.warning(
                    "util.py",
                    &format!(
                        "Running module {} ({}) failed\n{err}",
                        details.name, details.module.name
                    ),
                );
                event.set_result(Status::Fail);
                failures.push(err);
            }
        }
        let result = event.close(reporter, logger);
        parent.record_child(result);
    }
    failures
}

/// `Init.distro`, which every stage resolves on its way to a datasource.
///
/// Upstream lets `distros.fetch`'s `ImportError` escape `_get_data_source`, so
/// an unknown `system_info.distro` ends the stage with that one sentence
/// recorded against it and nothing else run — including the "no datasource"
/// path, which is reached later. `builtin.rs` always supplies a name, so the
/// `unwrap_or` is upstream's own default rather than a guess.
fn distro(cfg: &Object) -> Result<&'static ci_distro::Distro, String> {
    let name = cfg
        .get("system_info")
        .and_then(|info| info.get("distro"))
        .and_then(ci_config::Value::as_str)
        .unwrap_or("ubuntu");
    ci_distro::fetch(name).ok_or_else(|| {
        format!(
            "No distribution found for distro {name} \
             (searched ['{name}', 'cloudinit.distros.{name}'])"
        )
    })
}

/// `main_init`'s stage 7: set the datasource up, persist the blobs, consume
/// them.
#[allow(clippy::too_many_arguments)]
fn stage_seven(
    datasource: &Datasource,
    paths: &Paths,
    cfg: &Object,
    files: &[PathBuf],
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> Result<(), String> {
    setup_datasource(logger, reporter, parent);
    update(datasource, paths)?;
    let runners = ci_core::semaphore::Runners::new(
        paths.clone(),
        Some(datasource.instance_id.clone()),
    );
    run_consume_data(
        &runners, datasource, paths, cfg, files, logger, reporter, parent,
    )
    .map_err(|err| {
        logger.warning("util.py", &format!("Consuming user data failed!\n{err}"));
        "Consuming user data failed!".to_owned()
    })
}

/// `init.cloudify().run("consume_data", ..., freq=PER_INSTANCE)`.
#[allow(clippy::too_many_arguments)]
fn run_consume_data(
    runners: &ci_core::semaphore::Runners,
    datasource: &Datasource,
    paths: &Paths,
    cfg: &Object,
    files: &[PathBuf],
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> Result<(), String> {
    let ran = runners.run("consume_data", Frequency::Instance, false, || {
        consume_data(
            Frequency::Instance,
            datasource,
            paths,
            cfg,
            files,
            logger,
            reporter,
            parent,
        )
    })?;
    if ran.is_some() {
        return Ok(());
    }
    // Nothing ran per-instance, so give the per-always handlers their turn.
    // See https://bugs.launchpad.net/bugs/819507.
    consume_data(
        Frequency::Always,
        datasource,
        paths,
        cfg,
        files,
        logger,
        reporter,
        parent,
    )
}

/// One `events.ReportEventStack` scope. An error inside fails the scope and
/// then keeps travelling, as the raised exception does upstream.
fn scope(
    parent: &mut EventStack,
    reporter: &mut Reporter,
    logger: &mut Logger,
    name: &str,
    description: &str,
    body: impl FnOnce(&mut Logger) -> Result<(), String>,
) -> Result<(), String> {
    let mut event = parent.child(name, description);
    event.open(reporter, logger);
    let outcome = body(logger);
    if outcome.is_err() {
        event.set_result(Status::Fail);
    }
    let result = event.close(reporter, logger);
    parent.record_child(result);
    outcome
}

/// `Init.consume_data`.
#[allow(clippy::too_many_arguments)]
fn consume_data(
    frequency: Frequency,
    datasource: &Datasource,
    paths: &Paths,
    cfg: &Object,
    files: &[PathBuf],
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> Result<(), String> {
    scope(
        parent,
        reporter,
        logger,
        "consume-user-data",
        "reading and applying user-data",
        |logger| {
            if ci_config::option::get_bool(cfg, "allow_userdata", true) {
                consume_userdata(frequency, datasource, paths, logger)
            } else {
                logger
                    .debug("stages.py", "allow_userdata = False: discarding user-data");
                Ok(())
            }
        },
    )?;
    for (source, blob, set) in [
        (
            "vendordata",
            datasource.vendordata_raw.as_deref(),
            ci_userdata::handlers::Set::vendor_data as fn() -> _,
        ),
        (
            "vendordata2",
            datasource.vendordata2_raw.as_deref(),
            ci_userdata::handlers::Set::vendor_data2 as fn() -> _,
        ),
    ] {
        let (event_name, description) = if source == "vendordata" {
            ("consume-vendor-data", "reading and applying vendor-data")
        } else {
            ("consume-vendor-data2", "reading and applying vendor-data2")
        };
        scope(
            parent,
            reporter,
            logger,
            event_name,
            description,
            |logger| {
                consume_vendordata(source, blob, set, frequency, paths, cfg, logger)
            },
        )?;
    }

    // `_reset`: the cloud-config the handlers just wrote is part of the config
    // from here on.
    let merged = read_cfg(files);
    let mut combined = restricted(&merged);
    combined.insert("_doc".to_owned(), COMBINED_CLOUD_CONFIG_DOC.into());
    combined.insert(
        "system_info".to_owned(),
        merged
            .get("system_info")
            .cloned()
            .unwrap_or_else(|| ci_config::Value::Object(ci_config::Object::new())),
    );
    combined.insert("features".to_owned(), features_object());
    write_json(&paths.run_path(Lookup::CombinedCloudConfig), &combined)?;

    let sensitive_at = paths.run_path(Lookup::InstanceDataSensitive);
    let text = match std::fs::read_to_string(&sensitive_at) {
        Ok(text) => text,
        Err(e) => {
            logger.warning(
                "stages.py",
                &format!(
                    "Skipping write of system_info/features to {}. Unable to read file: {e}",
                    sensitive_at.display()
                ),
            );
            return Ok(());
        }
    };
    let Ok(ci_config::Value::Object(mut instance_json)) =
        serde_json::from_str::<ci_config::Value>(&text)
    else {
        logger.warning(
            "stages.py",
            &format!(
                "Skipping write of system_info/features to {}. Invalid JSON found",
                sensitive_at.display()
            ),
        );
        return Ok(());
    };
    for key in ["system_info", "features"] {
        if let Some(value) = combined.get(key) {
            instance_json.insert(key.to_owned(), value.clone());
        }
    }
    write_json(&sensitive_at, &instance_json)
}

/// `COMBINED_CLOUD_CONFIG_DOC`.
const COMBINED_CLOUD_CONFIG_DOC: &str =
    "Aggregated cloud-config created by merging merged_system_cfg \
     (/etc/cloud/cloud.cfg and /etc/cloud/cloud.cfg.d), metadata, vendordata \
     and userdata. The combined_cloud_config represents the aggregated \
     desired configuration acted upon by cloud-init.";

/// `Init.cfg`, which is `_extract_cfg("restricted")`.
fn restricted(cfg: &Object) -> Object {
    let mut out = cfg.clone();
    out.remove("system_info");
    out
}

/// `features.get_features()`: every uppercase module attribute.
fn features_object() -> ci_config::Value {
    let mut out = ci_config::Object::new();
    for name in ci_core::features::ALL_FEATURES {
        out.insert((*name).to_owned(), ci_config::Value::Bool(true));
    }
    ci_config::Value::Object(out)
}

fn write_json(path: &std::path::Path, value: &Object) -> Result<(), String> {
    let mut text = ci_core::dumps_indent(&ci_config::Value::Object(value.clone()), 1);
    text.push('\n');
    store(path, text.as_bytes())
}

/// `Init._consume_userdata`.
fn consume_userdata(
    frequency: Frequency,
    datasource: &Datasource,
    paths: &Paths,
    logger: &mut Logger,
) -> Result<(), String> {
    let blob = datasource.userdata_raw.as_deref().unwrap_or_default();
    do_handlers(
        ci_userdata::handlers::Set::user_data(),
        blob,
        frequency,
        datasource,
        paths,
        logger,
    )
}

/// `Init._consume_vendordata`.
fn consume_vendordata(
    source: &str,
    blob: Option<&[u8]>,
    set: fn() -> ci_userdata::handlers::Set,
    frequency: Frequency,
    paths: &Paths,
    cfg: &Object,
    logger: &mut Logger,
) -> Result<(), String> {
    let Some(blob) = blob.filter(|b| !b.is_empty()) else {
        logger.debug("stages.py", &format!("no {source} from datasource"));
        return Ok(());
    };
    let cfg_name = if source == "vendordata" {
        "vendor_data"
    } else {
        "vendor_data2"
    };
    let merged =
        ci_config::merger::merge_over(cfg.clone(), &paths.cloud_dir, Limits::default());
    let vdcfg = match merged.get(cfg_name) {
        Some(ci_config::Value::Object(map)) => map.clone(),
        None => ci_config::Object::new(),
        Some(_) => {
            logger.warning(
                "stages.py",
                &format!(
                    "invalid {cfg_name} setting. resetting to: {{'enabled': False}}"
                ),
            );
            let mut map = ci_config::Object::new();
            map.insert("enabled".to_owned(), ci_config::Value::Bool(false));
            map
        }
    };
    let enabled = vdcfg.get("enabled");
    let excluded = vdcfg
        .get("disabled_handlers")
        .and_then(ci_config::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        });
    if !enabled.is_some_and(ci_config::option::is_true) {
        logger.debug("stages.py", &format!("{source} consumption is disabled."));
        return Ok(());
    }
    if let Some(text) = enabled.and_then(ci_config::Value::as_str) {
        // `lifecycle.deprecate` with the default five-year schedule.
        logger.log(
            Level::Deprecated,
            "lifecycle.py",
            &format!(
                "Use of string '{text}' for 'vendor_data:enabled' field is \
                 deprecated in 23.1 and scheduled to be removed in 28.1. Use \
                 boolean value instead."
            ),
        );
    }
    logger.debug(
        "stages.py",
        &format!(
            "{source} will be consumed. disabled_handlers={}",
            match &excluded {
                None => "None".to_owned(),
                Some(list) => format!("[{}]", list.join(", ")),
            }
        ),
    );
    let set = match excluded {
        Some(types) => set().excluding(types),
        None => set(),
    };
    do_handlers_blob(set, blob, frequency, paths, None, logger)
}

/// `Init._do_handlers`, minus the custom handler modules.
fn do_handlers(
    set: ci_userdata::handlers::Set,
    blob: &[u8],
    frequency: Frequency,
    datasource: &Datasource,
    paths: &Paths,
    logger: &mut Logger,
) -> Result<(), String> {
    do_handlers_blob(
        set,
        blob,
        frequency,
        paths,
        Some(datasource.instance_id.clone()),
        logger,
    )
}

fn do_handlers_blob(
    mut set: ci_userdata::handlers::Set,
    blob: &[u8],
    frequency: Frequency,
    paths: &Paths,
    instance_id: Option<String>,
    logger: &mut Logger,
) -> Result<(), String> {
    // `PER_ALWAYS` never reaches a handler: every default handler is
    // `PER_INSTANCE`, so `run_part` returns before calling one.
    if frequency != Frequency::Instance {
        return Ok(());
    }
    let processed = ci_userdata::process(blob, paths).map_err(|e| e.to_string())?;
    let ctx = ci_userdata::handlers::Context {
        paths: paths.clone(),
        instance_id,
    };
    set.start();
    for part in &processed.parts {
        match set.handle(&ctx, part) {
            PartOutcome::Handled => {}
            PartOutcome::Excluded => logger.debug(
                "handlers/__init__.py",
                &format!("content_type \"{}\" is excluded", part.content_type),
            ),
            PartOutcome::Unhandled(None) => logger.debug(
                "handlers/__init__.py",
                &format!("Empty payload of type {}", part.content_type),
            ),
            PartOutcome::Unhandled(Some(head)) => {
                let kind = if part.content_type == "text/x-not-multipart" {
                    "non-multipart"
                } else {
                    "unknown content-type"
                };
                logger.warning(
                    "handlers/__init__.py",
                    &format!(
                        "Unhandled {kind} ({}) userdata: '{head}...'",
                        part.content_type
                    ),
                );
            }
            PartOutcome::Failed(err) => logger.warning(
                "util.py",
                &format!("Failed at un-mangling part {}\n{err}", part.filename),
            ),
        }
    }
    set.finish(&ctx).map_err(|e| e.to_string())
}

/// `Init.setup_datasource`.
fn setup_datasource(
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) {
    let mut event = parent.child("setup-datasource", "setting up datasource");
    event.open(reporter, logger);
    // `DataSource.setup` is a no-op, and neither ported datasource overrides
    // it, so the scope is all there is to reproduce.
    let result = event.close(reporter, logger);
    parent.record_child(result);
}

/// `Init.update`: the raw and the processed form of each of the three blobs.
///
/// The `.i` files hold `str(MIMEMultipart)`, so their random boundary differs
/// between runs on both sides.
fn update(datasource: &Datasource, paths: &Paths) -> Result<(), String> {
    for (raw_at, processed_at, blob) in [
        (
            Lookup::UserDataRaw,
            Lookup::UserData,
            datasource.userdata_raw.as_deref(),
        ),
        (
            Lookup::VendorDataRaw,
            Lookup::VendorData,
            datasource.vendordata_raw.as_deref(),
        ),
        (
            Lookup::VendorData2Raw,
            Lookup::VendorData2,
            datasource.vendordata2_raw.as_deref(),
        ),
    ] {
        let blob = blob.unwrap_or_default();
        store(&paths.instance_path(raw_at), blob)?;
        let processed = ci_userdata::process(blob, paths).map_err(|e| e.to_string())?;
        store(
            &paths.instance_path(processed_at),
            &processed.message.to_bytes(),
        )?;
    }
    Ok(())
}

fn store(path: &std::path::Path, data: &[u8]) -> Result<(), String> {
    ci_sys::atomic::write_file(path, data, ci_sys::WriteOptions::mode(0o600))
        .map_err(|e| e.to_string())
}

/// `Init.fetch`, as far as `_get_data_source`'s search half goes.
fn fetch(
    mode: Mode,
    cfg: &Object,
    paths: &Paths,
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> Option<Datasource> {
    let ds_deps: &[Dep] = if mode == Mode::InitLocal {
        &[Dep::Filesystem]
    } else {
        &[Dep::Filesystem, Dep::Network]
    };
    let cmdline = ci_config::cmdline::get_cmdline();
    // `Init.cfg` is `_extract_cfg("restricted")`: nobody below this point sees
    // `system_info`.
    let mut restricted = cfg.clone();
    restricted.remove("system_info");
    let mut ctx = Context {
        sys_cfg: &restricted,
        paths,
        cmdline: &cmdline,
        limits: ci_config::Limits::default(),
        logger,
        reporter,
    };
    let found = ci_datasource::find_source(
        &mut ctx,
        ds_deps,
        &ctx_datasource_list(cfg),
        parent,
    )
    .ok()?;
    // `_get_data_source` drops the link before `instancify` rebuilds it.
    let _ = std::fs::remove_file(paths.instance_link());
    logger.info(
        "stages.py",
        &format!("Loaded datasource {} - {found}", found.class_name),
    );
    Some(found)
}

/// `sys_cfg["datasource_list"]`, which `_get_datasources` reads.
fn ctx_datasource_list(cfg: &Object) -> Vec<String> {
    cfg.get("datasource_list")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Upstream's `DataSourceNotFoundException` path.
fn no_datasource(
    mode: Mode,
    dsmode: DsMode,
    force: bool,
    cfg: &Object,
    paths: &Paths,
    distro: &'static ci_distro::Distro,
    logger: &mut Logger,
) -> Outcome {
    if mode == Mode::InitLocal {
        logger.debug("main.py", "No local datasource found");
    } else {
        logger.warning(
            "main.py",
            "No instance datasource found! Likely bad things to come!",
        );
    }
    if force {
        logger.debug(
            "main.py",
            &format!("[{dsmode}] barreling on in force mode without datasource"),
        );
        // Upstream carries on to `instancify`, which needs an instance id that
        // only a datasource can supply, so `--force` always ends here
        // (COMPAT.md B31).
        logger.warning(
            "helpers.py",
            "No per instance data available, is there an datasource/iid set?",
        );
        return Outcome {
            datasource: None,
            errors: vec![NO_INSTANCE_DIR.to_owned()],
        };
    }
    // Upstream applies network config even here, so a machine with no
    // datasource still comes up on DHCP. A failure escapes as an exception and
    // the stage never reaches its own return.
    if let Err(err) =
        apply_network_config(mode, cfg, paths, distro, None, false, logger)
    {
        return Outcome {
            datasource: None,
            errors: vec![err],
        };
    }
    logger.debug("main.py", &format!("[{dsmode}] Exiting without datasource"));
    if mode == Mode::InitLocal {
        return Outcome::default();
    }
    Outcome {
        datasource: None,
        errors: vec!["No instance datasource found.".to_owned()],
    }
}

/// `init.apply_network_config(bring_up=_should_bring_up_interfaces(...))`.
fn apply_network_config(
    mode: Mode,
    cfg: &Object,
    paths: &Paths,
    distro: &'static ci_distro::Distro,
    datasource: Option<&Datasource>,
    is_new_instance: bool,
    logger: &mut Logger,
) -> Result<(), String> {
    let local = mode == Mode::InitLocal;
    let cmdline = ci_config::cmdline::get_cmdline();
    let system_info = system_info(cfg);
    network::apply_network_config(
        &network::Request {
            bring_up: network::should_bring_up_interfaces(cfg, local),
            cfg,
            system_info: &system_info,
            paths,
            distro,
            datasource,
            is_new_instance,
            cmdline: &cmdline,
            run_root: std::path::Path::new("/run"),
        },
        logger,
    )
}

/// `Init._extract_cfg("system")`, which is what the distro object carries.
fn system_info(cfg: &Object) -> Object {
    cfg.get("system_info")
        .and_then(ci_config::Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// `_should_wait_via_user_data`: could this blob need the network to be up?
///
/// The reason comes back even when the answer is no, because the caller logs
/// all three of them together in that case.
///
/// Upstream indexes and calls `.get` on whatever the keys below hold, so a
/// `write_files` that is not a list of mappings, or a `random_seed` that is not
/// a mapping, aborts the local stage with a `TypeError` or an `AttributeError`.
/// Malformed shapes simply do not match here — see COMPAT.md deviation 126.
fn should_wait_via_user_data(
    raw: Option<&[u8]>,
    logger: &mut Logger,
) -> (bool, String) {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return (false, "no configuration found".to_owned());
    };

    // Only the header is sniffed, because the blob may be an arbitrarily large
    // gzip, and `#cloud-config-archive` has to be told apart from
    // `#cloud-config` by more than its first few bytes.
    let trimmed = raw.trim_ascii();
    let head = trimmed.get(..42).unwrap_or(trimmed);
    if ci_userdata::types::type_from_starts_with(head, None)
        != Some("text/cloud-config")
    {
        return (true, "non-cloud-config user data found".to_owned());
    }

    let parsed = std::str::from_utf8(raw)
        .map_err(|err| err.to_string())
        .and_then(|text| {
            ci_config::yaml::load_yaml(text, ci_config::Limits::default())
                .map_err(|err| err.to_string())
        });
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(err) => {
            // `log_with_downgradable_level(version="24.4", WARNING)`, which the
            // "devel" deprecation boundary leaves at the requested level.
            logger.warning(
                "main.py",
                &format!("Unexpected failure parsing userdata: {err}"),
            );
            return (true, "failed to parse user data as yaml".to_owned());
        }
    };
    let Some(parsed) = parsed.as_object() else {
        return (true, "parsed config not in cloud-config format".to_owned());
    };

    // Each of these can reach for the network, so they all mean "wait".
    if let Some(entries) = parsed
        .get("write_files")
        .and_then(ci_config::Value::as_array)
    {
        for item in entries {
            let uri = item
                .get("source")
                .and_then(|source| source.get("uri"))
                .and_then(ci_config::Value::as_str)
                .unwrap_or("");
            if !uri.is_empty() && !uri.starts_with('/') && !uri.starts_with("file:") {
                return (true, "write_files with source uri found".to_owned());
            }
        }
    }
    if parsed
        .get("bootcmd")
        .is_some_and(ci_config::option::py_truthy)
    {
        return (true, "bootcmd found".to_owned());
    }
    if parsed
        .get("random_seed")
        .and_then(|seed| seed.get("command"))
        .is_some_and(ci_config::option::py_truthy)
    {
        return (true, "random_seed command found".to_owned());
    }
    if parsed
        .get("mounts")
        .is_some_and(ci_config::option::py_truthy)
    {
        return (true, "mounts found".to_owned());
    }
    (
        false,
        "cloud-config does not contain network requiring elements".to_owned(),
    )
}

/// `_should_wait_on_network`: may the network stage skip waiting for the
/// network?
///
/// The `None` arm is unreachable from `main_init`, which has already returned
/// by the time it would matter, but it is what upstream checks first.
fn should_wait_on_network(
    datasource: Option<&Datasource>,
    logger: &mut Logger,
) -> (bool, String) {
    let Some(datasource) = datasource else {
        return (true, "no datasource found".to_owned());
    };
    let (wait, user) =
        should_wait_via_user_data(datasource.userdata_raw.as_deref(), logger);
    if wait {
        return (true, format!("{user} in user data"));
    }
    let (wait, vendor) =
        should_wait_via_user_data(datasource.vendordata_raw.as_deref(), logger);
    if wait {
        return (true, format!("{vendor} in vendor data"));
    }
    let (wait, vendor2) =
        should_wait_via_user_data(datasource.vendordata2_raw.as_deref(), logger);
    if wait {
        return (true, format!("{vendor2} in vendor data2"));
    }
    (
        false,
        format!("user data: {user}, vendor data: {vendor}, vendor data2: {vendor2}"),
    )
}

/// The local stage's half of `features.MANUAL_NETWORK_WAIT`.
///
/// An absent `.skip-network` is what makes the network stage wait, so the file
/// is only ever written, never removed: `initialize_filesystem` does not create
/// `run_dir` afresh, but the marker lives there and `/run` is empty each boot.
fn write_skip_network(
    datasource: Option<&Datasource>,
    paths: &Paths,
    logger: &mut Logger,
) {
    let (should_wait, reason) = should_wait_on_network(datasource, logger);
    if should_wait {
        logger.debug(
            "main.py",
            &format!(
                "Network connectivity determined necessary for cloud-init's \
                 network stage. Reason: {reason}"
            ),
        );
        return;
    }
    logger.debug(
        "main.py",
        &format!(
            "Network connectivity determined unnecessary for cloud-init's \
             network stage. Reason: {reason}"
        ),
    );
    let marker = paths.run_path(Lookup::SkipNetwork);
    let _ = ci_sys::atomic::write_file(
        &marker,
        b"",
        ci_sys::WriteOptions::PUBLIC.volatile(),
    );
}

fn main_modules(
    args: &ModulesArgs,
    force: bool,
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> Outcome {
    // Stage 1
    let cfg = read_cfg(&args.files);
    let paths = Paths::from_config(&cfg);

    // Stage 2
    let distro = match distro(&cfg) {
        Ok(distro) => distro,
        Err(err) => {
            return Outcome {
                datasource: None,
                errors: vec![err],
            }
        }
    };
    let Some(datasource) = check_cache("trust", &cfg, &paths, logger, reporter, parent)
    else {
        // Upstream returns here, before it gets as far as configuring logging or
        // applying the `reporting` config, so neither is in force for this stage.
        let msg = format!(
            "Can not apply stage {}, no datasource found! Likely bad things to come!",
            args.mode.name()
        );
        logger.warning("main.py", &msg);
        eprintln!("{msg}");
        if force {
            // `Modules.cfg` needs the datasource, so upstream dies inside
            // `setup_logging` before logging or reporting are reconfigured
            // (COMPAT.md B31).
            return Outcome {
                datasource: None,
                errors: vec![DS_NOT_INITIALISED.to_owned()],
            };
        }
        return Outcome {
            datasource: None,
            errors: vec![msg],
        };
    };

    maybe_persist_instance_data(&datasource, &cfg, &paths, logger);

    // Stage 4
    logger.reconfigure(&cfg);
    apply_reporting_cfg(&cfg, reporter, logger);
    welcome(&format!("modules:{}", args.mode.name()), logger);

    // Stage 5
    let errors = run_module_section(
        args.mode.name(),
        &cfg,
        &paths,
        distro,
        Some(&datasource),
        logger,
        reporter,
        parent,
    );
    Outcome {
        datasource: Some(datasource.to_string()),
        errors,
    }
}

/// `_maybe_persist_instance_data`: a restored datasource never crawled, so the
/// files it would have written on the way may not be there.
fn maybe_persist_instance_data(
    datasource: &Datasource,
    cfg: &Object,
    paths: &Paths,
    logger: &mut Logger,
) {
    if paths.run_path(Lookup::InstanceData).exists() {
        return;
    }
    let cmdline = ci_config::cmdline::get_cmdline();
    let mut restricted = cfg.clone();
    restricted.remove("system_info");
    let mut reporter = Reporter::silent();
    let mut ctx = Context {
        sys_cfg: &restricted,
        paths,
        cmdline: &cmdline,
        limits: ci_config::Limits::default(),
        logger,
        reporter: &mut reporter,
    };
    if let Err(err) = ci_datasource::instance_data::persist(datasource, &mut ctx) {
        logger.warning(
            "__init__.py",
            &format!("Error persisting instance-data.json: {err}"),
        );
    }
}

/// Upstream's `RuntimeError` when `instancify` runs without a datasource.
const NO_INSTANCE_DIR: &str =
    "No instance directory is available. Has a datasource been fetched??";

/// Upstream's `RuntimeError` from the `Init.ds` property.
const DS_NOT_INITIALISED: &str = "Datasource is not initialized.";

/// `main_init`'s choice of `existing`: whether a cached datasource may be reused
/// without re-checking it against the running instance.
///
/// Only the local stage decides; the network stage always trusts what the local
/// stage left behind.
fn cache_trust(cfg: &Object, paths: &Paths, logger: &mut Logger) -> &'static str {
    if ci_config::option::get_bool(cfg, "manual_cache_clean", false) {
        logger.debug("main.py", "manual cache clean set from config");
        return "trust";
    }
    let marker = paths.instance_path(Lookup::ManualCleanMarker);
    if marker.exists() {
        logger.debug(
            "main.py",
            &format!("manual cache clean found from marker: {}", marker.display()),
        );
        return "trust";
    }
    "check"
}

/// `_get_data_source`'s cache probe, i.e. `_restore_from_checked_cache`.
fn check_cache(
    existing: &str,
    cfg: &Object,
    paths: &Paths,
    logger: &mut Logger,
    reporter: &mut Reporter,
    parent: &mut EventStack,
) -> Option<Datasource> {
    let mut cache = parent.child(
        "check-cache",
        &format!("attempting to read from cache [{existing}]"),
    );
    cache.open(reporter, logger);
    let (restored, description) =
        restore_from_checked_cache(existing, cfg, paths, logger);
    cache.set_description(description.clone());
    logger.debug("stages.py", &description);
    let result = cache.close(reporter, logger);
    parent.record_child(result);
    restored
}

/// `_restore_from_checked_cache`, returning the description upstream logs.
fn restore_from_checked_cache(
    existing: &str,
    cfg: &Object,
    paths: &Paths,
    logger: &mut Logger,
) -> (Option<Datasource>, String) {
    let Some(datasource) =
        ci_datasource::cache::load(&paths.instance_path(Lookup::ObjPkl))
    else {
        return (None, "no cache found".to_owned());
    };

    let run_iid = std::fs::read_to_string(paths.run_path(Lookup::InstanceId)).ok();
    if run_iid.as_deref().map(str::trim) == Some(datasource.instance_id.as_str()) {
        let message = format!("restored from cache with run check: {datasource}");
        return (Some(datasource), message);
    }
    if existing == "trust" {
        let message = format!("restored from cache: {datasource}");
        return (Some(datasource), message);
    }

    let cmdline = ci_config::cmdline::get_cmdline();
    let mut restricted = cfg.clone();
    restricted.remove("system_info");
    let mut reporter = Reporter::silent();
    let mut ctx = Context {
        sys_cfg: &restricted,
        paths,
        cmdline: &cmdline,
        limits: ci_config::Limits::default(),
        logger,
        reporter: &mut reporter,
    };
    let confirmed = ci_datasource::probe_for_class(datasource.class_name)
        .and_then(|probe| probe.check_instance_id(&mut ctx, &datasource.instance_id))
        .unwrap_or(false);
    if confirmed {
        let message = format!("restored from checked cache: {datasource}");
        (Some(datasource), message)
    } else {
        (None, format!("cache invalid in datasource: {datasource}"))
    }
}

/// `_write_to_cache`. A cache that cannot be written is upstream's `False`: it
/// is logged and the boot carries on.
fn write_to_cache(
    datasource: &Datasource,
    cfg: &Object,
    paths: &Paths,
    logger: &mut Logger,
) {
    if ci_config::option::get_bool(cfg, "manual_cache_clean", false) {
        let marker = paths.instance_path(Lookup::ManualCleanMarker);
        if let Err(err) = ci_sys::atomic::write_file(
            &marker,
            b"",
            ci_sys::atomic::WriteOptions::PUBLIC,
        ) {
            logger.warning(
                "stages.py",
                &format!("Failed writing {}: {err}", marker.display()),
            );
        }
    }
    if let Err(err) =
        ci_datasource::cache::store(datasource, &paths.instance_path(Lookup::ObjPkl))
    {
        logger.warning(
            "__init__.py",
            &format!("Failed pickling datasource {datasource}: {err}"),
        );
    }
}

/// `apply_reporting_cfg`. Upstream applies this after the stage's own start
/// event has already gone out, so that event only ever reaches the handlers the
/// previous configuration had registered.
fn apply_reporting_cfg(cfg: &Object, reporter: &mut Reporter, logger: &mut Logger) {
    match cfg.get("reporting") {
        Some(serde_json::Value::Object(reporting)) => {
            reporter.update_configuration(reporting, logger);
        }
        // Upstream calls `.items()` on whatever this is and dies (COMPAT.md B24).
        Some(other) if !other.is_null() => {
            eprintln!("Ignoring 'reporting': not a mapping: {other}");
        }
        _ => {}
    }
    seed_kvp_vm_id(reporter, logger);
}

/// `HyperVKvpReportingHandler.vm_id`'s DMI fallback. Upstream reads it lazily
/// inside the property, which `ci-report` cannot do — it sits below
/// `ci-datasource`, where `read_dmi_data` lives. Doing it here instead keeps
/// every event key carrying the real id, including the ones published before
/// the Azure datasource resolves it.
fn seed_kvp_vm_id(reporter: &mut Reporter, logger: &mut Logger) {
    let Some(handler) = reporter.kvp_handler() else {
        return;
    };
    if !handler.vm_id_is_unset() {
        return;
    }
    if let Some(uuid) = ci_datasource::dmi::read_dmi_data("system-uuid", logger) {
        if let Some(handler) = reporter.kvp_handler() {
            handler.set_vm_id(&uuid.to_lowercase());
        }
    }
}

/// `Init._initialize_filesystem`, minus the log-file ownership fix-up, which
/// belongs with logging in a later phase.
fn initialize_filesystem(paths: &Paths) {
    let _ = ci_sys::path::ensure_dir(&paths.cloud_dir, 0o755);
    for sub in CLOUD_SUBDIRS {
        let _ = ci_sys::path::ensure_dir(paths.cloud_dir.join(sub), 0o755);
    }
    let _ = ci_sys::path::ensure_dir(paths.run_dir.join("sem"), 0o755);
}

/// `welcome`, which upstream writes to stderr and the log.
fn welcome(action: &str, logger: &mut Logger) {
    let msg = format!(
        "Cloud-init v. {} running '{action}' at {}. Up {} seconds.",
        ci_core::version::version_string(),
        ci_core::time::format_last_update(ci_core::time::now_epoch()),
        ci_core::time::uptime(),
    );
    eprintln!("{msg}");
    // `%(filename)s` names the frame that called `log`, and upstream's welcome
    // goes through `multi_log`.
    logger.debug("log_util.py", &msg);
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

    fn config(text: &str) -> Object {
        match ci_config::load_yaml(text, ci_config::Limits::default()).unwrap() {
            serde_json::Value::Object(map) => map,
            other => panic!("not a mapping: {other}"),
        }
    }

    /// A datasource carrying the three blobs `Init.update` writes.
    fn datasource_with(user: &[u8], vendor: &[u8]) -> Datasource {
        Datasource {
            class_name: "DataSourceNoCloud",
            dsname: "NoCloud",
            dsmode: DsMode::Network,
            instance_id: "iid-update".to_owned(),
            metadata: Object::new(),
            userdata_raw: Some(user.to_vec()),
            vendordata_raw: Some(vendor.to_vec()),
            vendordata2_raw: None,
            network_config: None,
            platform_type: "nocloud".to_owned(),
            subplatform: "seed-dir (/none)".to_owned(),
            cloud_name_default: "unknown".to_owned(),
            detail: String::new(),
        }
    }

    /// A datasource whose user data both writes a script and configures
    /// vendor data, so one run exercises the whole of `consume_data`.
    #[test]
    fn consume_data_runs_the_handlers_and_writes_the_combined_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: dir.path().join("var"),
            run_dir: dir.path().join("run"),
            ..Paths::default()
        };
        std::fs::create_dir_all(paths.instance_link()).unwrap();
        std::fs::create_dir_all(&paths.run_dir).unwrap();
        let ds = datasource_with(
            b"#cloud-config\nvendor_data:\n  enabled: false\n",
            b"#!/bin/sh\necho vendor\n",
        );

        consume_data(
            Frequency::Instance,
            &ds,
            &paths,
            &Object::new(),
            &[],
            &mut Logger::silent(),
            &mut Reporter::silent(),
            &mut EventStack::new("init-network", "d"),
        )
        .unwrap();

        let combined =
            std::fs::read_to_string(paths.run_path(Lookup::CombinedCloudConfig))
                .unwrap();
        assert!(combined.contains("\"_doc\""));
        assert!(combined.contains("\"features\""));
        // The cloud-config part reached the handler.
        assert!(
            std::fs::read_to_string(paths.instance_path(Lookup::CloudConfig))
                .unwrap()
                .contains("vendor_data")
        );
        // ... and it turned vendor data off, so no vendor script was written.
        assert!(!paths
            .instance_path(Lookup::VendorScripts)
            .join("part-001")
            .exists());
    }

    #[test]
    fn a_per_always_consume_data_reaches_no_handler() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: dir.path().join("var"),
            run_dir: dir.path().join("run"),
            ..Paths::default()
        };
        std::fs::create_dir_all(paths.instance_link()).unwrap();
        std::fs::create_dir_all(&paths.run_dir).unwrap();

        consume_data(
            Frequency::Always,
            &datasource_with(b"#cloud-config\nc: 1\n", b""),
            &paths,
            &Object::new(),
            &[],
            &mut Logger::silent(),
            &mut Reporter::silent(),
            &mut EventStack::new("init-network", "d"),
        )
        .unwrap();

        assert!(!paths.instance_path(Lookup::CloudConfig).exists());
    }

    #[test]
    fn update_writes_the_raw_and_processed_blobs() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: dir.path().join("var"),
            run_dir: dir.path().join("run"),
            ..Paths::default()
        };
        std::fs::create_dir_all(paths.instance_link()).unwrap();

        update(&datasource_with(b"#cloud-config\nc: 1\n", b""), &paths).unwrap();

        for name in [
            Lookup::UserDataRaw,
            Lookup::UserData,
            Lookup::VendorDataRaw,
            Lookup::VendorData,
            Lookup::VendorData2Raw,
            Lookup::VendorData2,
        ] {
            let path = paths.instance_path(name);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", path.display());
        }

        let raw = std::fs::read(paths.instance_path(Lookup::UserDataRaw)).unwrap();
        assert_eq!(raw, b"#cloud-config\nc: 1\n");
        // An absent blob still yields a message with one empty part.
        let empty = std::fs::read(paths.instance_path(Lookup::VendorData2Raw)).unwrap();
        assert!(empty.is_empty());

        let processed =
            std::fs::read_to_string(paths.instance_path(Lookup::UserData)).unwrap();
        assert!(processed.starts_with("Content-Type: multipart/mixed;"));
        assert!(processed.contains("Content-Type: text/cloud-config"));
        assert!(processed.contains("#cloud-config\nc: 1\n"));
    }

    #[test]
    fn initialize_creates_the_state_tree() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: dir.path().join("var"),
            run_dir: dir.path().join("run"),
            ..Paths::default()
        };

        initialize_filesystem(&paths);

        for sub in CLOUD_SUBDIRS {
            assert!(paths.cloud_dir.join(sub).is_dir(), "{sub} missing");
        }
        assert!(paths.run_dir.join("sem").is_dir());
    }

    #[test]
    fn the_local_stage_clears_the_boot_finished_marker() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(&format!(
            // `network: config: disabled` keeps the stage off this machine's
            // real `/etc/netplan`: the datasource path is what is under test.
            "network:\n  config: disabled\n\
             system_info:\n  paths:\n    cloud_dir: {}\n    run_dir: {}\n",
            dir.path().join("var").display(),
            dir.path().join("run").display()
        ));
        let paths = Paths::from_config(&cfg);
        std::fs::create_dir_all(paths.instance_link()).unwrap();
        std::fs::write(paths.boot_finished(), b"done").unwrap();

        let outcome = main_init(
            Mode::InitLocal,
            &cfg,
            &[],
            false,
            &mut Logger::silent(),
            &mut Reporter::silent(),
            &mut EventStack::new("init-local", "d"),
        );

        assert!(outcome.errors.is_empty());
        assert!(!paths.boot_finished().exists());
    }

    #[test]
    fn the_network_stage_reports_the_missing_datasource() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(&format!(
            "network:\n  config: disabled\n\
             system_info:\n  paths:\n    cloud_dir: {}\n    run_dir: {}\n",
            dir.path().join("var").display(),
            dir.path().join("run").display()
        ));

        let outcome = main_init(
            Mode::Init,
            &cfg,
            &[],
            false,
            &mut Logger::silent(),
            &mut Reporter::silent(),
            &mut EventStack::new("init-network", "d"),
        );

        assert_eq!(outcome.errors, vec!["No instance datasource found."]);
    }

    #[test]
    fn a_modules_stage_names_the_mode_it_could_not_apply() {
        // `main_modules` reads its config from `args.files`, and an empty list
        // means the host's /etc/cloud -- so on a machine that has a real
        // cached datasource this stage finds one and the assertion inverts.
        let dir = tempfile::tempdir().unwrap();
        let cfg_file = dir.path().join("isolate.cfg");
        std::fs::write(
            &cfg_file,
            format!(
                "network:\n  config: disabled\n\
                 system_info:\n  paths:\n    cloud_dir: {}\n    run_dir: {}\n",
                dir.path().join("var").display(),
                dir.path().join("run").display()
            ),
        )
        .unwrap();

        let args = ModulesArgs {
            mode: ModulesMode::Final,
            files: vec![cfg_file],
        };
        let outcome = main_modules(
            &args,
            false,
            &mut Logger::silent(),
            &mut Reporter::silent(),
            &mut EventStack::new("modules-final", "d"),
        );
        assert_eq!(
            outcome.errors,
            vec![
                "Can not apply stage final, no datasource found! \
                 Likely bad things to come!"
            ]
        );
    }

    fn wait_via(raw: &str) -> (bool, String) {
        should_wait_via_user_data(Some(raw.as_bytes()), &mut Logger::silent())
    }

    #[test]
    fn only_cloud_config_without_network_using_keys_lets_the_wait_be_skipped() {
        // Nothing to run, so nothing to wait for.
        assert_eq!(
            should_wait_via_user_data(None, &mut Logger::silent()),
            (false, "no configuration found".to_owned())
        );
        assert_eq!(
            should_wait_via_user_data(Some(b""), &mut Logger::silent()),
            (false, "no configuration found".to_owned())
        );
        assert_eq!(
            wait_via("#cloud-config\nhostname: h\n"),
            (
                false,
                "cloud-config does not contain network requiring elements".to_owned()
            )
        );

        // Anything that is not cloud-config is opaque, so assume the worst.
        assert_eq!(
            wait_via("#!/bin/sh\necho hi\n"),
            (true, "non-cloud-config user data found".to_owned())
        );
        // `#cloud-config-archive` shares a prefix with `#cloud-config` and is
        // still not cloud-config; the 42-byte header is long enough to tell.
        assert_eq!(
            wait_via("#cloud-config-archive\n- content: x\n"),
            (true, "non-cloud-config user data found".to_owned())
        );
        // The header is sniffed after stripping, as upstream does.
        assert_eq!(
            wait_via("\n\n  #cloud-config\nbootcmd: [true]\n"),
            (true, "bootcmd found".to_owned())
        );

        assert_eq!(
            wait_via("#cloud-config\n\ta: [\n"),
            (true, "failed to parse user data as yaml".to_owned())
        );
        assert_eq!(
            wait_via("#cloud-config\n"),
            (true, "parsed config not in cloud-config format".to_owned())
        );

        for (yaml, reason) in [
            ("bootcmd: [echo hi]", "bootcmd found"),
            ("mounts: [[vdb, /mnt]]", "mounts found"),
            (
                "random_seed: {command: [pollinate]}",
                "random_seed command found",
            ),
            (
                "write_files: [{path: /a, source: {uri: http://x/a}}]",
                "write_files with source uri found",
            ),
        ] {
            assert_eq!(
                wait_via(&format!("#cloud-config\n{yaml}\n")),
                (true, reason.to_owned()),
                "{yaml}"
            );
        }

        // Present but falsy, or local, so none of them needs the network.
        for yaml in [
            "bootcmd: []",
            "mounts: []",
            "random_seed: {file: /dev/urandom}",
            "random_seed: {command: []}",
            "write_files: [{path: /a, content: x}]",
            "write_files: [{path: /a, source: {uri: /b}}]",
            "write_files: [{path: /a, source: {uri: 'file:///b'}}]",
        ] {
            assert!(!wait_via(&format!("#cloud-config\n{yaml}\n")).0, "{yaml}");
        }
    }

    #[test]
    fn the_no_wait_reason_names_all_three_blobs() {
        let mut ds = datasource_with(b"#cloud-config\nhostname: h\n", b"");
        ds.vendordata_raw = None;
        ds.vendordata2_raw = None;
        assert_eq!(
            should_wait_on_network(Some(&ds), &mut Logger::silent()),
            (
                false,
                "user data: cloud-config does not contain network requiring \
                 elements, vendor data: no configuration found, vendor data2: \
                 no configuration found"
                    .to_owned()
            )
        );

        // Each blob is named in the reason when it is the one that decided.
        ds.vendordata_raw = Some(b"#cloud-config\nbootcmd: [x]\n".to_vec());
        assert_eq!(
            should_wait_on_network(Some(&ds), &mut Logger::silent()),
            (true, "bootcmd found in vendor data".to_owned())
        );
        ds.vendordata_raw = None;
        ds.vendordata2_raw = Some(b"#!/bin/sh\n".to_vec());
        assert_eq!(
            should_wait_on_network(Some(&ds), &mut Logger::silent()),
            (
                true,
                "non-cloud-config user data found in vendor data2".to_owned()
            )
        );
        ds.userdata_raw = Some(b"#cloud-config\nmounts: [[vdb, /mnt]]\n".to_vec());
        assert_eq!(
            should_wait_on_network(Some(&ds), &mut Logger::silent()),
            (true, "mounts found in user data".to_owned())
        );

        assert_eq!(
            should_wait_on_network(None, &mut Logger::silent()),
            (true, "no datasource found".to_owned())
        );
    }

    #[test]
    fn the_skip_network_marker_is_only_written_when_no_wait_is_needed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            run_dir: dir.path().join("run"),
            ..Paths::default()
        };
        std::fs::create_dir_all(&paths.run_dir).unwrap();
        let marker = paths.run_path(Lookup::SkipNetwork);

        let mut ds = datasource_with(b"#cloud-config\nbootcmd: [x]\n", b"");
        write_skip_network(Some(&ds), &paths, &mut Logger::silent());
        assert!(!marker.exists());

        ds.userdata_raw = Some(b"#cloud-config\nhostname: h\n".to_vec());
        write_skip_network(Some(&ds), &paths, &mut Logger::silent());
        assert_eq!(std::fs::read(&marker).unwrap(), b"");
    }
}
