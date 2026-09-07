//! Port of `cc_disk_setup.py`: partition disks and put filesystems on them.
//!
//! Like `cc_growpart`, this module cannot be split into a plan and a run. It
//! decides what to do by *asking the disk* — `lsblk` for the children, `blkid`
//! for the filesystem, `sfdisk -l` or `sgdisk -p` for the partition table —
//! and each answer is only available once the previous command has run. So the
//! machine goes behind [`Host`] in the same shape: [`Live`] is the real one,
//! [`Fixture`] answers from a script and records every question, and the
//! differential compares the recorded sequence as well as the log.
//!
//! This is the most destructive module in the port. Every path that writes is
//! reached through a command or through [`Host::wipe_ends`], so a fixture run
//! cannot touch a device by construction: the fixture has no way to spawn
//! anything.

use ci_config::{Object, Value};
use ci_log::Logger;

use super::growpart::{CommandResult, ProcError};
use super::{py_str, Args};

const SOURCE: &str = "cc_disk_setup.py";

/// `LANG_C_ENV`.
const LANG_C_ENV: [(&str, &str); 1] = [("LANG", "C")];

mod gpt_ids;
pub use gpt_ids::SGDISK_TO_GPT_ID;

/// `sgdisk_to_gpt_id[code]`.
fn sgdisk_to_gpt_id(code: &str) -> Option<&'static str> {
    SGDISK_TO_GPT_ID
        .iter()
        .find(|(key, _)| *key == code)
        .map(|(_, guid)| *guid)
}

/// Everything `cc_disk_setup` asks the machine.
///
/// `&mut self` is for recording, not for state: [`Fixture`] appends each
/// question to a list so the differential can compare the order in which the
/// two implementations ask them.
pub trait Host {
    /// `subp.subp(argv, data=data, update_env=env, rcs=rcs)`.
    fn subp(
        &mut self,
        argv: &[String],
        data: Option<&str>,
        env: &[(&str, &str)],
        rcs: &[i32],
    ) -> Result<(String, String), ProcError>;

    /// `subp.subp(command, shell=True)`, which `mkfs` uses for a configured
    /// `cmd`.
    fn subp_shell(&mut self, command: &str) -> Result<(String, String), ProcError>;

    /// `subp.which(program)`, which answers with the resolved path because
    /// `mkfs` puts it straight into the command it builds.
    fn which(&mut self, program: &str) -> Option<String>;

    /// `os.path.exists(path)`.
    fn exists(&mut self, path: &str) -> bool;

    /// `pathlib.Path(path).is_block_device()`.
    fn is_block_device(&mut self, path: &str) -> bool;

    /// `os.path.realpath(path)`.
    fn realpath(&mut self, path: &str) -> String;

    /// `purge_disk_ptable`: zero the first and last mebibyte of the device.
    ///
    /// One call rather than the open/write/seek/write/flush upstream spells
    /// out, because there is no decision between those steps — either all of
    /// them happen or the first one fails.
    fn wipe_ends(&mut self, device: &str) -> Result<(), String>;
}

/// `str(exception)` of whatever upstream raised: the only thing `handle` does
/// with a failure is print it.
type Failure = String;

/// One row of `lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL`.
///
/// Upstream builds a dict pre-seeded with these four keys set to `None` and
/// then overwrites whichever the output names, so a column `lsblk` leaves out
/// stays `None` rather than being absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub name: Option<String>,
    /// `d["type"]`, spelled out because `type` is a keyword here.
    pub kind: Option<String>,
    pub fstype: Option<String>,
    pub label: Option<String>,
}

/// `value_splitter`.
///
/// The two failures are upstream's own: `shlex.split` raising on an unbalanced
/// quote, and the tuple unpacking of `x.split("=")` raising when a token has
/// no `=` or has more than one — see B87.
fn value_splitter(
    values: &str,
    start: usize,
) -> Result<Vec<(String, String)>, Failure> {
    let tokens =
        ci_core::shlex::split(values, false).map_err(|error| error.to_string())?;
    let mut pairs = Vec::new();
    for token in tokens.into_iter().skip(start) {
        let fields: Vec<&str> = token.split('=').collect();
        match fields.len() {
            2 => pairs.push((
                fields.first().unwrap_or(&"").to_string(),
                fields.get(1).unwrap_or(&"").to_string(),
            )),
            1 => {
                return Err("not enough values to unpack (expected 2, got 1)".to_owned())
            }
            count => {
                return Err(format!(
                    "too many values to unpack (expected 2, got {count})"
                ))
            }
        }
    }
    Ok(pairs)
}

/// `enumerate_disk`.
fn enumerate_disk(
    host: &mut dyn Host,
    device: &str,
    nodeps: bool,
) -> Result<Vec<Entry>, Failure> {
    let mut lsblk = argv(&["lsblk", "--pairs", "--output", "NAME,TYPE,FSTYPE,LABEL"]);
    lsblk.push(device.to_owned());
    if nodeps {
        lsblk.push("--nodeps".to_owned());
    }

    let (info, _) = host
        .subp(&lsblk, None, &[], &[0])
        .map_err(|error| format!("Failed during disk check for {device}\n{error}"))?;

    let mut entries = Vec::new();
    for part in info.trim().lines() {
        if part.split_whitespace().next().is_none() {
            continue;
        }
        let mut entry = Entry::default();
        for (key, value) in value_splitter(part, 0)? {
            match key.to_lowercase().as_str() {
                "name" => entry.name = Some(value),
                "type" => entry.kind = Some(value),
                "fstype" => entry.fstype = Some(value),
                "label" => entry.label = Some(value),
                // Upstream would add the key to its dict; nothing reads it.
                _ => {}
            }
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// `device_type`.
///
/// The `if "type" in d` guard upstream cannot be false — the key is seeded
/// with `None` — so an `lsblk` row without a `TYPE=` column reaches
/// `None.lower()`. The `AttributeError` that produces is transcribed rather
/// than smoothed over, because `is_device_valid` swallows it into a warning
/// and the two implementations have to agree about which warning.
fn device_type(host: &mut dyn Host, device: &str) -> Result<Option<String>, Failure> {
    match enumerate_disk(host, device, true)?.into_iter().next() {
        Some(entry) => match entry.kind {
            Some(kind) => Ok(Some(kind.to_lowercase())),
            None => Err("'NoneType' object has no attribute 'lower'".to_owned()),
        },
        None => Ok(None),
    }
}

/// `is_device_valid`.
fn is_device_valid(
    host: &mut dyn Host,
    log: &mut Logger,
    name: &str,
    partition: bool,
) -> bool {
    let Ok(d_type) = device_type(host, name) else {
        log.warning(SOURCE, &format!("Query against device {name} failed"));
        return false;
    };
    let d_type = d_type.unwrap_or_default();
    if partition && d_type == "part" {
        return true;
    }
    !partition && d_type == "disk"
}

/// `check_fs`'s three answers: the label, the filesystem type and the UUID,
/// each present only if `blkid` printed it.
type FsInfo = (Option<String>, Option<String>, Option<String>);

/// `check_fs`, returning `(label, fs_type, uuid)`.
fn check_fs(host: &mut dyn Host, device: &str) -> Result<FsInfo, Failure> {
    let blkid = argv(&["blkid", "-c", "/dev/null", device]);
    let (out, _) = host
        .subp(&blkid, None, &[], &[0, 2])
        .map_err(|error| format!("Failed during disk check for {device}\n{error}"))?;

    let (mut label, mut fs_type, mut uuid) = (None, None, None);
    if !out.is_empty() && out.lines().count() == 1 {
        for (key, value) in value_splitter(&out, 1)? {
            match key.to_lowercase().as_str() {
                "label" => label = Some(value),
                "type" => fs_type = Some(value),
                "uuid" => uuid = Some(value),
                _ => {}
            }
        }
    }
    Ok((label, fs_type, uuid))
}

/// `is_filesystem`.
fn is_filesystem(host: &mut dyn Host, device: &str) -> Result<Option<String>, Failure> {
    Ok(check_fs(host, device)?.1)
}

/// Python's `==` between an `lsblk` column, which is a string or `None`, and a
/// config value, which can be anything the operator wrote.
fn py_eq(column: Option<&str>, expected: &Value) -> bool {
    match expected {
        Value::Null => column.is_none(),
        Value::String(text) => column == Some(text.as_str()),
        _ => false,
    }
}

/// `find_device_node`, returning `(device, matched)`.
///
/// `valid_targets` is not a parameter: every caller leaves it at its default.
fn find_device_node(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
    fs_type: &Value,
    label: &Value,
    label_match: bool,
    replace_fs: &Value,
) -> Result<(Option<String>, bool), Failure> {
    // `if label is None: label = ""`, which makes a missing label compare
    // equal to an empty one but leaves a non-string label alone.
    let label = if label.is_null() {
        Value::from("")
    } else {
        label.clone()
    };

    let mut raw_device_used = false;
    for entry in enumerate_disk(host, device, false)? {
        let name = entry.name.clone().unwrap_or_default();

        if py_eq(entry.fstype.as_deref(), replace_fs) && !label_match {
            return Ok((Some(format!("/dev/{name}")), false));
        }

        // `(label_match and d["label"] == label) or not label_match`.
        let label_matches = !label_match || py_eq(entry.label.as_deref(), &label);
        if py_eq(entry.fstype.as_deref(), fs_type) && label_matches {
            return Ok((Some(format!("/dev/{name}")), true));
        }

        let kind = entry.kind.as_deref().unwrap_or_default();
        if entry.kind.is_some() && ["disk", "part"].contains(&kind) {
            if kind != "disk" || truthy_opt(entry.fstype.as_deref()) {
                raw_device_used = true;
            }

            if kind == "disk" {
                // Skip the raw disk, it is the default.
            } else if !truthy_opt(entry.fstype.as_deref()) {
                return Ok((Some(format!("/dev/{name}")), false));
            }
        }
    }

    if !raw_device_used {
        return Ok((Some(device.to_owned()), false));
    }

    log.warning(
        SOURCE,
        "Failed to find device during available device search.",
    );
    Ok((None, false))
}

/// `is_disk_used`.
fn is_disk_used(host: &mut dyn Host, device: &str) -> Result<bool, Failure> {
    if enumerate_disk(host, device, false)?.len() > 1 {
        return Ok(true);
    }
    Ok(truthy_opt(check_fs(host, device)?.1.as_deref()))
}

/// `get_hdd_size`: the size in sectors, as the float division upstream does.
fn get_hdd_size(host: &mut dyn Host, device: &str) -> Result<f64, Failure> {
    let size_in_bytes = host
        .subp(&argv(&["blockdev", "--getsize64", device]), None, &[], &[0])
        .map_err(|error| format!("Failed to get {device} size\n{error}"))?
        .0;
    let sector_size = host
        .subp(&argv(&["blockdev", "--getss", device]), None, &[], &[0])
        .map_err(|error| format!("Failed to get {device} size\n{error}"))?
        .0;

    let bytes = py_int(&size_in_bytes)?;
    let sector = py_int(&sector_size)?;
    if sector == 0.0 {
        // `int / int`, which names itself plainly.
        return Err("division by zero".to_owned());
    }
    Ok(bytes / sector)
}

/// `int(text)` on a command's output, with the `ValueError` upstream lets
/// escape when the output is not a number.
#[expect(
    clippy::cast_precision_loss,
    reason = "upstream divides two ints, which Python does in floating point"
)]
fn py_int(text: &str) -> Result<f64, Failure> {
    let trimmed = text.trim();
    trimmed
        .parse::<i64>()
        .map(|value| value as f64)
        .map_err(|_| {
            format!(
                "invalid literal for int() with base 10: {}",
                ci_config::repr_str(text)
            )
        })
}

/// `check_partition_mbr_layout`.
fn check_partition_mbr_layout(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
    layout: &Value,
) -> Result<Vec<Option<String>>, Failure> {
    read_parttbl(host, log, device)?;

    let data = format!("{}\n", py_str(layout));
    let (out, _) = host
        .subp(&argv(&["sfdisk", "-l", device]), Some(&data), &[], &[0])
        .map_err(|error| {
            format!("Error running partition command on {device}\n{error}")
        })?;

    let mut found_layout = Vec::new();
    for line in out.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(first) = fields.first() else {
            continue;
        };

        if !first.contains(device) {
            continue;
        }

        // Extended and empty entries are not understood.
        let last = fields.last().copied().unwrap_or_default().to_lowercase();
        if last == "extended" || last == "empty" {
            continue;
        }

        let mut type_label = None;
        for index in (1..fields.len()).rev() {
            let field = fields.get(index).copied().unwrap_or_default();
            if py_isdigit(field) && field != "/" {
                type_label = Some(field.to_owned());
                break;
            }
        }

        found_layout.push(type_label);
    }
    Ok(found_layout)
}

/// `check_partition_gpt_layout_sgdisk`.
fn check_partition_gpt_layout_sgdisk(
    host: &mut dyn Host,
    device: &str,
) -> Result<Vec<Option<String>>, Failure> {
    let (out, _) = host
        .subp(&argv(&["sgdisk", "-p", device]), None, &LANG_C_ENV, &[0])
        .map_err(|error| {
            format!("Error running partition command on {device}\n{error}")
        })?;

    let mut lines = out.lines();
    for line in lines.by_ref() {
        if line.trim_start().starts_with("Number") {
            break;
        }
    }

    let mut found = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(field) = fields.get(5) else {
            return Err("list index out of range".to_owned());
        };
        found.push(Some((*field).to_owned()));
    }
    Ok(found)
}

/// `check_partition_gpt_layout_sfdisk`.
fn check_partition_gpt_layout_sfdisk(
    host: &mut dyn Host,
    device: &str,
) -> Result<Vec<Option<String>>, Failure> {
    let (out, _) = host
        .subp(
            &argv(&["sfdisk", "-l", "-J", device]),
            None,
            &LANG_C_ENV,
            &[0],
        )
        .map_err(|error| {
            format!("Error running partition command on {device}\n{error}")
        })?;

    let wrap = |message: String| {
        format!("Error running partition command on {device}\n{message}")
    };

    let parsed: Value =
        serde_json::from_str(&out).map_err(|error| wrap(error.to_string()))?;
    let table = parsed
        .get("partitiontable")
        .ok_or_else(|| wrap("'partitiontable'".to_owned()))?;
    let partitions = match table.get("partitions") {
        Some(Value::Array(items)) => items.clone(),
        Some(_) | None => Vec::new(),
    };

    let mut found = Vec::new();
    for partition in &partitions {
        let kind = partition
            .get("type")
            .ok_or_else(|| wrap("'type'".to_owned()))?;
        found.push(Some(py_str(kind)));
    }
    Ok(found)
}

/// `check_partition_gpt_layout`.
fn check_partition_gpt_layout(
    host: &mut dyn Host,
    device: &str,
) -> Result<Vec<Option<String>>, Failure> {
    if host.which("sgdisk").is_some() {
        return check_partition_gpt_layout_sgdisk(host, device);
    }
    check_partition_gpt_layout_sfdisk(host, device)
}

/// `partition_type_matches`.
///
/// The parameters are named as upstream names them, which is not what the one
/// caller passes: it hands the configured type first and the observed one
/// second. See B87 — the three error messages this raises all name the wrong
/// value because of it.
fn partition_type_matches(
    found_type: &str,
    expected_type: &str,
) -> Result<bool, Failure> {
    let mut found_type = found_type.to_uppercase();
    if ![2, 4, 36].contains(&found_type.chars().count()) {
        return Err(format!("Unknown partition type found: {found_type}"));
    }

    let mut expected_type = expected_type.to_uppercase();
    if ![2, 4, 36].contains(&expected_type.chars().count()) {
        return Err(format!("Unknown partition type specified: {found_type}"));
    }

    if found_type.chars().count() == 2 {
        found_type.push_str("00");
    }
    if expected_type.chars().count() == 2 {
        expected_type.push_str("00");
    }

    if found_type.chars().count() == expected_type.chars().count() {
        return Ok(found_type == expected_type);
    }

    if found_type.chars().count() == 4 {
        match sgdisk_to_gpt_id(&found_type) {
            Some(guid) => found_type = guid.to_owned(),
            None => {
                return Err(format!("Cannot find GPT GUID for found type {found_type}"))
            }
        }
    }
    if expected_type.chars().count() == 4 {
        match sgdisk_to_gpt_id(&expected_type) {
            Some(guid) => expected_type = guid.to_owned(),
            None => {
                return Err(format!(
                    "Cannot find GPT GUID for expected type {found_type}"
                ))
            }
        }
    }

    Ok(found_type == expected_type)
}

/// `check_partition_layout`.
fn check_partition_layout(
    host: &mut dyn Host,
    log: &mut Logger,
    table_type: &str,
    device: &str,
    layout: &Value,
) -> Result<bool, Failure> {
    let found_layout = match table_type {
        "gpt" => check_partition_gpt_layout(host, device)?,
        "mbr" => check_partition_mbr_layout(host, log, device, layout)?,
        _ => return Err("Unable to determine table type".to_owned()),
    };

    log.debug(
        SOURCE,
        &format!(
            "called check_partition_{table_type}_layout({device}, {}), returned: {}",
            py_str(layout),
            py_list_opt(&found_layout),
        ),
    );

    if let Value::Bool(wanted) = layout {
        // Auto partitioning is happy with any single partition.
        return Ok(*wanted && !found_layout.is_empty());
    }

    if found_layout.len() != py_len(layout)? {
        return Ok(false);
    }

    let mut layout_types: Vec<Option<String>> = Vec::new();
    for item in py_iter(layout)? {
        match item {
            Value::Array(pair) => match pair.get(1) {
                Some(second) => layout_types.push(Some(py_str(second))),
                None => return Err("list index out of range".to_owned()),
            },
            _ => layout_types.push(None),
        }
    }

    log.debug(
        SOURCE,
        &format!(
            "Layout types={}. Found types={}",
            py_list_opt(&layout_types),
            py_list_opt(&found_layout),
        ),
    );

    for (itype, ftype) in layout_types.iter().zip(found_layout.iter()) {
        if let Some(itype) = itype {
            let ftype = match ftype {
                Some(text) => text.clone(),
                // `str(None)` is what upstream's `.upper()` sees.
                None => "None".to_owned(),
            };
            if !partition_type_matches(itype, &ftype)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// `get_partition_mbr_layout`: the `sfdisk` script, as one string.
fn get_partition_mbr_layout(size: f64, layout: &Value) -> Result<String, Failure> {
    if let Value::Bool(_) = layout {
        // A single partition, defaulting to Linux. Note that `False` never
        // reaches here: `mkpart` returns before this on a falsy layout.
        return Ok(",,83".to_owned());
    }

    let Value::Array(items) = layout else {
        return Err("Partition layout is invalid".to_owned());
    };
    if items.is_empty() {
        return Err("Partition layout is invalid".to_owned());
    }

    let last_part_num = items.len();
    if last_part_num > 4 {
        return Err("Only simply partitioning is allowed.".to_owned());
    }

    let mut part_definition = Vec::new();
    for (index, part) in items.iter().enumerate() {
        let mut part_type = Value::from(83);
        let mut percent = part.clone();

        if let Value::Array(pair) = part {
            if pair.len() != 2 {
                return Err(format!(
                    "Partition was incorrectly defined: {}",
                    ci_config::repr(part)
                ));
            }
            percent = pair.first().cloned().unwrap_or(Value::Null);
            part_type = pair.get(1).cloned().unwrap_or(Value::Null);
        }

        let part_size = py_int_of(size * (py_float(&percent)? / 100.0));

        if index + 1 == last_part_num {
            part_definition.push(format!(",,{}", py_str(&part_type)));
        } else {
            part_definition.push(format!(",{part_size},{}", py_str(&part_type)));
        }
    }

    Ok(part_definition.join("\n"))
}

/// `get_partition_gpt_layout`: `(type, [start, end])` per partition.
fn get_partition_gpt_layout(
    size: f64,
    layout: &Value,
) -> Result<Vec<(Value, String, String)>, Failure> {
    let Value::Bool(_) = layout else {
        let mut specs = Vec::new();
        for partition in py_iter(layout)? {
            let (percent, partition_type) = match &partition {
                Value::Array(pair) => {
                    if pair.len() != 2 {
                        return Err(format!(
                            "Partition was incorrectly defined: {}",
                            ci_config::repr(&partition)
                        ));
                    }
                    (
                        pair.first().cloned().unwrap_or(Value::Null),
                        pair.get(1).cloned().unwrap_or(Value::Null),
                    )
                }
                other => (other.clone(), Value::Null),
            };

            let part_size = py_int_of(size * (py_float(&percent)? / 100.0));
            specs.push((partition_type, "0".to_owned(), format!("+{part_size}")));
        }

        // The last partition should use up all remaining space.
        match specs.last_mut() {
            Some(last) => "0".clone_into(&mut last.2),
            None => return Err("list index out of range".to_owned()),
        }
        return Ok(specs);
    };

    Ok(vec![(Value::Null, "0".to_owned(), "0".to_owned())])
}

/// `len(value)`, which a string has and a number does not.
fn py_len(value: &Value) -> Result<usize, Failure> {
    match value {
        Value::Array(items) => Ok(items.len()),
        Value::String(text) => Ok(text.chars().count()),
        Value::Object(map) => Ok(map.len()),
        other => Err(format!(
            "object of type '{}' has no len()",
            ci_config::type_name(other)
        )),
    }
}

/// `for x in value`, which walks a string one character at a time.
fn py_iter(value: &Value) -> Result<Vec<Value>, Failure> {
    match value {
        Value::Array(items) => Ok(items.clone()),
        Value::String(text) => {
            Ok(text.chars().map(|c| Value::from(c.to_string())).collect())
        }
        Value::Object(map) => {
            Ok(map.keys().map(|key| Value::from(key.clone())).collect())
        }
        other => Err(format!(
            "'{}' object is not iterable",
            ci_config::type_name(other)
        )),
    }
}

/// `float(value)` over a config scalar, with the messages Python raises.
fn py_float(value: &Value) -> Result<f64, Failure> {
    match value {
        Value::Number(number) => number.as_f64().ok_or_else(|| "float".to_owned()),
        Value::Bool(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::String(text) => text.trim().parse::<f64>().map_err(|_| {
            format!(
                "could not convert string to float: {}",
                ci_config::repr_str(text)
            )
        }),
        other => Err(format!(
            "float() argument must be a string or a real number, not '{}'",
            ci_config::type_name(other)
        )),
    }
}

/// `int(float)`: truncation towards zero.
#[expect(
    clippy::cast_possible_truncation,
    reason = "upstream's int() truncates the same way"
)]
fn py_int_of(value: f64) -> i64 {
    value.trunc() as i64
}

/// `purge_disk`.
fn purge_disk(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
) -> Result<(), Failure> {
    // Wipe any filesystems first.
    for entry in enumerate_disk(host, device, false)? {
        let kind = entry.kind.as_deref().unwrap_or("");
        if entry.kind.is_some() && ["disk", "crypt"].contains(&kind) {
            continue;
        }
        let name = entry.name.clone().unwrap_or_default();
        log.info(SOURCE, &format!("Purging filesystem on /dev/{name}"));
        if host
            .subp(
                &argv(&["wipefs", "--all", &format!("/dev/{name}")]),
                None,
                &[],
                &[0],
            )
            .is_err()
        {
            return Err(format!("Failed FS purge of /dev/{name}"));
        }
    }

    purge_disk_ptable(host, log, device)
}

/// `purge_disk_ptable`.
fn purge_disk_ptable(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
) -> Result<(), Failure> {
    host.wipe_ends(device)?;
    read_parttbl(host, log, device)
}

/// `read_parttbl`.
///
/// The `partprobe` failure is logged, not raised — upstream calls
/// `util.logexc` and carries on. The two `udevadm settle` calls around it are
/// not guarded at all, so their failure ends whatever was partitioning.
fn read_parttbl(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
) -> Result<(), Failure> {
    let probe = if host.which("partprobe").is_some() {
        argv(&["partprobe", device])
    } else {
        argv(&["blockdev", "--rereadpt", device])
    };
    udevadm_settle(host).map_err(|error| error.to_string())?;
    if let Err(error) = host.subp(&probe, None, &[], &[0]) {
        logexc(log, &format!("Failed reading the partition table {error}"));
    }
    udevadm_settle(host).map_err(|error| error.to_string())?;
    Ok(())
}

/// `util.udevadm_settle()` with both optional arguments left out.
fn udevadm_settle(host: &mut dyn Host) -> Result<(), ProcError> {
    if host.which("udevadm").is_none() {
        return Ok(());
    }
    host.subp(&argv(&["udevadm", "settle"]), None, &[], &[0])
        .map(|_| ())
}

/// `exec_mkpart_mbr`.
fn exec_mkpart_mbr(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
    layout: &str,
) -> Result<(), Failure> {
    let data = format!("{layout}\n");
    host.subp(
        &argv(&["sfdisk", "--force", device]),
        Some(&data),
        &[],
        &[0],
    )
    .map_err(|error| format!("Failed to partition device {device}\n{error}"))?;
    read_parttbl(host, log, device)
}

/// `exec_mkpart_gpt_sgdisk`.
fn exec_mkpart_gpt_sgdisk(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
    layout: &[(Value, String, String)],
) -> Result<(), Failure> {
    let run = |host: &mut dyn Host, args: Vec<String>| -> Result<(), Failure> {
        host.subp(&args, None, &[], &[0])
            .map(|_| ())
            .map_err(|error| error.to_string())
    };

    let mut attempt = || -> Result<(), Failure> {
        run(host, argv(&["sgdisk", "-Z", device]))?;
        for (index, (partition_type, start, end)) in layout.iter().enumerate() {
            let number = index + 1;
            run(
                host,
                vec![
                    "sgdisk".to_owned(),
                    "-n".to_owned(),
                    format!("{number}:{start}:{end}"),
                    device.to_owned(),
                ],
            )?;
            if !partition_type.is_null() {
                // A four-character code right-padded with zeros: `82` becomes
                // `8200`, and `Linux` is left alone.
                let pinput = ljust(&py_str(partition_type), 4, '0');
                run(
                    host,
                    vec![
                        "sgdisk".to_owned(),
                        "-t".to_owned(),
                        format!("{number}:{pinput}"),
                        device.to_owned(),
                    ],
                )?;
            }
        }
        Ok(())
    };

    match attempt() {
        Ok(()) => Ok(()),
        Err(error) => {
            log.warning(SOURCE, &format!("Failed to partition device {device}"));
            Err(error)
        }
    }
}

/// `exec_mkpart_gpt_sfdisk`.
fn exec_mkpart_gpt_sfdisk(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
    layout: &[(Value, String, String)],
) -> Result<(), Failure> {
    let mut lines: Vec<String> = Vec::new();
    for (partition_type, _, end) in layout {
        let mut partition_type = ljust(&py_str(partition_type), 4, '0');
        if partition_type.chars().count() == 4 {
            if let Some(guid) = sgdisk_to_gpt_id(&partition_type) {
                partition_type = guid.to_owned();
            }
        }
        if partition_type.chars().count() != 36 {
            if partition_type != "None" {
                log.warning(
                    SOURCE,
                    &format!(
                        "Unknown GPT partition type {partition_type}, using Linux"
                    ),
                );
            }
            "0FC63DAF-8483-4772-8E79-3D69D8477DE4".clone_into(&mut partition_type);
        }
        if end == "0" {
            lines.push(format!(",,{partition_type}\n"));
        } else {
            lines.push(format!(",{end},{partition_type}\n"));
        }
    }
    let cmd = lines.concat();

    match host.subp(
        &argv(&["sfdisk", "-X", "gpt", "--force", device]),
        Some(&cmd),
        &[],
        &[0],
    ) {
        Ok(_) => Ok(()),
        Err(error) => {
            log.warning(SOURCE, &format!("Failed to partition device {device}"));
            Err(error.to_string())
        }
    }
}

/// `exec_mkpart_gpt`.
fn exec_mkpart_gpt(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
    layout: &[(Value, String, String)],
) -> Result<(), Failure> {
    if host.which("sgdisk").is_some() {
        exec_mkpart_gpt_sgdisk(host, log, device, layout)?;
    } else {
        exec_mkpart_gpt_sfdisk(host, log, device, layout)?;
    }
    read_parttbl(host, log, device)
}

/// `str.ljust(width, fill)`.
fn ljust(text: &str, width: usize, fill: char) -> String {
    let mut out = text.to_owned();
    while out.chars().count() < width {
        out.push(fill);
    }
    out
}

/// `assert_and_settle_device`.
fn assert_and_settle_device(host: &mut dyn Host, device: &str) -> Result<(), Failure> {
    if !host.exists(device) {
        udevadm_settle(host).map_err(|error| error.to_string())?;
        if !host.exists(device) {
            return Err(format!(
                "Device {device} did not exist and was not created \
                 with a udevadm settle."
            ));
        }
    }

    // Whether or not the device existed above, the udev events that populate
    // the database may not have finished, so settle again.
    udevadm_settle(host).map_err(|error| error.to_string())?;
    Ok(())
}

/// `mkpart`.
fn mkpart(
    host: &mut dyn Host,
    log: &mut Logger,
    device: &str,
    definition: &Object,
) -> Result<(), Failure> {
    // Resolve a symlink to the device it names.
    assert_and_settle_device(host, device)?;
    let device = host.realpath(device);
    let device = device.as_str();

    log.debug(SOURCE, &format!("Checking values for {device} definition"));
    let overwrite = definition
        .get("overwrite")
        .cloned()
        .unwrap_or(Value::Bool(false));
    let layout = definition
        .get("layout")
        .cloned()
        .unwrap_or(Value::Bool(false));
    let table_type = definition
        .get("table_type")
        .cloned()
        .unwrap_or_else(|| Value::from("mbr"));

    log.debug(SOURCE, "Checking against default devices");

    if matches!(layout, Value::Bool(false)) || !truthy(&layout) {
        log.debug(SOURCE, "Device is not to be partitioned, skipping");
        return Ok(());
    }

    // This prevents you from overwriting the device.
    log.debug(
        SOURCE,
        &format!("Checking if device {device} is a valid device"),
    );
    if !is_device_valid(host, log, device, false) {
        return Err(format!("Device {device} is not a disk device!"));
    }

    if let Value::String(text) = &layout {
        if text.to_lowercase() == "remove" {
            log.debug(SOURCE, "Instructed to remove partition table entries");
            return purge_disk(host, log, device);
        }
    }

    let table_type_str = py_str(&table_type);
    log.debug(SOURCE, "Checking if device layout matches");
    if check_partition_layout(host, log, &table_type_str, device, &layout)? {
        log.debug(SOURCE, "Device partitioning layout matches");
        return Ok(());
    }

    log.debug(SOURCE, "Checking if device is safe to partition");
    if !truthy(&overwrite)
        && (is_disk_used(host, device)?
            || truthy_opt(is_filesystem(host, device)?.as_deref()))
    {
        log.debug(
            SOURCE,
            &format!("Skipping partitioning on configured device {device}"),
        );
        return Ok(());
    }

    log.debug(SOURCE, &format!("Checking for device size of {device}"));
    let device_size = get_hdd_size(host, device)?;

    log.debug(SOURCE, "Calculating partition layout");
    match table_type_str.as_str() {
        "mbr" => {
            let part_definition = get_partition_mbr_layout(device_size, &layout)?;
            log.debug(SOURCE, &format!("   Layout is: {part_definition}"));
            log.debug(SOURCE, &format!("Creating partition table on {device}"));
            exec_mkpart_mbr(host, log, device, &part_definition)?;
        }
        "gpt" => {
            let part_definition = get_partition_gpt_layout(device_size, &layout)?;
            log.debug(
                SOURCE,
                &format!("   Layout is: {}", gpt_layout_repr(&part_definition)),
            );
            log.debug(SOURCE, &format!("Creating partition table on {device}"));
            exec_mkpart_gpt(host, log, device, &part_definition)?;
        }
        _ => return Err("Unable to determine table type".to_owned()),
    }

    log.debug(SOURCE, &format!("Partition table created for {device}"));
    Ok(())
}

/// `repr()` of the list of tuples `get_partition_gpt_layout` returns, which is
/// what the "Layout is" line prints.
fn gpt_layout_repr(specs: &[(Value, String, String)]) -> String {
    let rendered: Vec<String> = specs
        .iter()
        .map(|(kind, start, end)| {
            format!(
                "({}, [{}, {}])",
                ci_config::repr(kind),
                start,
                if end == "0" {
                    "0".to_owned()
                } else {
                    ci_config::repr_str(end)
                }
            )
        })
        .collect();
    format!("[{}]", rendered.join(", "))
}

/// `lookup_force_flag`.
fn lookup_force_flag(log: &mut Logger, fs: &str) -> String {
    let flags = [
        ("ext", "-F"),
        ("btrfs", "-f"),
        ("xfs", "-f"),
        ("reiserfs", "-f"),
        ("swap", "-f"),
    ];

    let lowered = fs.to_lowercase();
    let key = if lowered.contains("ext") {
        "ext"
    } else {
        lowered.as_str()
    };

    if let Some((_, flag)) = flags.iter().find(|(name, _)| *name == key) {
        return (*flag).to_owned();
    }

    log.warning(SOURCE, &format!("Force flag for {fs} is unknown."));
    String::new()
}

/// `mkfs`.
#[allow(clippy::too_many_lines)]
fn mkfs(host: &mut dyn Host, log: &mut Logger, fs_cfg: &Object) -> Result<(), Failure> {
    let label = fs_cfg.get("label").cloned().unwrap_or(Value::Null);
    let device_cfg = fs_cfg.get("device").cloned().unwrap_or(Value::Null);
    let partition = fs_cfg
        .get("partition")
        .map_or_else(|| "any".to_owned(), py_str);
    let fs_type = fs_cfg.get("filesystem").cloned().unwrap_or(Value::Null);
    let fs_cmd = fs_cfg
        .get("cmd")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let fs_opts = fs_cfg
        .get("extra_opts")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let fs_replace = fs_cfg
        .get("replace_fs")
        .cloned()
        .unwrap_or(Value::Bool(false));
    let overwrite = truthy(
        &fs_cfg
            .get("overwrite")
            .cloned()
            .unwrap_or(Value::Bool(false)),
    );

    // Resolve a symlink to the device it names. A `device` that is neither a
    // string nor a number reaches `os.stat` as-is, which is where upstream
    // stops; a number is a file descriptor as far as `os.path.exists` is
    // concerned, so it gets as far as the settle.
    let device_name = match &device_cfg {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        other => {
            let shown = py_str(other);
            host.exists(&shown);
            return Err(format!(
                "stat: path should be string, bytes, os.PathLike or integer, \
                 not {}",
                ci_config::type_name(other)
            ));
        }
    };
    assert_and_settle_device(host, &device_name)?;
    let mut device = host.realpath(&device_name);

    log.debug(
        SOURCE,
        &format!("Checking {device} against default devices"),
    );

    if partition.is_empty() || py_isdigit(&partition) {
        if py_isdigit(&partition) {
            // nvme names its partitions `nvme0n1p1`, not `nvme0n11`.
            if device
                .chars()
                .last()
                .is_some_and(|last| last.is_ascii_digit())
            {
                device.push('p');
            }
            device = format!("{device}{partition}");
            if !host.is_block_device(&device) {
                log.warning(
                    SOURCE,
                    &format!("Path {device} does not exist or is not a block device"),
                );
                return Ok(());
            }
            log.debug(
                SOURCE,
                &format!("Manual request of partition {partition} for {device}"),
            );
        }

        log.debug(SOURCE, &format!("Checking device {device}"));
        let (check_label, check_fstype, _) = check_fs(host, &device)?;
        log.debug(
            SOURCE,
            &format!(
                "Device '{device}' has check_label='{}' check_fstype={}",
                opt_str(check_label.as_deref()),
                opt_str(check_fstype.as_deref()),
            ),
        );

        if py_eq(check_label.as_deref(), &label)
            && py_eq(check_fstype.as_deref(), &fs_type)
        {
            log.debug(SOURCE, &format!("Existing file system found at {device}"));

            if !overwrite {
                log.debug(SOURCE, &format!("Device {device} has required file system"));
                return Ok(());
            }
            log.debug(SOURCE, &format!("Destroying filesystem on {device}"));
        } else {
            log.debug(
                SOURCE,
                &format!("Device {device} is cleared for formatting"),
            );
        }
    } else if matches!(partition.to_lowercase().as_str(), "auto" | "any") {
        let odevice = device.clone();
        log.debug(
            SOURCE,
            &format!(
                "Identifying device to create {} filesystem on",
                py_str(&label)
            ),
        );

        // `any` means pick the first match on the device with a matching
        // fs_type, whatever its label.
        let label_match = partition.to_lowercase() != "any";

        let (found, reuse) = find_device_node(
            host,
            log,
            &device,
            &fs_type,
            &label,
            label_match,
            &fs_replace,
        )?;
        let found_text = found.clone().unwrap_or_else(|| "None".to_owned());
        log.debug(
            SOURCE,
            &format!("Automatic device for {odevice} identified as {found_text}"),
        );

        if reuse {
            log.debug(SOURCE, "Found filesystem match, skipping formatting.");
            return Ok(());
        }

        if truthy(&fs_replace) && found.is_some() {
            log.debug(
                SOURCE,
                &format!("Replacing file system on {found_text} as instructed."),
            );
        }

        let Some(found) = found else {
            log.debug(
                SOURCE,
                &format!(
                    "No device available that matches request. \
                     Skipping fs creation for {}",
                    ci_config::repr(&Value::Object(fs_cfg.clone()))
                ),
            );
            return Ok(());
        };
        device = found;
    } else if partition.to_lowercase() == "none" {
        log.debug(
            SOURCE,
            &format!(
                "Using the raw device to place filesystem {} on",
                py_str(&label)
            ),
        );
    } else {
        log.debug(SOURCE, "Error in device identification handling.");
        return Ok(());
    }

    log.debug(
        SOURCE,
        &format!(
            "File system type '{}' with label '{}' will be created on {device}",
            py_str(&fs_type),
            py_str(&label),
        ),
    );

    if device.is_empty() {
        log.warning(SOURCE, &format!("Device is not known: {device}"));
        return Ok(());
    }

    if !truthy(&fs_type) && !truthy(&fs_cmd) {
        return Err(format!(
            "No way to create filesystem '{}'. fs_type or fs_cmd must be set.",
            py_str(&label)
        ));
    }

    // Build the command.
    let mut shell_command = None;
    let mut command: Vec<String> = Vec::new();
    if truthy(&fs_cmd) {
        let Value::String(template) = &fs_cmd else {
            return Err(format!(
                "unsupported operand type(s) for %: '{}' and 'dict'",
                ci_config::type_name(&fs_cmd)
            ));
        };
        let rendered = percent_format(
            template,
            &[
                ("label", py_str(&label)),
                ("filesystem", py_str(&fs_type)),
                ("device", device.clone()),
            ],
        )?;

        if overwrite {
            log.warning(
                SOURCE,
                &format!(
                    "fs_setup:overwrite ignored because cmd was specified: {rendered}"
                ),
            );
        }
        if truthy(&fs_opts) {
            log.warning(
                SOURCE,
                &format!(
                    "fs_setup:extra_opts ignored because cmd was specified: {rendered}"
                ),
            );
        }
        shell_command = Some(rendered);
    } else {
        let fs_type_text = py_str(&fs_type);
        let mkfs_cmd = host
            .which(&format!("mkfs.{fs_type_text}"))
            // `mkswap` is not `mkfs.swap`.
            .or_else(|| host.which(&format!("mk{fs_type_text}")));

        let Some(mkfs_cmd) = mkfs_cmd else {
            log.warning(
                SOURCE,
                &format!(
                    "Cannot create fstype '{fs_type_text}'.  \
                     No mkfs.{fs_type_text} command"
                ),
            );
            return Ok(());
        };

        command.push(mkfs_cmd);

        if truthy(&label) {
            command.push("-L".to_owned());
            command.push(py_str(&label));
        }

        // Filesystems that support a force flag.
        if overwrite || device_type(host, &device)?.as_deref() == Some("disk") {
            let force_flag = lookup_force_flag(log, &fs_type_text);
            if !force_flag.is_empty() {
                command.push(force_flag);
            }
        }

        match &fs_opts {
            Value::Array(items) => {
                for item in items {
                    command.push(py_str(item));
                }
            }
            Value::String(text) => {
                // `list.extend` over a string adds its characters.
                for character in text.chars() {
                    command.push(character.to_string());
                }
            }
            other => {
                return Err(format!(
                    "'{}' object is not iterable",
                    ci_config::type_name(other)
                ))
            }
        }

        command.push(device.clone());
    }

    log.debug(
        SOURCE,
        &format!("Creating file system {} on {device}", py_str(&label)),
    );
    let outcome = match &shell_command {
        Some(text) => host.subp_shell(text),
        None => host.subp(&command, None, &[], &[0]),
    };
    outcome.map(|_| ()).map_err(|error| {
        // `%s` of the command, which is the string itself under `shell=True`
        // and the list's repr otherwise -- and the same difference again
        // inside the exception's own `Command:` line.
        match &shell_command {
            Some(text) => {
                format!("Failed to exec of '{text}':\n{}", shell_error(&error))
            }
            None => format!(
                "Failed to exec of '{}':\n{error}",
                super::growpart::py_list(&command)
            ),
        }
    })
}

/// `str(ProcessExecutionError)` for a command that was a string rather than a
/// list, which is what `shell=True` hands the exception.
fn shell_error(error: &ProcError) -> String {
    format!(
        "Unexpected error while running command.\n\
         Command: {}\n\
         Exit code: {}\n\
         Reason: -\n\
         Stdout: {}\n\
         Stderr: {}",
        error.argv.first().cloned().unwrap_or_default(),
        error
            .exit_code
            .map_or_else(|| "-".to_owned(), |code| code.to_string()),
        super::growpart::indent_text(&error.stdout),
        super::growpart::indent_text(&error.stderr),
    )
}

/// Python's `%`-formatting with a mapping, which is how `fs_setup: cmd` is
/// filled in.
fn percent_format(
    template: &str,
    mapping: &[(&str, String)],
) -> Result<String, Failure> {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '%' {
            out.push(character);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some('(') => {
                chars.next();
                let mut key = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == ')' {
                        closed = true;
                        break;
                    }
                    key.push(next);
                }
                if !closed {
                    return Err("incomplete format key".to_owned());
                }
                // The conversion character, which upstream's templates always
                // write as `s`.
                match chars.next() {
                    Some('s') => {}
                    Some(other) => {
                        return Err(format!(
                        "unsupported format character '{other}' (0x{:x}) at index {}",
                        other as u32,
                        out.chars().count()
                    ))
                    }
                    None => return Err("incomplete format".to_owned()),
                }
                match mapping.iter().find(|(name, _)| *name == key) {
                    Some((_, value)) => out.push_str(value),
                    None => return Err(format!("'{key}'")),
                }
            }
            _ => return Err("format requires a mapping".to_owned()),
        }
    }
    Ok(out)
}

/// `update_disk_setup_devices`.
fn update_disk_setup_devices(
    disk_setup: &mut Object,
    log: &mut Logger,
    aliases: &Value,
) -> Result<(), Failure> {
    for origname in disk_setup.keys().cloned().collect::<Vec<String>>() {
        let Some(transformed) = alias_to_device(aliases, &origname)? else {
            continue;
        };
        if transformed == origname {
            continue;
        }

        if disk_setup.contains_key(&transformed) {
            log.info(
                SOURCE,
                &format!(
                    "Replacing {origname} in disk_setup for translation of {transformed}"
                ),
            );
            disk_setup.shift_remove(&transformed);
        }

        let mut moved = disk_setup.get(&origname).cloned().unwrap_or(Value::Null);
        if let Value::Object(map) = &mut moved {
            map.insert("_origname".to_owned(), Value::from(origname.clone()));
        }
        disk_setup.insert(transformed.clone(), moved);
        disk_setup.shift_remove(&origname);
        log.debug(
            SOURCE,
            &format!("updated disk_setup device entry '{origname}' to '{transformed}'"),
        );
    }
    Ok(())
}

/// `update_fs_setup_devices`.
fn update_fs_setup_devices(
    fs_setup: &mut [Value],
    log: &mut Logger,
    aliases: &Value,
) -> Result<(), Failure> {
    for definition in fs_setup {
        let Value::Object(definition) = definition else {
            log.warning(
                SOURCE,
                &format!("entry in disk_setup not a dict: {}", py_str(definition)),
            );
            continue;
        };

        let Some(origname) = definition.get("device").cloned() else {
            continue;
        };
        if origname.is_null() {
            continue;
        }
        // `util.expand_dotted_devname` calls `rsplit` on whatever it is given.
        let Value::String(origname) = origname else {
            return Err(format!(
                "'{}' object has no attribute 'rsplit'",
                ci_config::type_name(&origname)
            ));
        };

        let (dev, part) = super::mounts::expand_dotted_devname(&origname);

        if let Some(tformed) = alias_to_device(aliases, dev)? {
            log.debug(
                SOURCE,
                &format!(
                    "{origname} is mapped to disk={tformed} part={}",
                    part.unwrap_or("None")
                ),
            );
            definition.insert("_origname".to_owned(), Value::from(origname.clone()));
            definition.insert("device".to_owned(), Value::from(tformed));
        }

        if let Some(part) = part {
            if part.is_empty() {
                continue;
            }
            // In `<dev>.N`, the N overrides any `partition` key.
            if definition.contains_key("partition") {
                log.warning(
                    SOURCE,
                    &format!(
                        "Partition '{part}' from dotted device name '{origname}' \
                         overrides 'partition' key in {}",
                        ci_config::repr(&Value::Object(definition.clone()))
                    ),
                );
                let existing =
                    definition.get("partition").cloned().unwrap_or(Value::Null);
                definition.insert("_partition".to_owned(), existing);
            }
            definition.insert("partition".to_owned(), Value::from(part));
        }
    }
    Ok(())
}

/// `handle`'s `alias_to_device`.
///
/// `cloud.device_name_to_device` is the datasource hook that turns a metadata
/// name such as `ephemeral0` into a device, and no ported datasource
/// implements it (deviation 149), so it always answers `None` and the `or
/// name` fallback is what is left: the alias's value, or nothing.
///
/// # Errors
/// `device_aliases` is read with `.get()` and nothing checks its type first,
/// so anything but a mapping ends the module here.
fn alias_to_device(
    aliases: &Value,
    candidate: &str,
) -> Result<Option<String>, Failure> {
    let Value::Object(aliases) = aliases else {
        return Err(format!(
            "'{}' object has no attribute 'get'",
            ci_config::type_name(aliases)
        ));
    };
    Ok(aliases
        .get(candidate)
        .filter(|value| !value.is_null())
        .map(py_str))
}

/// Python truthiness over an `lsblk` or `blkid` column: a column the tool
/// printed empty is a string, and an empty string is false.
fn truthy_opt(value: Option<&str>) -> bool {
    value.is_some_and(|text| !text.is_empty())
}

/// Python truthiness over a config value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `str.isdigit()` for the ASCII digits, which is every digit a device name or
/// a partition number can carry here.
fn py_isdigit(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// `"%s" % value` for an optional string, where `None` prints as `None`.
fn opt_str(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_owned(), ToOwned::to_owned)
}

/// `repr()` of a list of optional strings.
fn py_list_opt(items: &[Option<String>]) -> String {
    let rendered: Vec<String> = items
        .iter()
        .map(|item| {
            item.as_ref()
                .map_or_else(|| "None".to_owned(), |text| ci_config::repr_str(text))
        })
        .collect();
    format!("[{}]", rendered.join(", "))
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// `util.logexc`: the message at warning, then again at debug where upstream
/// attaches the traceback this port does not have.
fn logexc(log: &mut Logger, message: &str) {
    log.warning("log_util.py", message);
    log.debug("log_util.py", message);
}

/// `handle`, against a [`Host`] the caller supplies.
///
/// # Errors
/// The two failures upstream does not catch: a `device_aliases` that is not a
/// mapping, and an `fs_setup` device name that is not a string.
pub fn handle_with(
    cfg: &Object,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    let aliases = cfg.get("device_aliases").cloned().unwrap_or_else(|| {
        // `cfg.get("device_aliases", {})`.
        Value::Object(Object::new())
    });

    if let Some(Value::Object(disk_setup)) = cfg.get("disk_setup") {
        let mut disk_setup = disk_setup.clone();
        update_disk_setup_devices(&mut disk_setup, log, &aliases)?;
        log.debug(
            SOURCE,
            &format!(
                "Partitioning disks: {}",
                py_str(&Value::Object(disk_setup.clone()))
            ),
        );
        for (disk, definition) in disk_setup.clone() {
            let Value::Object(definition) = definition else {
                log.warning(SOURCE, &format!("Invalid disk definition for {disk}"));
                continue;
            };

            if let Err(error) = mkpart(host, log, &disk, &definition) {
                logexc(log, &format!("Failed partitioning operation\n{error}"));
            }
        }
    }

    if let Some(Value::Array(fs_setup)) = cfg.get("fs_setup") {
        let mut fs_setup = fs_setup.clone();
        log.debug(
            SOURCE,
            &format!(
                "setting up filesystems: {}",
                py_str(&Value::Array(fs_setup.clone()))
            ),
        );
        update_fs_setup_devices(&mut fs_setup, log, &aliases)?;
        for definition in fs_setup {
            let Value::Object(definition) = definition else {
                log.warning(
                    SOURCE,
                    &format!("Invalid file system definition: {}", py_str(&definition)),
                );
                continue;
            };

            if let Err(error) = mkfs(host, log, &definition) {
                logexc(log, &format!("Failed during filesystem operation\n{error}"));
            }
        }
    }
    Ok(())
}

/// `handle`.
///
/// # Errors
/// Never: upstream catches every failure per disk and per filesystem, and the
/// two it does not catch are config type errors that end the module rather
/// than the boot.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    if args.root != std::path::Path::new("/") {
        // Nothing here is rooted, so a test root would reach the real disks.
        return Ok(());
    }
    let cfg = args.cfg.clone();
    let mut host = Live;
    let _ = handle_with(&cfg, &mut host, args.logger);
    Ok(())
}

/// The real machine.
#[derive(Debug)]
pub struct Live;

impl Host for Live {
    fn subp(
        &mut self,
        argv: &[String],
        data: Option<&str>,
        env: &[(&str, &str)],
        rcs: &[i32],
    ) -> Result<(String, String), ProcError> {
        let mut command = ci_sys::subp::Subp::new(argv);
        for (key, value) in env {
            command = command.env(key, value);
        }
        if let Some(data) = data {
            command = command.stdin(data.as_bytes().to_vec());
        }
        finish(argv, command, rcs)
    }

    fn subp_shell(&mut self, command: &str) -> Result<(String, String), ProcError> {
        let argv = vec![command.to_owned()];
        finish(
            &argv,
            ci_sys::subp::Subp::new(["/bin/sh", "-c", command]),
            &[0],
        )
    }

    fn which(&mut self, program: &str) -> Option<String> {
        ci_sys::subp::which(program).map(|path| path.to_string_lossy().into_owned())
    }

    fn exists(&mut self, path: &str) -> bool {
        std::fs::metadata(path).is_ok()
    }

    fn is_block_device(&mut self, path: &str) -> bool {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_block_device())
    }

    fn realpath(&mut self, path: &str) -> String {
        std::fs::canonicalize(path).map_or_else(
            |_| path.to_owned(),
            |resolved| resolved.to_string_lossy().into_owned(),
        )
    }

    fn wipe_ends(&mut self, device: &str) -> Result<(), String> {
        use std::io::{Seek, SeekFrom, Write};

        // `start_len` and `end_len`, which upstream sets to a mebibyte each.
        const LENGTH: usize = 1024 * 1024;
        let zeros = vec![0u8; LENGTH];
        let back = i64::try_from(LENGTH).unwrap_or(i64::MAX);
        let path = std::path::Path::new(device);
        let oserror = |error: &std::io::Error| ci_core::pyerr::oserror(error, path);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| oserror(&error))?;
        file.write_all(&zeros).map_err(|error| oserror(&error))?;
        file.seek(SeekFrom::End(-back))
            .map_err(|error| oserror(&error))?;
        file.write_all(&zeros).map_err(|error| oserror(&error))?;
        file.flush().map_err(|error| oserror(&error))?;
        Ok(())
    }
}

/// Run a built command and turn a disallowed exit code into a [`ProcError`],
/// which is where `subp.subp`'s `rcs` argument lands.
fn finish(
    argv: &[String],
    command: ci_sys::subp::Subp,
    rcs: &[i32],
) -> Result<(String, String), ProcError> {
    let out = command.run().map_err(|error| ProcError {
        argv: argv.to_vec(),
        exit_code: None,
        stdout: String::new(),
        stderr: error.to_string(),
    })?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if out.code.is_some_and(|code| rcs.contains(&code)) {
        return Ok((stdout, stderr));
    }
    Err(ProcError {
        argv: argv.to_vec(),
        exit_code: out.code,
        stdout,
        stderr,
    })
}

/// A [`Host`] whose every answer is set up front.
///
/// This is what `dump-cc-disk-setup` drives. A question with no scripted
/// answer gets the same "no" on both sides rather than a panic, because the
/// cases worth having are the ones where the two implementations ask
/// *different* questions.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// Keyed by the argv joined with spaces.
    pub commands: Vec<(String, CommandResult)>,
    /// Keyed by the whole command line, for the `shell=True` call `mkfs`
    /// makes when `fs_setup: cmd` is set.
    pub shell: Vec<(String, CommandResult)>,
    /// Program name to the path `subp.which` answers with.
    pub which: Vec<(String, String)>,
    pub exists: Vec<String>,
    pub block: Vec<String>,
    pub realpath: Vec<(String, String)>,
    /// Devices whose end-wipe fails, and the `OSError` text it fails with.
    pub wipe: Vec<(String, String)>,
    /// What the module asked, in order.
    pub calls: Vec<String>,
}

impl Fixture {
    fn record(&mut self, call: String) {
        self.calls.push(call);
    }

    fn lookup<'a, T>(table: &'a [(String, T)], key: &str) -> Option<&'a T> {
        table
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// The answer a scripted command gives, once its exit code has been
    /// checked against the caller's `rcs`.
    fn answer(
        argv: &[String],
        result: Option<&CommandResult>,
        rcs: &[i32],
    ) -> Result<(String, String), ProcError> {
        let Some(result) = result else {
            return Err(ProcError {
                argv: argv.to_vec(),
                exit_code: Some(127),
                stdout: String::new(),
                stderr: format!(
                    "{}: not found",
                    argv.first().cloned().unwrap_or_default()
                ),
            });
        };
        if rcs.contains(&result.exit_code) {
            return Ok((result.stdout.clone(), result.stderr.clone()));
        }
        Err(ProcError {
            argv: argv.to_vec(),
            exit_code: Some(result.exit_code),
            stdout: result.stdout.clone(),
            stderr: result.stderr.clone(),
        })
    }
}

impl Host for Fixture {
    fn subp(
        &mut self,
        argv: &[String],
        data: Option<&str>,
        env: &[(&str, &str)],
        rcs: &[i32],
    ) -> Result<(String, String), ProcError> {
        let key = argv.join(" ");
        let shown: Vec<String> = env
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        let codes: Vec<String> = rcs.iter().map(ToString::to_string).collect();
        self.record(format!(
            "subp {key} env={} data={} rcs={}",
            shown.join(","),
            data.map_or_else(
                || "-".to_owned(),
                |text| serde_json::to_string(text).unwrap_or_default()
            ),
            codes.join(","),
        ));
        let result = Self::lookup(&self.commands, &key).cloned();
        Self::answer(argv, result.as_ref(), rcs)
    }

    fn subp_shell(&mut self, command: &str) -> Result<(String, String), ProcError> {
        self.record(format!("shell {command}"));
        let result = Self::lookup(&self.shell, command).cloned();
        Self::answer(&[command.to_owned()], result.as_ref(), &[0])
    }

    fn which(&mut self, program: &str) -> Option<String> {
        self.record(format!("which {program}"));
        Self::lookup(&self.which, program).cloned()
    }

    fn exists(&mut self, path: &str) -> bool {
        self.record(format!("exists {path}"));
        self.exists.iter().any(|name| name == path)
    }

    fn is_block_device(&mut self, path: &str) -> bool {
        self.record(format!("isblk {path}"));
        self.block.iter().any(|name| name == path)
    }

    fn realpath(&mut self, path: &str) -> String {
        self.record(format!("realpath {path}"));
        Self::lookup(&self.realpath, path)
            .cloned()
            .unwrap_or_else(|| path.to_owned())
    }

    fn wipe_ends(&mut self, device: &str) -> Result<(), String> {
        self.record(format!("wipe {device}"));
        match Self::lookup(&self.wipe, device) {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn commands(items: &[(&str, &str)]) -> Vec<(String, CommandResult)> {
        items
            .iter()
            .map(|(key, stdout)| {
                (
                    (*key).to_owned(),
                    CommandResult {
                        exit_code: 0,
                        stdout: (*stdout).to_owned(),
                        stderr: String::new(),
                    },
                )
            })
            .collect()
    }

    /// A machine with one blank spare disk and every tool installed.
    fn machine() -> Fixture {
        Fixture {
            commands: commands(&[
                ("udevadm settle", ""),
                ("partprobe /dev/sdb", ""),
                (
                    "lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb --nodeps",
                    "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"\" LABEL=\"\"\n",
                ),
                (
                    "lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb",
                    "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"\" LABEL=\"\"\n",
                ),
                ("sgdisk -p /dev/sdb", "Number  Start End Size Code Name\n"),
                ("blockdev --getsize64 /dev/sdb", "429496729600\n"),
                ("blockdev --getss /dev/sdb", "512\n"),
                ("sgdisk -Z /dev/sdb", ""),
                ("sgdisk -n 1:0:0 /dev/sdb", ""),
                ("sgdisk -n 1:0:+419430400 /dev/sdb", ""),
                ("sgdisk -n 2:0:0 /dev/sdb", ""),
                ("sgdisk -t 1:8300 /dev/sdb", ""),
                ("sgdisk -t 2:8200 /dev/sdb", ""),
            ]),
            which: pairs(&[
                ("sgdisk", "/usr/sbin/sgdisk"),
                ("partprobe", "/usr/sbin/partprobe"),
                ("udevadm", "/usr/bin/udevadm"),
            ]),
            exists: owned(&["/dev/sdb"]),
            ..Fixture::default()
        }
    }

    fn run(cfg: &serde_json::Value, host: &mut Fixture) -> Vec<String> {
        let mut log = ci_log::Logger::capturing();
        let cfg = cfg.as_object().unwrap().clone();
        handle_with(&cfg, host, &mut log).unwrap();
        log.captured().to_vec()
    }

    #[test]
    fn a_two_partition_gpt_layout_becomes_four_sgdisk_calls() {
        let mut host = machine();
        run(
            &serde_json::json!({
                "disk_setup": {"/dev/sdb": {
                    "table_type": "gpt",
                    "layout": [[50, 83], [50, 82]],
                    "overwrite": true,
                }},
            }),
            &mut host,
        );

        let sgdisk: Vec<&String> = host
            .calls
            .iter()
            .filter(|call| {
                call.contains("subp sgdisk -") && !call.contains("sgdisk -p")
            })
            .collect();
        assert_eq!(
            sgdisk,
            [
                "subp sgdisk -Z /dev/sdb env= data=- rcs=0",
                // 50% of 838860800 sectors, as `+sectors` from 0.
                "subp sgdisk -n 1:0:+419430400 /dev/sdb env= data=- rcs=0",
                "subp sgdisk -t 1:8300 /dev/sdb env= data=- rcs=0",
                // The last one takes what is left, whatever its share said.
                "subp sgdisk -n 2:0:0 /dev/sdb env= data=- rcs=0",
                "subp sgdisk -t 2:8200 /dev/sdb env= data=- rcs=0",
            ]
        );
    }

    #[test]
    fn the_partition_type_is_padded_to_four_characters() {
        // `82` is `8200`; a name is left alone and reaches sgdisk as written.
        assert_eq!(ljust("82", 4, '0'), "8200");
        assert_eq!(ljust("Linux", 4, '0'), "Linux");
        assert_eq!(ljust("", 4, '0'), "0000");
    }

    #[test]
    fn two_digit_and_four_digit_types_meet_as_guids() {
        assert!(partition_type_matches("83", "8300").unwrap());
        assert!(
            partition_type_matches("8300", "0FC63DAF-8483-4772-8E79-3D69D8477DE4")
                .unwrap()
        );
        assert!(!partition_type_matches(
            "8200",
            "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
        )
        .unwrap());
        // A four-character code with no GUID behind it, named in the message
        // that B87 says is the wrong way round.
        assert_eq!(
            partition_type_matches("FFFF", "0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
            Err("Cannot find GPT GUID for found type FFFF".to_owned())
        );
        // Two four-character codes are compared as they are; the GUID
        // promotion only happens when one side is already a GUID.
        assert!(!partition_type_matches("8300", "FFFF").unwrap());
        assert_eq!(
            partition_type_matches("0FC63DAF-8483-4772-8E79-3D69D8477DE4", "FFFF"),
            Err("Cannot find GPT GUID for expected type \
                 0FC63DAF-8483-4772-8E79-3D69D8477DE4"
                .to_owned())
        );
        assert_eq!(
            partition_type_matches("830", "8300"),
            Err("Unknown partition type found: 830".to_owned())
        );
        assert_eq!(
            partition_type_matches("8300", "830"),
            Err("Unknown partition type specified: 8300".to_owned())
        );
    }

    #[test]
    fn an_lsblk_value_with_an_equals_sign_in_it_ends_the_walk() {
        // B87: `x.split("=")` is unpacked into exactly two names.
        assert_eq!(
            value_splitter("NAME=\"sdb\" LABEL=\"a=b\"", 0),
            Err("too many values to unpack (expected 2, got 3)".to_owned())
        );
        assert_eq!(
            value_splitter("NAME=\"sdb\" bare", 0),
            Err("not enough values to unpack (expected 2, got 1)".to_owned())
        );
        assert_eq!(
            value_splitter("/dev/sdb: TYPE=\"ext4\"", 1),
            Ok(vec![("TYPE".to_owned(), "ext4".to_owned())])
        );
    }

    #[test]
    fn a_configured_command_is_filled_in_by_name() {
        let mapping = [
            ("label", "data".to_owned()),
            ("filesystem", "ext4".to_owned()),
            ("device", "/dev/sdb1".to_owned()),
        ];
        assert_eq!(
            percent_format("mkfs -t %(filesystem)s -L %(label)s %(device)s", &mapping),
            Ok("mkfs -t ext4 -L data /dev/sdb1".to_owned())
        );
        assert_eq!(
            percent_format("echo 100%% of %(device)s", &mapping),
            Ok("echo 100% of /dev/sdb1".to_owned())
        );
        // An unknown key is a KeyError, whose text is just the key.
        assert_eq!(
            percent_format("mkfs %(nope)s", &mapping),
            Err("'nope'".to_owned())
        );
    }

    #[test]
    fn a_disk_that_is_already_laid_out_is_left_alone() {
        let mut host = machine();
        host.commands.push((
            "sgdisk -p /dev/sdb".to_owned(),
            CommandResult {
                exit_code: 0,
                stdout: "Number Start End Size Code Name\n 1 2048 838860766 400G 8300 Linux\n"
                    .to_owned(),
                stderr: String::new(),
            },
        ));
        // The later entry wins in the fixture's lookup order, so drop the
        // blank one the machine ships with.
        host.commands.retain(|(name, result)| {
            name != "sgdisk -p /dev/sdb" || result.stdout.contains("2048")
        });

        let log = run(
            &serde_json::json!({
                "disk_setup": {"/dev/sdb": {"table_type": "gpt", "layout": true}},
            }),
            &mut host,
        );

        assert!(log
            .iter()
            .any(|line| line.ends_with("Device partitioning layout matches")));
        assert!(!host.calls.iter().any(|call| call.contains("sgdisk -Z")));
    }

    #[test]
    fn the_end_of_a_disk_is_zeroed_as_well_as_the_start() {
        // `purge_disk_ptable` writes a mebibyte at each end because GPT keeps
        // a copy of the table at the back. Nothing else in the file changes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        let size = 4 * 1024 * 1024;
        std::fs::write(&path, vec![0xAAu8; size]).unwrap();

        Live.wipe_ends(path.to_str().unwrap()).unwrap();

        let written = std::fs::read(&path).unwrap();
        assert_eq!(written.len(), size);
        let mib = 1024 * 1024;
        assert!(written[..mib].iter().all(|byte| *byte == 0));
        assert!(written[mib..size - mib].iter().all(|byte| *byte == 0xAA));
        assert!(written[size - mib..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn a_disk_that_is_not_there_is_not_partitioned() {
        let mut host = machine();
        host.exists.clear();

        let log = run(
            &serde_json::json!({
                "disk_setup": {"/dev/sdb": {"table_type": "gpt", "layout": true}},
            }),
            &mut host,
        );

        assert!(log.iter().any(|line| line.contains(
            "Device /dev/sdb did not exist and was not created with a udevadm settle."
        )));
        assert!(!host.calls.iter().any(|call| call.contains("sgdisk")));
    }

    #[test]
    fn every_gdisk_code_maps_to_a_guid_of_the_right_shape() {
        assert_eq!(SGDISK_TO_GPT_ID.len(), 362);
        assert_eq!(
            sgdisk_to_gpt_id("8200"),
            Some("0657FD6D-A4AB-43C4-84E5-0933C84B4F4F")
        );
        assert_eq!(sgdisk_to_gpt_id("ffff"), None);
        for (code, guid) in SGDISK_TO_GPT_ID {
            assert_eq!(code.len(), 4, "{code} is not a four-character code");
            assert_eq!(guid.len(), 36, "{guid} is not a GUID");
        }
    }
}
