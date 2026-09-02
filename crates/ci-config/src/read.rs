//! Port of `util.read_conf*` and `stages.fetch_base_config`.
//!
//! Precedence, highest first: kernel command line, `/run/cloud-init/cloud.cfg`,
//! `/etc/cloud/cloud.cfg.d/*.cfg` (reverse-sorted, so `99-` wins),
//! `/etc/cloud/cloud.cfg`, built-in defaults.

use std::path::{Path, PathBuf};

use crate::builtin;
use crate::merge::merge_many;
use crate::yaml::{load_mapping, Limits, YamlError};
use crate::Object;

#[derive(Debug, thiserror::Error)]
pub enum ConfError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Yaml {
        path: PathBuf,
        #[source]
        source: YamlError,
    },
}

/// Where to source jinja variables from when rendering config files.
pub type InstanceData<'a> = Option<&'a Path>;

/// Read one config file, rendering it first if it declares a jinja template.
///
/// A missing file is not an error; it yields an empty mapping, matching upstream.
pub fn read_conf(
    path: impl AsRef<Path>,
    instance_data: InstanceData<'_>,
    limits: Limits,
) -> Result<Object, ConfError> {
    let path = path.as_ref();
    let Some(text) = ci_sys::path::read_text_optional(path, limits.max_bytes as u64)
        .map_err(|source| ConfError::Io {
            path: path.to_path_buf(),
            source,
        })?
    else {
        return Ok(Object::new());
    };

    let text = match instance_data {
        Some(data_path) if is_jinja(&text) => {
            render(&text, data_path, limits).unwrap_or(text.clone())
        }
        _ => text,
    };

    load_mapping(&text, limits).map_err(|source| ConfError::Yaml {
        path: path.to_path_buf(),
        source,
    })
}

/// Read `*.cfg` files from a `cloud.cfg.d` directory.
///
/// Reverse-sorted so that later-numbered files take precedence under the
/// first-wins default merger.
pub fn read_conf_d(
    dir: impl AsRef<Path>,
    instance_data: InstanceData<'_>,
    limits: Limits,
) -> Result<Object, ConfError> {
    let dir = dir.as_ref();
    let mut files = match ci_sys::path::list_files_sorted(dir) {
        Ok(files) => files,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Object::new()),
        Err(source) => {
            return Err(ConfError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
    };
    files.retain(|f| f.extension().is_some_and(|e| e == "cfg"));
    files.reverse();

    let mut configs = Vec::with_capacity(files.len());
    for file in files {
        configs.push(read_conf(&file, instance_data, limits)?);
    }
    Ok(merge_many(configs, false))
}

/// Read a config file together with its `<file>.d` directory.
pub fn read_conf_with_confd(
    cfgfile: impl AsRef<Path>,
    instance_data: InstanceData<'_>,
    limits: Limits,
) -> Result<Object, ConfError> {
    let cfgfile = cfgfile.as_ref();
    let cfg = read_conf(cfgfile, instance_data, limits)?;

    let confd = match cfg.get("conf_d") {
        Some(serde_json::Value::String(dir)) => Some(PathBuf::from(dir)),
        Some(serde_json::Value::Null) | None => {
            let implicit = with_suffix(cfgfile, ".d");
            implicit.is_dir().then_some(implicit)
        }
        // A non-string `conf_d` is a malformed config; upstream raises here.
        Some(_) => None,
    };
    let Some(confd) = confd else {
        return Ok(cfg);
    };

    let confd_cfg = read_conf_d(&confd, instance_data, limits)?;
    Ok(merge_many(vec![confd_cfg, cfg], false))
}

/// Port of `stages.fetch_base_config`.
pub fn fetch_base_config(
    instance_data: InstanceData<'_>,
    limits: Limits,
) -> Result<Object, ConfError> {
    let cloud_config = std::env::var(builtin::CFG_ENV_NAME)
        .map_or_else(|_| PathBuf::from(builtin::CLOUD_CONFIG), PathBuf::from);

    let sources = vec![
        builtin::cfg_builtin(),
        read_conf_with_confd(&cloud_config, instance_data, limits)?,
        read_conf(builtin::RUN_CLOUD_CONFIG, instance_data, limits)?,
        crate::cmdline::read_conf_from_cmdline(&crate::cmdline::get_cmdline(), limits),
    ];
    // `reverse` makes the *last* source the highest priority.
    Ok(merge_many(sources, true))
}

fn is_jinja(text: &str) -> bool {
    matches!(
        ci_template::detect_template(text),
        Ok((ci_template::TemplateKind::Jinja, _))
    )
}

fn render(text: &str, instance_data: &Path, limits: Limits) -> Option<String> {
    let raw = ci_sys::path::read_text_optional(instance_data, limits.max_bytes as u64)
        .ok()
        .flatten()?;
    let data: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let params = ci_template::convert_jinja_instance_data_with_aliases(&data);
    ci_template::render_string(text, &params).ok()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
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
    use serde_json::Value;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn missing_file_is_empty() {
        let cfg = read_conf("/nonexistent/cloud.cfg", None, Limits::default()).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn confd_overrides_the_base_file_and_higher_numbers_win() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("cloud.cfg");
        write(&base, "hostname: base\nlocale: en_GB\n");
        write(
            &dir.path().join("cloud.cfg.d/05-early.cfg"),
            "hostname: early\n",
        );
        write(
            &dir.path().join("cloud.cfg.d/99-late.cfg"),
            "hostname: late\n",
        );
        // Not a .cfg file: must be ignored.
        write(
            &dir.path().join("cloud.cfg.d/README"),
            "hostname: ignored\n",
        );

        let cfg = read_conf_with_confd(&base, None, Limits::default()).unwrap();
        assert_eq!(cfg["hostname"], Value::String("late".into()));
        assert_eq!(cfg["locale"], Value::String("en_GB".into()));
    }

    #[test]
    fn renders_jinja_config_against_instance_data() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("instance-data.json");
        write(
            &data,
            r#"{"v1": {"local_hostname": "web01", "cloud_name": "azure"}}"#,
        );
        let cfg_path = dir.path().join("90-tpl.cfg");
        write(
            &cfg_path,
            "## template: jinja\nhostname: {{ v1.local_hostname }}\nfqdn: {{ cloud_name }}\n",
        );

        let cfg = read_conf(&cfg_path, Some(&data), Limits::default()).unwrap();
        assert_eq!(cfg["hostname"], Value::String("web01".into()));
        assert_eq!(cfg["fqdn"], Value::String("azure".into()));
    }

    #[test]
    fn non_jinja_files_are_not_rendered() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("instance-data.json");
        write(&data, r#"{"v1": {}}"#);
        let cfg_path = dir.path().join("50-plain.cfg");
        write(&cfg_path, "message: \"{{ not_a_template }}\"\n");

        let cfg = read_conf(&cfg_path, Some(&data), Limits::default()).unwrap();
        assert_eq!(cfg["message"], Value::String("{{ not_a_template }}".into()));
    }
}
