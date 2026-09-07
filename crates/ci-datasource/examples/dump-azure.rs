//! Dump what the Azure identity and IMDS helpers produce, for comparison
//! against `tests/differential/azure.py`.
//!
//! Usage: dump-azure identity <syspath-dir>
//!        dump-azure swap <uuid>...
//!        dump-azure imds <base-url>
//!        dump-azure ovf <file>
//!        dump-azure report <timestamp> <vm-id|-> <kind> [argument...]
//!        dump-azure wire goalstate <file>
//!        dump-azure wire certs [endpoint]
//!        dump-azure wire filter-pubkeys <keys-json> <pubkey-info-json>
//!        dump-azure wire health <file> <ready|failure> [description]
//!        dump-azure wire minimal-ovf <username|-> <hostname> <bool|->
//!        dump-azure ds crawl <ovf-file>
//!        dump-azure ds imds <json-file>
//!        dump-azure ds pps <ovf-json> <imds-json> <reported-ready>
//!        dump-azure ds sshkey <key>
//!        dump-azure ds iid <system-uuid> <previous|->
//!        dump-azure ds subplatform <seed|->
//!        dump-azure ds dscfg <sys-cfg-json>
//!        dump-azure ds keys <metadata-json>
//!        dump-azure ds pubkeyinfo <cfg-json> <imds-json>
//!        dump-azure ds instanceid <metadata-json> <fallback>
//!        dump-azure ds region <metadata-json>
//!        dump-azure ds netconfig <sys-cfg-json> <imds-json> <nics-json>
//!        dump-azure netcfg config <network-json> <secondary> [nics-json]
//!        dump-azure netcfg driver <mac> <nics-json>
//!        dump-azure netcfg validate <imds-json> <nics-json> <primary-mac|->
//!        dump-azure netcfg mac <address>...
//!        dump-azure crawl <fixture-json>
//!
//! A crawl fixture is a JSON object standing in for the machine: see
//! `crawl_fixture` below for the keys, and `tests/differential/azure.py crawl`
//! for the same object driving the Python datasource.
//!
//! A nics JSON file is a list of `{"mac": ..., "driver": ...}` objects,
//! standing in for `net.get_interfaces()`.

use ci_datasource::azure::{
    certs, crawl, ds, errors, identity, imds, kvp, netcfg, ovf, wire,
};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut log = ci_log::Logger::silent();

    match args.split_first() {
        Some((mode, rest)) if mode == "identity" => {
            let Some(root) = rest.first() else {
                eprintln!("usage: dump-azure identity <syspath-dir>");
                return std::process::ExitCode::from(2);
            };
            let root = std::path::Path::new(root);
            let tag = ci_datasource::dmi::read_syspath_at(
                root,
                "chassis-asset-tag",
                &mut log,
            );
            println!(
                "asset-tag={}",
                show(
                    identity::classify_chassis_asset_tag(tag.as_deref(), &mut log)
                        .map(str::to_owned)
                )
            );
            let system_uuid =
                ci_datasource::dmi::read_syspath_at(root, "system-uuid", &mut log)
                    .map(|uuid| uuid.to_lowercase());
            println!("system-uuid={}", show(system_uuid.clone()));
            let vm_id = system_uuid.and_then(|uuid| {
                identity::convert_system_uuid_to_vm_id(
                    &uuid,
                    identity::is_vm_gen1(),
                    &mut log,
                )
            });
            println!("vm-id={}", show(vm_id));
            println!("gen1={}", identity::is_vm_gen1());
            std::process::ExitCode::SUCCESS
        }
        Some((mode, rest)) if mode == "swap" => {
            for uuid in rest {
                println!(
                    "{uuid} -> {}",
                    show(identity::byte_swap_system_uuid(uuid, &mut log))
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Some((mode, rest)) if mode == "imds" => {
            let Some(base) = rest.first() else {
                eprintln!("usage: dump-azure imds <base-url>");
                return std::process::ExitCode::from(2);
            };
            match imds::fetch_metadata_with_api_fallback(base, None, Some(3), &mut log)
            {
                Ok(value) => {
                    println!("{}", ci_core::json_dumps(&value));
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{}", error.message);
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Some((mode, rest)) if mode == "ovf" => {
            let Some(path) = rest.first() else {
                eprintln!("usage: dump-azure ovf <file>");
                return std::process::ExitCode::from(2);
            };
            let Ok(text) = std::fs::read_to_string(path) else {
                eprintln!("cannot read {path}");
                return std::process::ExitCode::from(2);
            };
            dump_ovf(&text, &mut log)
        }
        Some((mode, rest)) if mode == "report" => {
            dump_report(rest);
            std::process::ExitCode::SUCCESS
        }
        Some((mode, rest)) if mode == "wire" => dump_wire(rest, &mut log),
        Some((mode, rest)) if mode == "ds" => dump_ds(rest, &mut log),
        Some((mode, rest)) if mode == "netcfg" => dump_netcfg(rest, &mut log),
        Some((mode, rest)) if mode == "crawl" => dump_crawl(rest, &mut log),
        _ => {
            eprintln!(
                "usage: dump-azure \
                 <identity|swap|imds|ovf|report|wire|ds|netcfg|crawl> ..."
            );
            std::process::ExitCode::from(2)
        }
    }
}

/// `netcfg <config|driver|validate|mac> ...`
fn dump_netcfg(args: &[String], log: &mut ci_log::Logger) -> std::process::ExitCode {
    let arg = |index: usize| args.get(index).map_or("", String::as_str);
    let nics = |index: usize| read_nics(arg(index));
    let json = |index: usize| -> ci_config::Object {
        std::fs::read_to_string(arg(index))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    };

    match arg(0) {
        "config" => {
            match netcfg::generate_network_config(
                &json(1),
                arg(2) == "true",
                &nics(3),
                log,
            ) {
                Ok(config) => {
                    println!("config={}", ci_core::jsonfmt::json_dumps(&config));
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    println!("error={error}");
                    std::process::ExitCode::from(1)
                }
            }
        }
        "driver" => {
            let driver = netcfg::determine_device_driver_for_mac(arg(1), &nics(2), log);
            println!("driver={}", show(driver));
            std::process::ExitCode::SUCCESS
        }
        "validate" => {
            let primary = Some(arg(3)).filter(|value| *value != "-");
            println!(
                "valid={}",
                py_bool(netcfg::validate_imds_network_metadata(
                    &json(1),
                    &nics(2),
                    primary,
                    log
                ))
            );
            std::process::ExitCode::SUCCESS
        }
        "mac" => {
            for mac in args.iter().skip(1) {
                println!("mac={}", netcfg::normalize_mac_address(mac));
            }
            std::process::ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown netcfg mode {other}");
            std::process::ExitCode::from(2)
        }
    }
}

/// `ds <crawl|imds|pps|sshkey|iid|subplatform> ...`
fn dump_ds(args: &[String], log: &mut ci_log::Logger) -> std::process::ExitCode {
    let arg = |index: usize| args.get(index).map_or("", String::as_str);
    let json = |index: usize| -> ci_config::Object {
        std::fs::read_to_string(arg(index))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    };

    match arg(0) {
        "crawl" => {
            let Ok(text) = std::fs::read_to_string(arg(1)) else {
                eprintln!("cannot read {}", arg(1));
                return std::process::ExitCode::from(2);
            };
            match ds::read_azure_ovf(&text, log) {
                Ok(crawl) => {
                    println!(
                        "metadata={}",
                        ci_core::jsonfmt::json_dumps(&ci_config::Value::Object(
                            crawl.metadata
                        ))
                    );
                    println!("userdata={}", ci_core::b64::encode(&crawl.userdata));
                    println!(
                        "config={}",
                        ci_core::jsonfmt::json_dumps(&ci_config::Value::Object(
                            crawl.config
                        ))
                    );
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    println!("error={error}");
                    std::process::ExitCode::from(1)
                }
            }
        }
        "imds" => {
            let md = json(1);
            println!("username={}", ds::username_from_imds(&md).unwrap_or("None"));
            println!("userdata={}", ds::userdata_from_imds(&md).unwrap_or("None"));
            println!("hostname={}", ds::hostname_from_imds(&md).unwrap_or("None"));
            println!(
                "disable-password={}",
                ds::disable_password_from_imds(&md)
                    .map_or_else(|| "None".to_owned(), py_bool)
            );
            println!("ppstype={}", ds::ppstype_from_imds(&md).unwrap_or("None"));
            match ds::public_keys_from_imds(&md, log) {
                Some(keys) => {
                    for key in keys {
                        println!("key={key}");
                    }
                }
                None => println!("keys=None"),
            }
            std::process::ExitCode::SUCCESS
        }
        "pps" => {
            let kind =
                ds::determine_pps_type(&json(1), &json(2), arg(3) == "true", log);
            println!("pps={kind}");
            std::process::ExitCode::SUCCESS
        }
        "sshkey" => {
            println!("openssh={}", py_bool(ds::key_is_openssh_formatted(arg(1))));
            std::process::ExitCode::SUCCESS
        }
        "iid" => {
            let previous = Some(arg(2)).filter(|value| *value != "-");
            println!("iid={}", ds::iid(arg(1), previous, log));
            std::process::ExitCode::SUCCESS
        }
        "subplatform" => {
            let seed = Some(arg(1)).filter(|value| *value != "-");
            println!("subplatform={}", ds::subplatform(seed));
            std::process::ExitCode::SUCCESS
        }
        other => dump_ds_config(other, args, log),
    }
}

/// The half of `ds` that works off the datasource config and the accessors.
fn dump_ds_config(
    mode: &str,
    args: &[String],
    log: &mut ci_log::Logger,
) -> std::process::ExitCode {
    let arg = |index: usize| args.get(index).map_or("", String::as_str);
    let json = |index: usize| -> ci_config::Object {
        std::fs::read_to_string(arg(index))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    };

    match mode {
        "dscfg" => {
            let cfg = ds::ds_config(&json(1));
            println!(
                "dscfg={}",
                ci_core::jsonfmt::json_dumps(&ci_config::Value::Object(cfg.clone()))
            );
            println!(
                "ephemeral0={}",
                show(ds::device_name_to_device(&cfg, "ephemeral0").map(str::to_owned))
            );
            std::process::ExitCode::SUCCESS
        }
        "keys" => {
            let md = json(1);
            for key in ds::public_ssh_keys(&md, log) {
                println!("key={key}");
            }
            for key in ds::public_keys_from_ovf(&md, log) {
                println!("ovf-key={key}");
            }
            std::process::ExitCode::SUCCESS
        }
        "pubkeyinfo" => {
            match ds::wireserver_pubkey_info(&json(1), &json(2), log) {
                Some(info) => println!(
                    "pubkeys={}",
                    ci_core::jsonfmt::json_dumps(&ci_config::Value::Array(info))
                ),
                None => println!("pubkeys=None"),
            }
            std::process::ExitCode::SUCCESS
        }
        "instanceid" => {
            println!("iid={}", ds::instance_id(&json(1), arg(2)));
            std::process::ExitCode::SUCCESS
        }
        "region" => {
            let md = json(1);
            println!("region={}", show_value(ds::region(&md)));
            println!("zone={}", show_value(ds::availability_zone(&md)));
            std::process::ExitCode::SUCCESS
        }
        "netconfig" => {
            let cfg = ds::ds_config(&json(1));
            let imds = json(2);
            let nics = read_nics(arg(3));
            match ds::generate_network_config(&cfg, Some(&imds), &nics, log) {
                Some(config) => {
                    println!("config={}", ci_core::jsonfmt::json_dumps(&config));
                }
                None => println!("config=None"),
            }
            std::process::ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown ds mode {other}");
            std::process::ExitCode::from(2)
        }
    }
}

/// A stand-in for `net.get_interfaces()`, read from a JSON fixture.
fn read_nics(path: &str) -> Vec<netcfg::Interface> {
    let parsed: Vec<ci_config::Value> = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    parsed
        .iter()
        .map(|entry| netcfg::Interface {
            mac: entry
                .get("mac")
                .and_then(ci_config::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            driver: entry
                .get("driver")
                .and_then(ci_config::Value::as_str)
                .map(str::to_owned),
        })
        .collect()
}

fn show_value(value: Option<&ci_config::Value>) -> String {
    value.map_or_else(
        || "None".to_owned(),
        |value| {
            value
                .as_str()
                .map_or_else(|| value.to_string(), ToOwned::to_owned)
        },
    )
}

fn py_bool(value: bool) -> String {
    if value {
        "True".to_owned()
    } else {
        "False".to_owned()
    }
}

/// `wire <goalstate|health|minimal-ovf> ...`
#[allow(clippy::too_many_lines)]
fn dump_wire(args: &[String], log: &mut ci_log::Logger) -> std::process::ExitCode {
    let arg = |index: usize| args.get(index).map_or("", String::as_str);
    let read = |index: usize| std::fs::read_to_string(arg(index));

    match arg(0) {
        // Live modes. `fetch` prints the raw goal state so the certificates
        // document it points at can be pulled with `get`; both are GETs, so
        // neither mutates platform state.
        "fetch" | "get" => {
            let endpoint = if arg(1).is_empty() {
                wire::DEFAULT_ENDPOINT
            } else {
                arg(1)
            };
            let client = wire::Client::new(endpoint).with_budget(
                std::time::Duration::from_secs(20),
                std::time::Duration::from_secs(1),
            );
            let fetched = if arg(0) == "get" {
                client.fetch_url(arg(2), log)
            } else {
                client.fetch_goal_state_raw(log)
            };
            match fetched {
                Ok(body) => {
                    print!("{}", String::from_utf8_lossy(&body));
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    println!("error={error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        "goalstate" => {
            let Ok(text) = read(1) else {
                eprintln!("cannot read {}", arg(1));
                return std::process::ExitCode::from(2);
            };
            match wire::GoalState::parse(&text) {
                Ok(state) => {
                    println!("incarnation={}", state.incarnation);
                    println!("container-id={}", state.container_id);
                    println!("instance-id={}", state.instance_id);
                    println!(
                        "certificates-url={}",
                        state.certificates_url.unwrap_or_else(|| "None".to_owned())
                    );
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    println!("error={error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        // Live, and still only GETs: generate a transport certificate, fetch
        // the goal state with it, and decrypt the document it names.
        "certs" => {
            let endpoint = if arg(1).is_empty() {
                wire::DEFAULT_ENDPOINT
            } else {
                arg(1)
            };
            let transport = match certs::Transport::generate(log) {
                Ok(transport) => transport,
                Err(error) => {
                    println!("error={error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let client = wire::Client::new(endpoint)
                .with_certificate(transport.certificate())
                .with_budget(
                    std::time::Duration::from_secs(20),
                    std::time::Duration::from_secs(1),
                );
            let goal_state = match client.fetch_goal_state(log) {
                Ok(goal_state) => goal_state,
                Err(error) => {
                    println!("error={error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let Some(document) = goal_state.certificates_xml else {
                println!("certificates-xml=None");
                return std::process::ExitCode::SUCCESS;
            };
            match transport.parse_certificates(&document) {
                Ok(keys) => {
                    for (fingerprint, key) in keys {
                        println!("{fingerprint}={}", key.trim_end());
                    }
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    println!("error={error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        "filter-pubkeys" => {
            let Ok(keys) = read(1) else {
                eprintln!("cannot read {}", arg(1));
                return std::process::ExitCode::from(2);
            };
            let Ok(info) = read(2) else {
                eprintln!("cannot read {}", arg(2));
                return std::process::ExitCode::from(2);
            };
            let keys: std::collections::BTreeMap<String, String> =
                serde_json::from_str::<ci_config::Object>(&keys)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, value)| {
                        (name, value.as_str().unwrap_or_default().to_owned())
                    })
                    .collect();
            let info: Vec<ci_config::Value> =
                serde_json::from_str(&info).unwrap_or_default();
            for key in certs::filter_pubkeys(&keys, &info, log) {
                println!("{key}");
            }
            std::process::ExitCode::SUCCESS
        }
        "health" => {
            let Ok(text) = read(1) else {
                eprintln!("cannot read {}", arg(1));
                return std::process::ExitCode::from(2);
            };
            let Ok(state) = wire::GoalState::parse(&text) else {
                eprintln!("unparseable goal state");
                return std::process::ExitCode::from(2);
            };
            let document = if arg(2) == "failure" {
                wire::build_report(
                    &state,
                    "NotReady",
                    Some("ProvisioningFailed"),
                    arg(3),
                )
            } else {
                wire::build_report(&state, "Ready", None, "")
            };
            print!("{}", String::from_utf8_lossy(&document));
            std::process::ExitCode::SUCCESS
        }
        // The one mode that writes to the platform: fetch the live goal state
        // and POST a Ready health report at it. Guarded by an explicit token
        // so it cannot be reached from the differential harness, which drives
        // every other mode here, and refuses to send anything but Ready --
        // a NotReady report is what tells Azure a deployment failed.
        "report-ready" => {
            if arg(1) != "--i-am-on-the-vm-and-mean-it" {
                eprintln!(
                    "wire report-ready POSTs to the Azure fabric; pass \
                     --i-am-on-the-vm-and-mean-it to confirm"
                );
                return std::process::ExitCode::from(2);
            }
            let endpoint = if arg(2).is_empty() {
                wire::DEFAULT_ENDPOINT
            } else {
                arg(2)
            };
            let client = wire::Client::new(endpoint).with_budget(
                std::time::Duration::from_secs(20),
                std::time::Duration::from_secs(1),
            );
            let goal_state = match client.fetch_goal_state(log) {
                Ok(goal_state) => goal_state,
                Err(error) => {
                    println!("error=goal-state: {error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let document = wire::build_report(&goal_state, "Ready", None, "");
            println!("url=http://{endpoint}/machine?comp=health");
            println!("incarnation={}", goal_state.incarnation);
            println!("container-id={}", goal_state.container_id);
            println!("instance-id={}", goal_state.instance_id);
            println!("--- body ---");
            print!("{}", String::from_utf8_lossy(&document));
            println!("--- end ---");
            match client.post_health_report(document, log) {
                Ok(()) => {
                    println!("posted=ok");
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    println!("posted=error: {error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        // `NotReady`/`ProvisioningFailed` is how a deployment is marked failed,
        // so this carries a second token of its own and never shares one with
        // `report-ready`.
        "report-failure" => {
            if arg(1) != "--i-am-on-the-vm-and-mean-it"
                || arg(2) != "--this-reports-the-vm-failed"
            {
                eprintln!(
                    "wire report-failure tells the Azure fabric this VM failed \
                     to provision; pass --i-am-on-the-vm-and-mean-it \
                     --this-reports-the-vm-failed <encoded-report> [endpoint] \
                     to confirm"
                );
                return std::process::ExitCode::from(2);
            }
            let encoded_report = arg(3);
            if encoded_report.is_empty() {
                eprintln!("wire report-failure needs an encoded report");
                return std::process::ExitCode::from(2);
            }
            let endpoint = if arg(4).is_empty() {
                wire::DEFAULT_ENDPOINT
            } else {
                arg(4)
            };
            let client = wire::Client::new(endpoint).with_budget(
                std::time::Duration::from_secs(20),
                std::time::Duration::from_secs(1),
            );
            let goal_state = match client.fetch_goal_state(log) {
                Ok(goal_state) => goal_state,
                Err(error) => {
                    println!("error=goal-state: {error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let document = wire::build_report(
                &goal_state,
                "NotReady",
                Some("ProvisioningFailed"),
                encoded_report,
            );
            println!("url=http://{endpoint}/machine?comp=health");
            println!("incarnation={}", goal_state.incarnation);
            println!("container-id={}", goal_state.container_id);
            println!("instance-id={}", goal_state.instance_id);
            println!("--- body ---");
            print!("{}", String::from_utf8_lossy(&document));
            println!("--- end ---");
            match client.post_health_report(document, log) {
                Ok(()) => {
                    println!("posted=ok");
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    println!("posted=error: {error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        "minimal-ovf" => {
            let username = if arg(1) == "-" { None } else { Some(arg(1)) };
            let disable = match arg(3) {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            let document = wire::build_minimal_ovf(username, arg(2), disable);
            print!("{}", String::from_utf8_lossy(&document));
            std::process::ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown wire mode {other}");
            std::process::ExitCode::from(2)
        }
    }
}

/// `report <timestamp> <vm-id|-> <kind> [argument...]`
fn dump_report(args: &[String]) {
    let Some((stamp, rest)) = args.split_first() else {
        return;
    };
    let (vm_id, rest) = match rest.split_first() {
        Some((id, rest)) => (if id == "-" { None } else { Some(id.as_str()) }, rest),
        None => return,
    };
    let arg = |index: usize| rest.get(index).map_or("", String::as_str);

    if arg(0) == "success" {
        println!("{}", kvp::success_report(vm_id, stamp));
        return;
    }

    let mut error = match arg(0) {
        "ovf-invalid" => errors::ovf_invalid_metadata(arg(1)),
        "ovf-parsing" => errors::ovf_parsing_exception(arg(1)),
        "os-disk-pps" => errors::os_disk_pps_failure(),
        "proxy-missing" => errors::proxy_agent_not_found(),
        "proxy-status" => errors::proxy_agent_status_failure(
            arg(1).parse().unwrap_or(0),
            arg(2),
            arg(3),
        ),
        "vm-id" => errors::vm_identification(arg(1), Some(arg(2))),
        "imds-parsing" => errors::imds_metadata_parsing_exception(arg(1)),
        "imds-invalid" => {
            let value = serde_json::from_str(arg(2)).unwrap_or(ci_config::Value::Null);
            errors::imds_invalid_metadata(arg(1), &value)
        }
        "imds-url" => errors::imds_url_error(
            &ci_url::Error {
                url: arg(1).to_owned(),
                code: arg(2).parse().ok(),
                message: arg(3).to_owned(),
                tls: false,
            },
            arg(4).parse().unwrap_or(0.0),
        ),
        other => {
            eprintln!("unknown report kind {other}");
            return;
        }
    };
    error.timestamp.clone_from(stamp);
    println!("{}", error.as_encoded_report(vm_id));
}

fn dump_ovf(text: &str, log: &mut ci_log::Logger) -> std::process::ExitCode {
    let env = match ovf::parse_text(text, log) {
        Ok(env) => env,
        Err(error) => {
            // Only the messages the port reproduces verbatim are printed; a
            // parse failure's wording is our own.
            match &error {
                ovf::Error::Parsing(_) => println!("error=parsing"),
                ovf::Error::NonAzure(message) => {
                    println!("error=non-azure: {message}");
                }
                ovf::Error::InvalidMetadata(_) => {
                    println!("error=invalid-metadata: {error}");
                }
            }
            return std::process::ExitCode::FAILURE;
        }
    };

    println!("hostname={}", show(env.hostname));
    println!("username={}", show(env.username));
    println!("password={}", show(env.password));
    println!(
        "custom-data={}",
        show(env.custom_data.map(|data| ci_core::b64::encode(&data)))
    );
    println!(
        "disable-ssh-password-auth={}",
        show(env.disable_ssh_password_auth.map(|flag| flag.to_string()))
    );
    println!("preprovisioned-vm={}", env.preprovisioned_vm);
    println!(
        "preprovisioned-vm-type={}",
        show(env.preprovisioned_vm_type)
    );
    println!(
        "provision-guest-proxy-agent={}",
        env.provision_guest_proxy_agent
    );
    for key in env.public_keys {
        println!(
            "key fingerprint={} path={} value={}",
            show(key.fingerprint),
            show(key.path),
            key.value
        );
    }
    std::process::ExitCode::SUCCESS
}

fn show(value: Option<String>) -> String {
    value.unwrap_or_else(|| "<none>".to_owned())
}

/// Build the fixture machine from `fixture-json`.
///
/// The keys are the ones `tests/differential/azure.py crawl` monkeypatches
/// onto the Python datasource, so one file drives both sides.
fn crawl_fixture(doc: &ci_config::Object) -> Result<crawl::Fixture, String> {
    let str_at = |key: &str| doc.get(key).and_then(ci_config::Value::as_str);
    let bool_at =
        |key: &str| doc.get(key).and_then(ci_config::Value::as_bool) == Some(true);
    let obj_at = |key: &str| {
        doc.get(key)
            .and_then(ci_config::Value::as_object)
            .cloned()
            .unwrap_or_default()
    };

    let mut candidates = Vec::new();
    if let Some(list) = doc.get("candidates").and_then(ci_config::Value::as_array) {
        for entry in list {
            let Some(entry) = entry.as_object() else {
                return Err("candidate is not an object".to_owned());
            };
            let path = entry
                .get("path")
                .and_then(ci_config::Value::as_str)
                .ok_or("candidate has no path")?;
            let path = std::path::PathBuf::from(path);
            candidates.push(
                if entry.get("kind").and_then(ci_config::Value::as_str)
                    == Some("device")
                {
                    crawl::Source::Device(path)
                } else {
                    crawl::Source::Dir(path)
                },
            );
        }
    }

    // Sources hold OVF text rather than a parsed document, so both sides run
    // their own parser and the crawl covers `read_azure_ovf` too.
    let mut sources = Vec::new();
    if let Some(map) = doc.get("sources").and_then(ci_config::Value::as_object) {
        for (key, text) in map {
            let text = text
                .as_str()
                .ok_or_else(|| format!("source {key} is not a string"))?;
            sources.push((key.clone(), text.to_owned()));
        }
    }

    let strings = |key: &str| -> Vec<String> {
        doc.get(key)
            .and_then(ci_config::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(ci_config::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };

    Ok(crawl::Fixture {
        system_uuid: match str_at("system_uuid") {
            Some(uuid) => Ok(uuid.to_owned()),
            None => Err(str_at("system_uuid_error")
                .unwrap_or("failed to read system-uuid")
                .to_owned()),
        },
        gen1: bool_at("gen1"),
        candidates,
        sources,
        unmountable: strings("unmountable"),
        networking_up: bool_at("networking_up"),
        imds: obj_at("imds"),
        reported_ready_marker: bool_at("reported_ready_marker"),
        report_ready: match str_at("report_ready_error") {
            Some(message) => Err(message.to_owned()),
            None => Ok(strings("report_ready")),
        },
        previous_instance_id: str_at("previous_instance_id").map(str::to_owned),
        random_seed: str_at("random_seed").map(str::to_owned),
        calls: Vec::new(),
    })
}

fn dump_crawl(args: &[String], log: &mut ci_log::Logger) -> std::process::ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: dump-azure crawl <fixture-json>");
        return std::process::ExitCode::from(2);
    };
    let doc: ci_config::Object = match std::fs::read_to_string(path)
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
    {
        Ok(doc) => doc,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut platform = match crawl_fixture(&doc) {
        Ok(platform) => platform,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let data_dir = doc
        .get("data_dir")
        .and_then(ci_config::Value::as_str)
        .unwrap_or("/var/lib/waagent");
    let negotiated =
        doc.get("negotiated").and_then(ci_config::Value::as_bool) == Some(true);

    // The crawl's own success report goes to the Hyper-V pool, which is a
    // host mutation; a dump has no business making one.
    let mut reporter = ci_report::Reporter::silent();
    let result = crawl::crawl_metadata(
        &mut platform,
        std::path::Path::new(data_dir),
        negotiated,
        &mut reporter,
        log,
    );

    // The calls come first so a divergence in ordering is visible even when
    // the crawl went on to fail.
    for call in &platform.calls {
        println!("call {call}");
    }
    match result {
        Ok(crawled) => {
            println!("seed={}", crawled.seed);
            println!("userdata={}", ci_core::b64::encode(&crawled.userdata));
            for (label, value) in [
                ("metadata", &crawled.metadata),
                ("cfg", &crawled.cfg),
                ("files", &crawled.files),
            ] {
                println!(
                    "{label}={}",
                    ci_core::jsonfmt::json_dumps(&ci_config::Value::Object(
                        value.clone()
                    ))
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            println!("error={error}");
            std::process::ExitCode::FAILURE
        }
    }
}
