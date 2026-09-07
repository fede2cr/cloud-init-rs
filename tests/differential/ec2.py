"""Upstream side of the EC2 metadata crawl differential."""

import base64
import sys

from cloudinit import util
from cloudinit.atomic_helper import json_dumps
from cloudinit.sources.helpers import ec2


def show(blob):
    return "<none>" if not blob else base64.b64encode(blob).decode("ascii")


def main(argv):
    address = argv[0]
    kwargs = {"metadata_address": address, "timeout": 2, "retries": 0}

    print("metadata=%s" % json_dumps(ec2.get_instance_metadata(**kwargs)))
    print("identity=%s" % json_dumps(ec2.get_instance_identity(**kwargs)))
    userdata = ec2.get_instance_userdata(**kwargs)
    print("userdata=%s" % show(userdata))
    # What DataSourceEc2.crawl_metadata stores.
    print("decoded=%s" % show(util.maybe_b64decode(userdata)))


if __name__ == "__main__":
    main(sys.argv[1:])
