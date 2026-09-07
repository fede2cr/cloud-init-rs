//! `cloud-init devel net-convert` — read one network config format, write
//! another.
//!
//! This is upstream's own debugging tool, and it is the only way to exercise
//! the network renderers without a live system, which makes it the natural
//! differential surface for everything in `ci-net`.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args as ClapArgs, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    Eni,
    Yaml,
    #[value(name = "azure-imds")]
    AzureImds,
    #[value(name = "network_data.json")]
    NetworkDataJson,
    #[value(name = "vmware-imc")]
    VmwareImc,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eni => "eni",
            Self::Yaml => "yaml",
            Self::AzureImds => "azure-imds",
            Self::NetworkDataJson => "network_data.json",
            Self::VmwareImc => "vmware-imc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputKind {
    Eni,
    Netplan,
    Networkd,
    Sysconfig,
    #[value(name = "network-manager")]
    NetworkManager,
}

impl OutputKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eni => "eni",
            Self::Netplan => "netplan",
            Self::Networkd => "networkd",
            Self::Sysconfig => "sysconfig",
            Self::NetworkManager => "network-manager",
        }
    }
}

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The network configuration to read.
    #[arg(long = "network-data", short = 'p', value_name = "PATH")]
    pub network_data: PathBuf,

    /// The format of the given network config.
    #[arg(long, short = 'k', value_enum)]
    pub kind: Kind,

    /// Directory to place output in.
    #[arg(long, short = 'd', value_name = "PATH")]
    pub directory: PathBuf,

    /// The distro whose renderer configuration to use.
    #[arg(long, short = 'D')]
    pub distro: String,

    /// Interface name to mac mapping.
    #[arg(long, short = 'm', value_name = "name,mac")]
    pub mac: Vec<String>,

    /// Enable debug logging to stderr.
    #[arg(long)]
    pub debug: bool,

    /// The network config format to emit.
    #[arg(long = "output-kind", short = 'O', value_enum)]
    pub output_kind: OutputKind,
}

pub fn run(args: &Args) -> u8 {
    // `choices=distros.OSFAMILIES` flattened, which offers names that
    // `distros.fetch` will then refuse — `dragonfly` has no module.
    if !ci_distro::NAMES.contains(&args.distro.as_str()) {
        eprintln!(
            "net-convert: error: argument -D/--distro: invalid choice: '{}'",
            args.distro
        );
        return 2;
    }

    // argparse's `type=open` opens the file before anything else happens, so a
    // missing input is a usage error rather than a runtime one.
    let Ok(net_data) = ci_sys::path::read_text_capped(
        &args.network_data,
        ci_sys::path::DEFAULT_MAX_BYTES,
    ) else {
        eprintln!(
            "net-convert: error: argument -p/--network-data: can't open '{}'",
            args.network_data.display()
        );
        return 2;
    };

    // "if not args.directory.endswith('/')" — the trailing slash is visible in
    // the message the tool prints, so it is not cosmetic.
    let directory = format!(
        "{}/",
        args.directory.display().to_string().trim_end_matches('/')
    );

    if let Err(e) = std::fs::create_dir_all(&directory) {
        eprintln!("Cannot create directory {directory}: {e}");
        return 1;
    }

    let pre_ns = match args.kind {
        Kind::Yaml => match parse_yaml(&net_data) {
            Ok(value) => value,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        },
        other => {
            eprintln!(
                "net-convert: input kind '{}' is not implemented yet",
                other.as_str()
            );
            return 1;
        }
    };

    // `distros.fetch(args.distro)`, which imports a module named after the
    // distro and raises where there is none.
    let Some(distro) = ci_distro::fetch(&args.distro) else {
        eprintln!(
            "ImportError: No distribution found for distro {0} \
             (searched ['{0}', 'cloudinit.distros.{0}'])",
            args.distro
        );
        return 1;
    };

    if args.output_kind != OutputKind::Netplan {
        eprintln!(
            "net-convert: output kind '{}' is not implemented yet",
            args.output_kind.as_str()
        );
        return 1;
    }
    // Upstream reads `config["netplan_path"][1:]` off the `{}` default when
    // the distro has no netplan entry, so `-O netplan` only works for the five
    // that do.
    if !distro.has_renderer_config("netplan") {
        eprintln!("KeyError: 'netplan_path'");
        return 1;
    }
    let netplan = distro.renderer_config("netplan");
    let header = netplan
        .get("netplan_header")
        .and_then(ci_config::Value::as_str)
        .unwrap_or_default()
        .to_owned();

    render_netplan(args, &pre_ns, &directory, &header)
}

fn render_netplan(
    args: &Args,
    pre_ns: &serde_json::Value,
    directory: &str,
    header: &str,
) -> u8 {
    // `loggers.setup_basic_logging(level=WARN)`, or DEBUG with --debug. The
    // warnings the parser and the renderers raise go through this, so they
    // carry the timestamp/filename prefix operators expect in a log.
    let mut logger = ci_log::Logger::basic(if args.debug {
        ci_log::Level::Debug
    } else {
        ci_log::Level::Warning
    });

    let mut parse_warnings = ci_net::state::Warnings::default();
    let state = match ci_net::state::parse_net_config_data(
        pre_ns,
        ci_net::state::Target::Netplan,
        &mut parse_warnings,
    ) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    for warning in &parse_warnings.0 {
        logger.warning("network_state.py", warning);
    }

    // Both lines, and the blank one after them, are written before the render
    // so any renderer warning lands underneath.
    let mut stderr = std::io::stderr().lock();
    let _ = write!(
        stderr,
        "Read input format '{}' from '{}'.\n\
         Wrote output format '{}' to '{}'\n\n",
        args.kind.as_str(),
        args.network_data.display(),
        args.output_kind.as_str(),
        directory,
    );
    drop(stderr);

    let mut warnings = ci_net::netplan::Warnings::default();
    let content = ci_net::netplan::render_content(
        &state,
        // net-convert turns both feature flags on unconditionally.
        ci_net::netplan::Features {
            dhcp_use_domains: true,
            ipv6_mtu: true,
        },
        &mut warnings,
    );
    for warning in &warnings.0 {
        logger.warning("netplan.py", warning);
    }
    let content = ci_net::netplan::render_with_header(&content, header);

    let target = Path::new(directory).join("etc/netplan/50-cloud-init.yaml");
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("Cannot create directory {}: {e}", parent.display());
            return 1;
        }
    }
    // 0600, because `features.NETPLAN_CONFIG_ROOT_READ_ONLY` is on: netplan
    // config can carry wifi passwords.
    if let Err(e) = write_file(&target, content.as_bytes(), 0o600) {
        eprintln!("Cannot write {}: {e}", target.display());
        return 1;
    }
    0
}

/// `util.write_file`: the mode is applied explicitly, not left to the umask.
fn write_file(path: &Path, content: &[u8], mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    file.write_all(content)?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

/// `yaml.safe_load`, then the `if "network" in pre_ns` unwrap.
fn parse_yaml(text: &str) -> Result<serde_json::Value, String> {
    let value = ci_config::yaml::load_yaml(text, ci_config::yaml::Limits::default())
        .map_err(|e| e.to_string())?;
    match value.get("network") {
        Some(inner) => Ok(inner.clone()),
        None => Ok(value),
    }
}
