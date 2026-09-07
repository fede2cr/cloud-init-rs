#!/usr/bin/env python3
"""Dump what `cloudinit.sources.azure.identity` and `.imds` produce.

Paired with the `dump-azure` example in ci-datasource.

Usage: azure.py identity <syspath-dir>
       azure.py swap <uuid>...
       azure.py imds <base-url>
       azure.py ovf <file>
       azure.py report <timestamp> <vm-id|-> <kind> [argument...]
       azure.py wire <goalstate|health|minimal-ovf|filter-pubkeys> ...
       azure.py crawl <fixture-json>
"""
import base64
import json
import os
import sys
import tempfile
from xml.etree import ElementTree as ET

sys.path.insert(0, "/usr/lib/python3/dist-packages")

from cloudinit import dmi, subp, util, version  # noqa: E402
from cloudinit.atomic_helper import json_dumps  # noqa: E402
from cloudinit.sources import DataSourceAzure as azure_ds  # noqa: E402
from cloudinit.sources.azure import errors, identity, imds  # noqa: E402
from cloudinit.sources.helpers import azure as azure_helper  # noqa: E402
from cloudinit.sources.helpers.azure import (  # noqa: E402
    NonAzureDataSource,
    OvfEnvXml,
)
from cloudinit.net.dhcp import NoDHCPLeaseError  # noqa: E402
from cloudinit.url_helper import UrlError  # noqa: E402


def show(value):
    return "<none>" if value is None else value


def dump_ovf(path):
    with open(path, "r") as handle:
        text = handle.read()
    try:
        env = OvfEnvXml.parse_text(text)
    except NonAzureDataSource as error:
        print("error=non-azure: %s" % error)
        return 1
    except errors.ReportableErrorOvfParsingException:
        print("error=parsing")
        return 1
    except errors.ReportableErrorOvfInvalidMetadata as error:
        # `reason` carries the prefix the port renders through Display.
        print("error=invalid-metadata: %s" % error.reason)
        return 1

    custom_data = env.custom_data
    if custom_data is not None:
        custom_data = base64.b64encode(custom_data).decode("ascii")
    print("hostname=%s" % show(env.hostname))
    print("username=%s" % show(env.username))
    print("password=%s" % show(env.password))
    print("custom-data=%s" % show(custom_data))
    print(
        "disable-ssh-password-auth=%s"
        % (
            "<none>"
            if env.disable_ssh_password_auth is None
            else str(env.disable_ssh_password_auth).lower()
        )
    )
    print("preprovisioned-vm=%s" % str(env.preprovisioned_vm).lower())
    print("preprovisioned-vm-type=%s" % show(env.preprovisioned_vm_type))
    print(
        "provision-guest-proxy-agent=%s"
        % str(env.provision_guest_proxy_agent).lower()
    )
    for key in env.public_keys:
        print(
            "key fingerprint=%s path=%s value=%s"
            % (show(key["fingerprint"]), show(key["path"]), key["value"])
        )
    return 0


class _Stamp:
    """`as_encoded_report` calls `.isoformat()`; the clock is not comparable."""

    def __init__(self, text):
        self.text = text

    def isoformat(self):
        return self.text


def dump_report(args):
    """report <timestamp> <vm-id|-> <kind> [argument...]"""
    stamp = args[0]
    vm_id = None if args[1] == "-" else args[1]
    kind = args[2]
    rest = args[3:]

    def arg(index):
        return rest[index] if index < len(rest) else ""

    if kind == "success":
        # `report_success_to_host` builds this inline; the timestamp is the
        # only part that is not reproducible, so it is passed in.
        print(
            errors.encode_report(
                [
                    "result=success",
                    "agent=Cloud-Init/%s" % version.version_string(),
                    "timestamp=%s" % stamp,
                    "vm_id=%s" % vm_id,
                ]
            )
        )
        return 0

    if kind == "ovf-invalid":
        error = errors.ReportableErrorOvfInvalidMetadata(arg(0))
    elif kind == "ovf-parsing":
        error = errors.ReportableErrorOvfParsingException(
            exception=ET.ParseError(arg(0))
        )
        error.reason = "error parsing ovf-env.xml: %s" % arg(0)
    elif kind == "os-disk-pps":
        error = errors.ReportableErrorOsDiskPpsFailure()
    elif kind == "proxy-missing":
        error = errors.ReportableErrorProxyAgentNotFound()
    elif kind == "proxy-status":
        error = errors.ReportableErrorProxyAgentStatusFailure(
            subp.ProcessExecutionError(
                exit_code=int(arg(0)), stdout=arg(1), stderr=arg(2)
            )
        )
    elif kind == "vm-id":
        error = errors.ReportableErrorVmIdentification(
            exception=ValueError(arg(0)), system_uuid=arg(1)
        )
        error.supporting_data["exception"] = arg(0)
    elif kind == "imds-parsing":
        error = errors.ReportableErrorImdsMetadataParsingException(
            exception=ValueError(arg(0))
        )
        error.supporting_data["exception"] = arg(0)
    elif kind == "imds-invalid":
        error = errors.ReportableErrorImdsInvalidMetadata(
            key=arg(0), value=json.loads(arg(1))
        )
    elif kind == "imds-url":
        error = errors.ReportableErrorImdsUrlError(
            exception=UrlError(None, code=int(arg(1)) if arg(1) else None),
            duration=float(arg(3)),
        )
        error.supporting_data["exception"] = arg(2)
        error.supporting_data["url"] = arg(0)
    else:
        print("unknown report kind %s" % kind, file=sys.stderr)
        return 2

    error.timestamp = _Stamp(stamp)
    print(error.as_encoded_report(vm_id=vm_id))
    return 0


def dump_wire(args):
    def arg(index):
        return args[index] if len(args) > index else ""

    kind = arg(0)

    if kind == "minimal-ovf":
        disable = {"true": True, "false": False}.get(arg(3))
        sys.stdout.write(
            azure_helper.build_minimal_ovf(
                username=arg(1) if arg(1) != "-" else None,
                hostname=arg(2),
                disable_ssh_password_auth=disable,
            ).decode()
        )
        return 0

    if kind == "filter-pubkeys":
        with open(arg(1)) as handle:
            keys_by_fingerprint = json.load(handle)
        with open(arg(2)) as handle:
            pubkey_info = json.load(handle)
        for key in azure_helper.WALinuxAgentShim._filter_pubkeys(
            keys_by_fingerprint, pubkey_info
        ):
            print(key)
        return 0

    with open(arg(1)) as handle:
        text = handle.read()

    try:
        # need_certificate=False keeps the parse offline; the certificates URL
        # is read back off the tree instead.
        state = azure_helper.GoalState(text, None, need_certificate=False)
    except ET.ParseError as e:
        print("error=Failed to parse GoalState XML: %s" % e)
        return 1
    except azure_helper.InvalidGoalStateXMLException as e:
        print("error=%s" % e)
        return 1

    if kind == "goalstate":
        print("incarnation=%s" % state.incarnation)
        print("container-id=%s" % state.container_id)
        print("instance-id=%s" % state.instance_id)
        url = state._text_from_xpath(
            "./Container/RoleInstanceList/RoleInstance"
            "/Configuration/Certificates"
        )
        print("certificates-url=%s" % (url if url is not None else "None"))
        return 0

    if kind == "health":
        reporter = azure_helper.GoalStateHealthReporter(state, None, "endpoint")
        if arg(2) == "failure":
            document = reporter.build_report(
                incarnation=state.incarnation,
                container_id=state.container_id,
                instance_id=state.instance_id,
                status=reporter.PROVISIONING_NOT_READY_STATUS,
                substatus=reporter.PROVISIONING_FAILURE_SUBSTATUS,
                description=arg(3),
            )
        else:
            document = reporter.build_report(
                incarnation=state.incarnation,
                container_id=state.container_id,
                instance_id=state.instance_id,
                status=reporter.PROVISIONING_SUCCESS_STATUS,
            )
        sys.stdout.write(document.decode())
        return 0

    print("unknown wire mode %s" % kind, file=sys.stderr)
    return 2


class _Ds:
    """Enough of DataSourceAzure to reach the methods that only read data."""

    _ppstype_from_imds = azure_ds.DataSourceAzure._ppstype_from_imds
    _determine_pps_type = azure_ds.DataSourceAzure._determine_pps_type
    _get_public_keys_from_imds = (
        azure_ds.DataSourceAzure._get_public_keys_from_imds
    )
    _get_public_keys_from_ovf = (
        azure_ds.DataSourceAzure._get_public_keys_from_ovf
    )
    _determine_wireserver_pubkey_info = (
        azure_ds.DataSourceAzure._determine_wireserver_pubkey_info
    )
    _generate_network_config = (
        azure_ds.DataSourceAzure._generate_network_config
    )
    get_public_ssh_keys = azure_ds.DataSourceAzure.get_public_ssh_keys
    get_instance_id = azure_ds.DataSourceAzure.get_instance_id
    device_name_to_device = azure_ds.DataSourceAzure.device_name_to_device
    _get_subplatform = azure_ds.DataSourceAzure._get_subplatform
    _iid = azure_ds.DataSourceAzure._iid

    def __init__(self, **kwargs):
        self._reported_ready_marker_file = "/nonexistent"
        self.seed = None
        self._system_uuid = None
        self.metadata = {}
        self.__dict__.update(kwargs)

    def _query_vm_id(self):
        pass


class _PpsUnsupported(Exception):
    def __init__(self, pps_type):
        super().__init__("Pre-provisioning (%s) is not supported" % pps_type)


class _CrawlDs(_Ds):
    """`crawl_metadata` itself, over a fixture machine.

    Everything that would touch a VM is answered from the fixture JSON, the
    way upstream's own unit tests drive this method; what stays real is the
    sequencing, the OVF parsing and every `_*_from_imds` accessor. Calls are
    recorded so the comparison covers the order, not only the result.
    """

    crawl_metadata = azure_ds.DataSourceAzure.crawl_metadata

    def __init__(self, fixture):
        super().__init__()
        self._fixture = fixture
        self.calls = []
        self._iso_dev = None
        self._negotiated = bool(fixture.get("negotiated"))
        self._system_uuid = fixture.get("system_uuid")
        self._vm_id = None
        self.ds_cfg = {"data_dir": fixture.get("data_dir", "/var/lib/waagent")}
        self.seed_dir = "/var/lib/cloud/seed/azure"
        self._networking_up = False

    def _query_vm_id(self):
        if self._system_uuid and self._vm_id:
            return
        self.calls.append("system_uuid")
        if self._system_uuid is None:
            raise errors.ReportableErrorVmIdentification(
                exception=RuntimeError(
                    self._fixture.get(
                        "system_uuid_error", "failed to read system-uuid"
                    )
                )
            )
        self._vm_id = identity.convert_system_uuid_to_vm_id(self._system_uuid)

    def _setup_ephemeral_networking(self, *, timeout_minutes=5, **kwargs):
        self.calls.append("dhcp(%d)" % timeout_minutes)
        self._networking_up = bool(self._fixture.get("networking_up"))
        if not self._networking_up:
            raise NoDHCPLeaseError("no lease")

    def _is_ephemeral_networking_up(self):
        return self._networking_up

    def get_metadata_from_imds(self, report_failure: bool):
        self.calls.append("imds(%s)" % report_failure)
        return self._fixture.get("imds", {})

    def _check_azure_proxy_agent_status(self):
        self.calls.append("proxy_agent")

    def _report_ready(self, *, pubkey_info=None):
        self.calls.append("report_ready(%d)" % len(pubkey_info or []))
        error = self._fixture.get("report_ready_error")
        if error:
            raise RuntimeError(error)
        return list(self._fixture.get("report_ready", []))

    def _cleanup_markers(self):
        self.calls.append("cleanup_markers")

    def validate_imds_network_metadata(self, imds_md):
        return True

    # The port detects pre-provisioning and refuses it, because the netlink
    # and reprovisioning halves are not ported (deviation 111). Stopping here
    # too keeps everything before the refusal comparable.
    def _wait_for_pps_running_reuse(self):
        raise _PpsUnsupported("Running")

    def _wait_for_pps_savable_reuse(self):
        raise _PpsUnsupported("Savable")

    def _wait_for_pps_os_disk_shutdown(self):
        raise _PpsUnsupported("PreprovisionedOSDisk")

    def _wait_for_pps_unknown_reuse(self):
        raise _PpsUnsupported("Unknown")



def dump_crawl(args):
    """`azure.py crawl <fixture-json>`, paired with `dump-azure crawl`."""
    with open(args[0]) as handle:
        fixture = json.load(handle)

    sources = fixture.get("sources", {})
    unmountable = set(fixture.get("unmountable", []))
    candidates = [entry["path"] for entry in fixture.get("candidates", [])]

    datasource = _CrawlDs(fixture)
    # `_iid` reads the previous instance id from the data path, so the fixture
    # value has to arrive as a file the way it would on a real second boot.
    data = tempfile.mkdtemp()
    previous = fixture.get("previous_instance_id")
    if previous is not None:
        with open(os.path.join(data, "instance-id"), "w") as handle:
            handle.write(previous)
    datasource.paths = _Paths(data)

    def load_dir(src):
        datasource.calls.append("load(%s)" % src)
        if src in unmountable:
            raise util.MountFailedError(src)
        if src not in sources:
            raise NonAzureDataSource("no ovf-env.xml in %s" % src)
        md, ud, cfg = azure_ds.read_azure_ovf(sources[src].encode())
        # The salt is random, so neither side can compare it (deviation 81).
        cfg.get("system_info", {}).get("default_user", {}).pop(
            "hashed_passwd", None
        )
        # `load_azure_ds_dir` reads the document undecoded.
        return (md, ud, cfg, {"ovf-env.xml": sources[src].encode()})

    def mount_cb(src, func, **kwargs):
        return func(src)

    def list_candidates(seed_dir, ddir):
        datasource.calls.append("candidates")
        return iter(candidates)

    azure_ds.list_possible_azure_ds = list_candidates
    azure_ds.load_azure_ds_dir = load_dir
    azure_ds.util.mount_cb = mount_cb
    azure_ds._get_random_seed = lambda: fixture.get("random_seed")
    azure_ds.util.is_FreeBSD = lambda: False
    # The port takes the generation as an argument rather than probing, so
    # the fixture has to pin it on this side too.
    identity.is_vm_gen1 = lambda: bool(fixture.get("gen1"))

    try:
        crawled = datasource.crawl_metadata()
    except Exception as error:
        for call in datasource.calls:
            print("call %s" % call)
        # `ReportableError` never passes its reason to `Exception.__init__`,
        # so `str()` on one is empty; the reason is the part with content.
        print("error=%s" % getattr(error, "reason", error))
        return 1

    for call in datasource.calls:
        print("call %s" % call)
    print("seed=%s" % datasource.seed)
    raw = crawled["userdata_raw"]
    if not isinstance(raw, bytes):
        raw = raw.encode()
    print("userdata=%s" % base64.b64encode(raw).decode())
    print("metadata=%s" % json_dumps(crawled["metadata"]))
    print("cfg=%s" % json_dumps(crawled["cfg"]))
    print("files=%s" % json_dumps(crawled["files"]))
    return 0


def dump_ds(args):
    def arg(index):
        return args[index] if len(args) > index else ""

    def json_arg(index):
        with open(arg(index)) as handle:
            return json.load(handle)

    kind = arg(0)

    if kind == "crawl":
        with open(arg(1), "rb") as handle:
            contents = handle.read()
        try:
            md, ud, cfg = azure_ds.read_azure_ovf(contents)
        except Exception as e:
            print("error=%s" % e)
            return 1
        # The hashed password carries a random salt, so it cannot be compared
        # and the port does not produce it (deviation 81).
        cfg.get("system_info", {}).get("default_user", {}).pop(
            "hashed_passwd", None
        )
        print("metadata=%s" % json_dumps(md))
        # `read_azure_ovf` hands back bytes when CustomData was present and the
        # empty *string* when it was not.
        raw = ud if isinstance(ud, bytes) else ud.encode()
        print("userdata=%s" % base64.b64encode(raw).decode())
        print("config=%s" % json_dumps(cfg))
        return 0

    if kind == "imds":
        md = json_arg(1)
        print("username=%s" % azure_ds._username_from_imds(md))
        print("userdata=%s" % azure_ds._userdata_from_imds(md))
        print("hostname=%s" % azure_ds._hostname_from_imds(md))
        print("disable-password=%s" % azure_ds._disable_password_from_imds(md))
        print("ppstype=%s" % _Ds()._ppstype_from_imds(md))
        try:
            keys = _Ds()._get_public_keys_from_imds(md)
        except (KeyError, ValueError):
            print("keys=None")
        else:
            for key in keys:
                print("key=%s" % key)
        return 0

    if kind == "pps":
        ds = _Ds()
        if arg(3) == "true":
            ds._reported_ready_marker_file = __file__
        print("pps=%s" % ds._determine_pps_type(json_arg(1), json_arg(2)).value)
        return 0

    if kind == "sshkey":
        print("openssh=%s" % azure_ds._key_is_openssh_formatted(arg(1)))
        return 0

    if kind == "iid":
        ds = _Ds(_system_uuid=arg(1))
        previous = arg(2) if arg(2) != "-" else None
        with tempfile.TemporaryDirectory() as data:
            if previous is not None:
                with open(os.path.join(data, "instance-id"), "w") as handle:
                    handle.write(previous)
            ds.paths = _Paths(data)
            print("iid=%s" % ds._iid())
        return 0

    if kind == "subplatform":
        seed = arg(1) if arg(1) != "-" else None
        print("subplatform=%s" % _Ds(seed=seed)._get_subplatform())
        return 0

    if kind == "dscfg":
        cfg = util.mergemanydict(
            [
                util.get_cfg_by_path(json_arg(1), azure_ds.DS_CFG_PATH, {}),
                azure_ds.BUILTIN_DS_CONFIG,
            ]
        )
        print("dscfg=%s" % json_dumps(cfg))
        print(
            "ephemeral0=%s"
            % _Ds(ds_cfg=cfg).device_name_to_device("ephemeral0")
        )
        return 0

    if kind == "keys":
        ds = _Ds(metadata=json_arg(1))
        for key in ds.get_public_ssh_keys():
            print("key=%s" % key)
        for key in ds._get_public_keys_from_ovf():
            print("ovf-key=%s" % key)
        return 0

    if kind == "pubkeyinfo":
        info = _Ds()._determine_wireserver_pubkey_info(
            cfg=json_arg(1), imds_md=json_arg(2)
        )
        if info is None:
            print("pubkeys=None")
        else:
            print("pubkeys=%s" % json_dumps(info))
        return 0

    if kind == "instanceid":
        ds = _Ds(metadata=json_arg(1), _system_uuid=arg(2))
        with tempfile.TemporaryDirectory() as data:
            ds.paths = _Paths(data)
            print("iid=%s" % ds.get_instance_id())
        return 0

    if kind == "region":
        ds = _Ds(metadata=json_arg(1))
        print("region=%s" % azure_ds.DataSourceAzure.region.fget(ds))
        print(
            "zone=%s" % azure_ds.DataSourceAzure.availability_zone.fget(ds)
        )
        return 0

    if kind == "netconfig":
        _patch_interfaces(arg(3))
        # The port stops at "use the fallback", which needs
        # `net.generate_fallback_config` (deviation 83).
        azure_ds._generate_network_config_from_fallback_config = (
            lambda: "FALLBACK"
        )
        ds = _Ds(
            ds_cfg=util.mergemanydict(
                [
                    util.get_cfg_by_path(json_arg(1), azure_ds.DS_CFG_PATH, {}),
                    azure_ds.BUILTIN_DS_CONFIG,
                ]
            ),
            _metadata_imds=json_arg(2),
        )
        config = ds._generate_network_config()
        if config == "FALLBACK":
            print("config=None")
        else:
            print("config=%s" % json_dumps(config))
        return 0

    print("unknown ds mode %s" % kind, file=sys.stderr)
    return 2


class _Paths:
    def __init__(self, data):
        self._data = data

    def get_cpath(self, _name):
        return self._data


class _Ctx:
    def __init__(self, iface):
        self.iface = iface


def _patch_interfaces(path):
    """Stand in for `net.get_interfaces()` with a fixture list."""
    if not path:
        nics = []
    else:
        with open(path) as handle:
            nics = json.load(handle)
    entries = [
        ("nic%d" % idx, nic.get("mac"), nic.get("driver"), "device-id")
        for idx, nic in enumerate(nics)
    ]
    azure_ds.net.get_interfaces = lambda *a, **kw: entries


def dump_netcfg(args):
    def arg(index):
        return args[index] if len(args) > index else ""

    def json_arg(index):
        with open(arg(index)) as handle:
            return json.load(handle)

    kind = arg(0)

    if kind == "config":
        _patch_interfaces(arg(3))
        try:
            config = (
                azure_ds.generate_network_config_from_instance_network_metadata(
                    json_arg(1),
                    apply_network_config_for_secondary_ips=arg(2) == "true",
                )
            )
        except Exception as e:
            print("error=%s" % e)
            return 1
        print("config=%s" % json_dumps(config))
        return 0

    if kind == "driver":
        _patch_interfaces(arg(2))
        print(
            "driver=%s"
            % show(azure_ds.determine_device_driver_for_mac(arg(1)))
        )
        return 0

    if kind == "validate":
        _patch_interfaces(arg(2))
        ds = _Ds()
        primary = arg(3) if arg(3) != "-" else None
        ds._ephemeral_dhcp_ctx = _Ctx("eth0") if primary else None
        azure_ds.net.get_interface_mac = lambda _iface: primary
        print(
            "valid=%s"
            % azure_ds.DataSourceAzure.validate_imds_network_metadata(
                ds, json_arg(1)
            )
        )
        return 0

    if kind == "mac":
        for mac in args[1:]:
            print("mac=%s" % azure_ds.normalize_mac_address(mac))
        return 0

    print("unknown netcfg mode %s" % kind, file=sys.stderr)
    return 2


def main(argv):
    mode = argv[0] if argv else ""

    if mode == "identity":
        # `read_dmi_data` short-circuits inside a container, so the syspath
        # reader is called directly, the way tests/differential/dmi.py does.
        dmi.DMI_SYS_PATH = argv[1]
        tag = dmi._read_dmi_syspath("chassis-asset-tag")
        try:
            asset_tag = identity.ChassisAssetTag(tag).value
        except ValueError:
            asset_tag = None
        print("asset-tag=%s" % show(asset_tag))

        system_uuid = dmi._read_dmi_syspath("system-uuid")
        if system_uuid is not None:
            system_uuid = system_uuid.lower()
        print("system-uuid=%s" % show(system_uuid))

        vm_id = None
        if system_uuid is not None:
            try:
                vm_id = identity.convert_system_uuid_to_vm_id(system_uuid)
            except ValueError:
                vm_id = None
        print("vm-id=%s" % show(vm_id))
        print("gen1=%s" % str(identity.is_vm_gen1()).lower())
        return 0

    if mode == "swap":
        for value in argv[1:]:
            try:
                print("%s -> %s" % (value, identity.byte_swap_system_uuid(value)))
            except ValueError:
                print("%s -> <none>" % value)
        return 0

    if mode == "imds":
        imds.IMDS_URL = argv[1]
        try:
            metadata = imds.fetch_metadata_with_api_fallback(
                retry_deadline=None, max_connection_errors=3
            )
        except (UrlError, ValueError) as error:
            print(str(error), file=sys.stderr)
            return 1
        print(json_dumps(metadata))
        return 0

    if mode == "ovf":
        return dump_ovf(argv[1])

    if mode == "report":
        return dump_report(argv[1:])

    if mode == "wire":
        return dump_wire(argv[1:])

    if mode == "ds":
        return dump_ds(argv[1:])

    if mode == "netcfg":
        return dump_netcfg(argv[1:])

    if mode == "crawl":
        return dump_crawl(argv[1:])

    print(
        "usage: azure.py "
        "<identity|swap|imds|ovf|report|wire|ds|netcfg|crawl> ...",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
