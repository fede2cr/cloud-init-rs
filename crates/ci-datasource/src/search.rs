//! Port of `sources.find_source` and the module discovery around it.
//!
//! Upstream finds datasource classes by importing `DataSource<name>` modules
//! named in `datasource_list` from the packages in `datasource_pkg_list`. There
//! is no import to do here, so the registry is a table; the observable part —
//! which datasources are tried, in what order, and what is logged and reported
//! when none match — is preserved.

use ci_report::events::EventStack;

use crate::types::{Context, Datasource, Dep, Probe};

/// `sources.DataSourceNotFoundException`.
#[derive(Debug, Clone)]
pub struct NotFound {
    pub message: String,
}

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NotFound {}

/// One row of a datasource module's `datasources` table.
struct Entry {
    /// The name `datasource_list` matches, case-insensitively.
    dsname: &'static str,
    deps: &'static [Dep],
    build: fn() -> Box<dyn Probe>,
}

/// Every datasource this build knows about, in the order upstream's modules
/// declare them.
const REGISTRY: &[Entry] = &[
    Entry {
        dsname: "NoCloud",
        deps: &[Dep::Filesystem],
        build: || Box::new(crate::nocloud::NoCloud::local()),
    },
    Entry {
        dsname: "NoCloud",
        deps: &[Dep::Filesystem, Dep::Network],
        build: || Box::new(crate::nocloud::NoCloud::net()),
    },
    Entry {
        dsname: "ConfigDrive",
        deps: &[Dep::Filesystem],
        build: || Box::new(crate::configdrive::ConfigDrive),
    },
    Entry {
        dsname: "LXD",
        deps: &[Dep::Filesystem],
        build: || Box::new(crate::lxd::Lxd),
    },
    Entry {
        dsname: "Azure",
        deps: &[Dep::Filesystem],
        build: || Box::new(crate::azure::probe::Azure),
    },
    Entry {
        dsname: "OpenStack",
        deps: &[Dep::Filesystem, Dep::Network],
        build: || Box::new(crate::openstack::OpenStack),
    },
    Entry {
        dsname: "GCE",
        deps: &[Dep::Filesystem, Dep::Network],
        build: || Box::new(crate::gce::Gce),
    },
    Entry {
        dsname: "Ec2",
        deps: &[Dep::Filesystem, Dep::Network],
        build: || Box::new(crate::ec2::Ec2),
    },
    Entry {
        dsname: "None",
        deps: &[Dep::Filesystem, Dep::Network],
        build: || Box::new(crate::none::NoneSource),
    },
    Entry {
        dsname: "None",
        deps: &[],
        build: || Box::new(crate::none::NoneSource),
    },
];

/// The probe whose class a cached datasource names.
///
/// Upstream has no equivalent: a pickle carries its own class. The port needs
/// one to run `check_instance_id` against a restored datasource.
#[must_use]
pub fn probe_for_class(class_name: &str) -> Option<Box<dyn Probe>> {
    REGISTRY
        .iter()
        .map(|entry| (entry.build)())
        .find(|probe| probe.class_name() == class_name)
}

/// `sources.list_sources`: the classes named in `cfg_list` whose declared
/// dependencies match `depends` exactly.
///
/// Set equality, not containment: a datasource declared `(DEP_FILESYSTEM,)` is
/// deliberately invisible to the network stage.
#[must_use]
pub fn list_sources(cfg_list: &[String], depends: &[Dep]) -> Vec<Box<dyn Probe>> {
    let mut wanted = depends.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    let mut found = Vec::new();
    for name in cfg_list {
        for entry in REGISTRY {
            if !entry.dsname.eq_ignore_ascii_case(name) {
                continue;
            }
            let mut deps = entry.deps.to_vec();
            deps.sort_unstable();
            deps.dedup();
            if deps == wanted {
                found.push((entry.build)());
            }
        }
    }
    found
}

/// `sources.find_source`.
///
/// Each candidate gets its own reporting event so that a boot log shows which
/// datasources were tried and which one answered.
pub fn find_source(
    ctx: &mut Context<'_>,
    ds_deps: &[Dep],
    cfg_list: &[String],
    parent: &mut EventStack,
) -> Result<Datasource, NotFound> {
    let candidates = list_sources(cfg_list, ds_deps);
    let names: Vec<&str> = candidates.iter().map(|c| c.class_name()).collect();
    let mode = if ds_deps.contains(&Dep::Network) {
        "network"
    } else {
        "local"
    };
    ctx.logger.debug(
        "__init__.py",
        &format!(
            "Searching for {mode} data source in: [{}]",
            names
                .iter()
                .map(|name| format!("'{name}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );

    for probe in &candidates {
        let name = probe.class_name();
        let short = name.strip_prefix("DataSource").unwrap_or(name);
        let mut event = parent.child(
            &format!("search-{short}"),
            &format!("searching for {mode} data from {name}"),
        );
        event.set_message(format!("no {mode} data found from {name}"));
        event.open(ctx.reporter, ctx.logger);
        ctx.logger.debug(
            "__init__.py",
            &format!("Seeing if we can get any data from {name}"),
        );
        let found = probe.check_and_get_data(ctx);
        if found.is_some() {
            event.set_message(format!("found {mode} data from {name}"));
        }
        let result = event.close(ctx.reporter, ctx.logger);
        parent.record_child(result);
        if let Some(datasource) = found {
            // `DataSource.get_data` persists as soon as `_get_data` succeeds,
            // inside the search rather than after it.
            if let Err(err) = crate::instance_data::persist(&datasource, ctx) {
                ctx.logger.warning(
                    "__init__.py",
                    &format!("Error persisting instance-data.json: {err}"),
                );
            }
            return Ok(datasource);
        }
    }

    Err(NotFound {
        message: format!(
            "Did not find any data source, searched classes: ({})",
            names.join(", ")
        ),
    })
}

/// `sources.parse_cmdline_or_dmi`.
///
/// `ci.ds=` and `ci.datasource=` are deprecated spellings of `ds=`; they are
/// still accepted, and `ds=` still wins when more than one is present.
#[must_use]
pub fn parse_cmdline_or_dmi(input: &str) -> String {
    for key in ["ds", "ci.ds", "ci.datasource"] {
        if let Some(value) = first_value(input, key) {
            return value;
        }
    }
    String::new()
}

/// `re.search(r"(?:^|\s)<key>=([^\s;]+)", input)`.
fn first_value(input: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let mut from = 0usize;
    while let Some(at) = input.get(from..)?.find(&needle) {
        let start = from + at;
        let value_start = start + needle.len();
        let starts_a_word = start == 0
            || input
                .get(..start)?
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if starts_a_word {
            let value: String = input
                .get(value_start..)?
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != ';')
                .collect();
            if !value.is_empty() {
                return Some(value);
            }
        }
        from = value_start;
    }
    None
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

    fn names(cfg: &[&str], deps: &[Dep]) -> Vec<&'static str> {
        let list: Vec<String> = cfg.iter().map(|s| (*s).to_owned()).collect();
        list_sources(&list, deps)
            .iter()
            .map(|p| p.class_name())
            .collect()
    }

    #[test]
    fn the_local_stage_never_sees_a_network_only_datasource() {
        assert_eq!(
            names(&["NoCloud", "None"], &[Dep::Filesystem]),
            ["DataSourceNoCloud"]
        );
        assert_eq!(
            names(&["NoCloud", "None"], &[Dep::Filesystem, Dep::Network]),
            ["DataSourceNoCloudNet", "DataSourceNone"]
        );
    }

    #[test]
    fn datasource_list_decides_the_search_order() {
        assert_eq!(
            names(&["None", "NoCloud"], &[Dep::Filesystem, Dep::Network]),
            ["DataSourceNone", "DataSourceNoCloudNet"]
        );
    }

    #[test]
    fn datasource_names_are_matched_without_regard_to_case() {
        assert_eq!(
            names(&["nocloud"], &[Dep::Filesystem]),
            ["DataSourceNoCloud"]
        );
    }

    #[test]
    fn the_empty_dependency_set_is_what_the_modules_stages_search() {
        assert_eq!(names(&["None", "NoCloud"], &[]), ["DataSourceNone"]);
    }

    #[test]
    fn a_datasource_on_the_command_line_must_start_a_word() {
        assert_eq!(parse_cmdline_or_dmi("ro ds=nocloud quiet"), "nocloud");
        assert_eq!(parse_cmdline_or_dmi("ro ds=nocloud;s=/tmp"), "nocloud");
        assert_eq!(parse_cmdline_or_dmi("ro nods=nocloud"), "");
    }

    #[test]
    fn the_deprecated_spellings_are_still_accepted_but_rank_below_ds() {
        assert_eq!(parse_cmdline_or_dmi("ci.ds=OpenStack"), "OpenStack");
        assert_eq!(parse_cmdline_or_dmi("ci.datasource=Ec2"), "Ec2");
        assert_eq!(parse_cmdline_or_dmi("ci.ds=Ec2 ds=NoCloud"), "NoCloud");
    }
}
