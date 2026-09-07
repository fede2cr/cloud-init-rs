"""Upstream side of the LXD socket API differential."""

import sys

from cloudinit.atomic_helper import json_dumps
from cloudinit.sources import DataSourceLXD as lxd


def main(argv):
    # The socket path is a module constant; the adapter reads it per request.
    lxd.LXD_SOCKET_PATH = argv[0]
    print(json_dumps(lxd.read_metadata(metadata_keys=lxd.MetaDataKeys.ALL)))


if __name__ == "__main__":
    main(sys.argv[1:])
