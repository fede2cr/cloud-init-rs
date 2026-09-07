"""Upstream side of the GCE metadata differential."""

import base64
import collections
import sys

from cloudinit.atomic_helper import json_dumps
from cloudinit.sources import DataSourceGCE as gce

# `read_md` wants a `get_url_params` result; the module's own `__main__` omits
# it and crashes (bug B42), so the differential builds one.
UrlParams = collections.namedtuple(
    "UrlParams", "num_retries sec_between_retries"
)


def main(argv):
    ret = gce.read_md(
        address=argv[0], url_params=UrlParams(0, 0), platform_check=False
    )

    if not ret["success"]:
        print("failed=%s" % ret["reason"])
        return

    print("metadata=%s" % json_dumps(ret["meta-data"]))
    userdata = ret["user-data"]
    print(
        "userdata=%s"
        % (
            "<none>"
            if userdata is None
            else base64.b64encode(userdata).decode("ascii")
        )
    )


if __name__ == "__main__":
    main(sys.argv[1:])
