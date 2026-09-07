//! Port of `cloudinit/event.py` and `stages.update_event_enabled`.
//!
//! A datasource declares which events it can react to and which of those are on
//! by default; user-data's `updates:` block may override the defaults per
//! scope; and a `hotplug.enabled` file written by `cloud-init devel
//! hotplug-hook` can add `hotplug` on top. The single consumer is
//! `Init.apply_network_config`, which is Phase 4 — this is the decision it will
//! ask for.
//!
//! `update_event_enabled` lives in `stages.py` upstream, but it reads nothing
//! from the stage other than the datasource and the merged config, so it sits
//! next to the model it interprets.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use ci_config::{repr, repr_str, Object, Value};
use ci_core::{Lookup, Paths};
use ci_log::Logger;

use crate::types::Probe;

/// `event.EventScope`.
///
/// Upstream has one member and a comment saying it means to grow more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Network,
}

impl Scope {
    /// The enum's value, which is what `__str__` returns.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "network" => Some(Self::Network),
            _ => None,
        }
    }

    /// `repr(EventScope.NETWORK)`.
    fn repr(self) -> String {
        format!("<EventScope.NETWORK: '{}'>", self.as_str())
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `event.EventType`.
///
/// The declaration order is also the order the port's sets iterate in; upstream
/// iterates a `set` of enum members, whose order is the members' identity
/// hashes and so differs between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Type {
    Boot,
    BootNewInstance,
    BootLegacy,
    Hotplug,
}

impl Type {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::BootNewInstance => "boot-new-instance",
            Self::BootLegacy => "boot-legacy",
            Self::Hotplug => "hotplug",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "boot" => Some(Self::Boot),
            "boot-new-instance" => Some(Self::BootNewInstance),
            "boot-legacy" => Some(Self::BootLegacy),
            "hotplug" => Some(Self::Hotplug),
            _ => None,
        }
    }

    /// `repr(EventType.BOOT)`.
    fn repr(self) -> String {
        let name = match self {
            Self::Boot => "BOOT",
            Self::BootNewInstance => "BOOT_NEW_INSTANCE",
            Self::BootLegacy => "BOOT_LEGACY",
            Self::Hotplug => "HOTPLUG",
        };
        format!("<EventType.{name}: '{}'>", self.as_str())
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `Dict[EventScope, Set[EventType]]`.
pub type Events = BTreeMap<Scope, BTreeSet<Type>>;

/// One scope with the events named for it.
#[must_use]
pub fn events(scope: Scope, types: &[Type]) -> Events {
    let mut map = Events::new();
    map.insert(scope, types.iter().copied().collect());
    map
}

/// `DataSource.supported_update_events`.
#[must_use]
pub fn supported_default() -> Events {
    events(
        Scope::Network,
        &[
            Type::BootNewInstance,
            Type::Boot,
            Type::BootLegacy,
            Type::Hotplug,
        ],
    )
}

/// `DataSource.default_update_events`: regenerate network config on a new
/// instance id, and on nothing else.
#[must_use]
pub fn default_default() -> Events {
    events(Scope::Network, &[Type::BootNewInstance])
}

/// `event.userdata_to_events`: `{'network': {'when': ['boot']}}` becomes
/// `{NETWORK: {BOOT}}`.
///
/// An entry that names an unknown scope is warned about and dropped, leaving
/// the datasource's default for that scope in force. An entry whose `when` list
/// names an unknown event is warned about and mapped to the *empty* set, which
/// then overrides the default — so one typo disables the scope rather than
/// falling back. Both of those are upstream's behaviour.
///
/// Every remaining malformed shape — `updates` not a mapping, a scope that is
/// not a mapping, a missing or non-iterable `when` — leaves upstream with an
/// unhandled `KeyError`/`TypeError`/`AttributeError` out of the boot stage
/// (COMPAT.md B51). The port warns and drops the entry instead, which is what
/// upstream already does for the one malformed shape it does handle.
pub fn userdata_to_events(user_config: Option<&Value>, logger: &mut Logger) -> Events {
    let mut update_config = Events::new();
    let config = match user_config {
        None | Some(Value::Null) => return update_config,
        Some(value) => value,
    };
    let Some(map) = config.as_object() else {
        logger.warning(
            "event.py",
            &format!(
                "{} is not a valid updates block! Update data will be ignored",
                repr(config)
            ),
        );
        return update_config;
    };

    for (scope, scope_list) in map {
        let Some(new_scope) = Scope::parse(scope) else {
            logger.warning(
                "event.py",
                &format!(
                    "{} is not a valid EventScope! Update data will be ignored \
                     for '{scope}' scope",
                    repr_str(scope)
                ),
            );
            continue;
        };
        let Some(when) = scope_list.get("when") else {
            logger.warning(
                "event.py",
                &format!(
                    "no 'when' list! Update data will be ignored for '{scope}' \
                     scope"
                ),
            );
            continue;
        };
        let Some(candidates) = iterate(when) else {
            logger.warning(
                "event.py",
                &format!(
                    "{} is not an iterable 'when'! Update data will be ignored \
                     for '{scope}' scope",
                    repr(when)
                ),
            );
            continue;
        };

        let mut new_values = BTreeSet::new();
        for candidate in candidates {
            if let Some(event) = candidate.as_str().and_then(Type::parse) {
                new_values.insert(event);
            } else {
                // The list comprehension aborts on the first bad element,
                // so the events before it are discarded too.
                logger.warning(
                    "event.py",
                    &format!(
                        "{} is not a valid EventType! Update data will be \
                         ignored for '{scope}' scope",
                        repr(&candidate)
                    ),
                );
                new_values.clear();
                break;
            }
        }
        update_config.insert(new_scope, new_values);
    }

    update_config
}

/// What Python's `for x in value` yields: a list's items, a string's characters
/// and a mapping's keys. Anything else is not iterable.
fn iterate(value: &Value) -> Option<Vec<Value>> {
    match value {
        Value::Array(items) => Some(items.clone()),
        Value::String(text) => {
            Some(text.chars().map(|c| Value::String(c.to_string())).collect())
        }
        Value::Object(map) => {
            Some(map.keys().map(|key| Value::String(key.clone())).collect())
        }
        _ => None,
    }
}

/// `util.read_hotplug_enabled_file`, narrowed to the `scopes` list both callers
/// read.
///
/// A file that is not decodable is warned about and ignored; upstream names the
/// compiled-in default path in that warning rather than the one it just failed
/// to read, which is reproduced.
#[must_use]
pub fn read_hotplug_enabled_scopes(paths: &Paths, logger: &mut Logger) -> Vec<String> {
    let path = paths.cpath(Lookup::HotplugEnabled);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            logger.debug("util.py", &format!("File not found: {}", path.display()));
            return Vec::new();
        }
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(content)) => content
            .get("scopes")
            .and_then(Value::as_array)
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(|scope| scope.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        Ok(_) => Vec::new(),
        Err(error) => {
            logger.warning(
                "util.py",
                &format!(
                    "Ignoring contents of /var/lib/cloud/hotplug.enabled \
                     because it is not decodable. Error: {error}"
                ),
            );
            Vec::new()
        }
    }
}

/// `stages.update_event_enabled`: is `event_source_type` allowed for `scope` on
/// this datasource, given the merged config?
///
/// On first boot there is no user-data yet, so only the datasource's defaults
/// are consulted and an event the tenant asked for can still be denied.
#[must_use]
pub fn update_event_enabled(
    probe: &dyn Probe,
    cfg: &Object,
    event_source_type: Type,
    scope: Scope,
    paths: &Paths,
    logger: &mut Logger,
) -> bool {
    let default_events = probe.default_update_events();
    let user_events = userdata_to_events(cfg.get("updates"), logger);
    // `mergemanydict` never replaces a key it already has, and the values are
    // sets rather than mappings, so a scope named in user-data wins whole: the
    // datasource's default events for that scope are not unioned in.
    let mut allowed = user_events;
    for (default_scope, default_types) in &default_events {
        allowed
            .entry(*default_scope)
            .or_insert_with(|| default_types.clone());
    }

    if probe
        .supported_update_events()
        .get(&scope)
        .is_some_and(|supported| supported.contains(&Type::Hotplug))
    {
        let enabled = read_hotplug_enabled_scopes(paths, logger);
        if enabled.iter().any(|name| name == scope.as_str()) {
            logger.debug(
                "stages.py",
                &format!(
                    "Adding event: scope={scope} EventType={} found in {}",
                    Type::Hotplug,
                    paths.cpath(Lookup::HotplugEnabled).display()
                ),
            );
            allowed.entry(scope).or_default().insert(Type::Hotplug);
        }
    }

    logger.debug(
        "stages.py",
        &format!("Allowed events: {}", repr_events(&allowed)),
    );

    if allowed
        .get(&scope)
        .is_some_and(|types| types.contains(&event_source_type))
    {
        logger.debug(
            "stages.py",
            &format!("Event Allowed: scope={scope} EventType={event_source_type}"),
        );
        return true;
    }

    logger.debug(
        "stages.py",
        &format!("Event Denied: scopes=['{scope}'] EventType={event_source_type}"),
    );
    false
}

/// `repr` of the `Dict[EventScope, Set[EventType]]` the debug line prints.
fn repr_events(map: &Events) -> String {
    if map.is_empty() {
        return "{}".to_owned();
    }
    let entries: Vec<String> = map
        .iter()
        .map(|(scope, types)| {
            let set = if types.is_empty() {
                "set()".to_owned()
            } else {
                let members: Vec<String> = types.iter().map(|t| t.repr()).collect();
                format!("{{{}}}", members.join(", "))
            };
            format!("{}: {set}", scope.repr())
        })
        .collect();
    format!("{{{}}}", entries.join(", "))
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

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn to_events(text: &str) -> Events {
        let mut logger = Logger::silent();
        userdata_to_events(Some(&parse(text)), &mut logger)
    }

    #[test]
    fn a_when_list_becomes_the_scopes_event_set() {
        assert_eq!(
            to_events(r#"{"network": {"when": ["boot", "hotplug"]}}"#),
            events(Scope::Network, &[Type::Boot, Type::Hotplug])
        );
    }

    #[test]
    fn an_unknown_scope_is_dropped_but_an_unknown_event_empties_the_scope() {
        assert!(to_events(r#"{"storage": {"when": ["boot"]}}"#).is_empty());
        assert_eq!(
            to_events(r#"{"network": {"when": ["boot", "bogus"]}}"#),
            events(Scope::Network, &[])
        );
    }

    #[test]
    fn a_when_string_is_iterated_by_character_as_python_does() {
        // "boot" yields 'b', which is not an event, so the scope empties.
        assert_eq!(
            to_events(r#"{"network": {"when": "boot"}}"#),
            events(Scope::Network, &[])
        );
    }

    #[test]
    fn the_warning_text_matches_the_enum_error_python_raises() {
        let mut logger = Logger::silent();
        userdata_to_events(
            Some(&parse(r#"{"storage": {"when": ["boot"]}}"#)),
            &mut logger,
        );
        userdata_to_events(Some(&parse(r#"{"network": {"when": [1]}}"#)), &mut logger);
        let recorded = logger.recoverable_errors();
        let warnings = recorded["WARNING"].as_array().unwrap();
        assert_eq!(
            warnings[0],
            Value::from(
                "'storage' is not a valid EventScope! Update data will be \
                 ignored for 'storage' scope"
            )
        );
        assert_eq!(
            warnings[1],
            Value::from(
                "1 is not a valid EventType! Update data will be ignored for \
                 'network' scope"
            )
        );
    }

    #[test]
    fn a_malformed_scope_is_dropped_where_upstream_raises() {
        assert!(to_events(r#"{"network": {}}"#).is_empty());
        assert!(to_events(r#"{"network": null}"#).is_empty());
        assert!(to_events(r#"{"network": {"when": 1}}"#).is_empty());
        let mut logger = Logger::silent();
        assert!(userdata_to_events(Some(&parse(r#""nope""#)), &mut logger).is_empty());
    }

    #[test]
    fn a_user_scope_replaces_the_default_rather_than_adding_to_it() {
        let mut logger = Logger::silent();
        let paths = Paths::default();
        let probe = crate::none::NoneSource;
        let cfg: Object =
            serde_json::from_str(r#"{"updates": {"network": {"when": ["boot"]}}}"#)
                .unwrap();

        assert!(update_event_enabled(
            &probe,
            &cfg,
            Type::Boot,
            Scope::Network,
            &paths,
            &mut logger
        ));
        assert!(!update_event_enabled(
            &probe,
            &cfg,
            Type::BootNewInstance,
            Scope::Network,
            &paths,
            &mut logger
        ));
    }

    #[test]
    fn the_datasource_default_applies_when_userdata_says_nothing() {
        let mut logger = Logger::silent();
        let paths = Paths::default();
        let cfg = Object::new();

        assert!(update_event_enabled(
            &crate::none::NoneSource,
            &cfg,
            Type::BootNewInstance,
            Scope::Network,
            &paths,
            &mut logger
        ));
        assert!(update_event_enabled(
            &crate::gce::Gce,
            &cfg,
            Type::Boot,
            Scope::Network,
            &paths,
            &mut logger
        ));
        assert!(!update_event_enabled(
            &crate::none::NoneSource,
            &cfg,
            Type::Boot,
            Scope::Network,
            &paths,
            &mut logger
        ));
    }

    #[test]
    fn the_hotplug_enabled_file_adds_an_event_the_defaults_leave_out() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: root.path().to_path_buf(),
            ..Paths::default()
        };
        std::fs::write(
            paths.cpath(Lookup::HotplugEnabled),
            r#"{"scopes": ["network"]}"#,
        )
        .unwrap();
        let mut logger = Logger::silent();

        assert!(update_event_enabled(
            &crate::none::NoneSource,
            &Object::new(),
            Type::Hotplug,
            Scope::Network,
            &paths,
            &mut logger
        ));
    }

    #[test]
    fn the_allowed_events_debug_line_renders_python_enum_reprs() {
        assert_eq!(repr_events(&Events::new()), "{}");
        assert_eq!(
            repr_events(&events(Scope::Network, &[])),
            "{<EventScope.NETWORK: 'network'>: set()}"
        );
        assert_eq!(
            repr_events(&events(Scope::Network, &[Type::Boot, Type::Hotplug])),
            "{<EventScope.NETWORK: 'network'>: {<EventType.BOOT: 'boot'>, \
             <EventType.HOTPLUG: 'hotplug'>}}"
        );
    }
}
