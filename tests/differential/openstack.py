"""Upstream side of the OpenStack metadata-service differential."""

import base64
import sys

from cloudinit.atomic_helper import json_dumps
from cloudinit.sources import BrokenMetadata
from cloudinit.sources import DataSourceOpenStack as ds
from cloudinit.sources.helpers.openstack import NonReadable


def as_b64(value):
    if value is None:
        return "<none>"
    if isinstance(value, str):
        value = value.encode("utf-8")
    return base64.b64encode(value).decode("ascii")


def as_json(value):
    return "<none>" if value is None else json_dumps(value)


def main(argv):
    try:
        results = ds.read_metadata_service(argv[0], timeout=2, retries=0)
    except NonReadable as e:
        print("NonReadable=%s" % e)
        return
    except BrokenMetadata as e:
        print("BrokenMetadata=%s" % e)
        return

    metadata = dict(results.get("metadata", {}))
    # read_v2 decodes random_seed to bytes in place; the port keeps a string.
    seed = metadata.get("random_seed")
    if isinstance(seed, bytes):
        metadata["random_seed"] = seed.decode("utf-8", "replace")

    userdata = results.get("userdata")
    # read_v2 seeds userdata with "" whether or not user_data was served.
    if userdata == "" or userdata == b"":
        userdata = None

    print("version=%s" % results["version"])
    print("metadata=%s" % json_dumps(metadata))
    print("userdata=%s" % as_b64(userdata))
    print("dsmode=%s" % results.get("dsmode", "<none>"))
    print("vendordata=%s" % as_json(results.get("vendordata")))
    print("vendordata2=%s" % as_json(results.get("vendordata2")))
    print("networkdata=%s" % as_json(results.get("networkdata")))
    # _read_ec2_metadata crawls the EC2 tree, which the port does not have.
    print("ec2-metadata=<none>")
    print("network_config=%s" % as_b64(results.get("network_config")))
    for path in sorted(results.get("files", {})):
        print("file %s=%s" % (path, as_b64(results["files"][path])))


if __name__ == "__main__":
    main(sys.argv[1:])
