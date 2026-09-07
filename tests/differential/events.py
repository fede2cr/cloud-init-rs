#!/usr/bin/env python3
"""Dump what `cloudinit.event` and `stages.update_event_enabled` decide.

Paired with the `dump-events` example in ci-datasource.

`convert` prints the `{scope: [event...]}` mapping an `updates:` block turns
into; `enabled` prints the yes/no a datasource class gives for one event. Both
print every WARNING they logged, because those are what reach
`status.json`'s `recoverable_errors`.

Usage: events.py convert <cfg.json>
       events.py enabled <DataSourceClass> <cfg.json> <event> <cloud-dir>
"""
import json
import logging
import sys

sys.path.insert(0, "/usr/lib/python3/dist-packages")

from cloudinit import helpers, stages  # noqa: E402
from cloudinit.event import EventScope, EventType, userdata_to_events  # noqa: E402
from cloudinit.sources import (  # noqa: E402
    DataSourceConfigDrive,
    DataSourceEc2,
    DataSourceGCE,
    DataSourceLXD,
    DataSourceNoCloud,
    DataSourceNone,
    DataSourceOpenStack,
)

CLASSES = {
    cls.__name__: cls
    for cls in (
        DataSourceNoCloud.DataSourceNoCloud,
        DataSourceNoCloud.DataSourceNoCloudNet,
        DataSourceNone.DataSourceNone,
        DataSourceConfigDrive.DataSourceConfigDrive,
        DataSourceLXD.DataSourceLXD,
        DataSourceOpenStack.DataSourceOpenStack,
        DataSourceGCE.DataSourceGCE,
        DataSourceEc2.DataSourceEc2,
    )
}


class Collector(logging.Handler):
    def __init__(self):
        super().__init__(level=logging.WARNING)
        self.messages = []

    def emit(self, record):
        self.messages.append(record.getMessage())


def collect():
    handler = Collector()
    root = logging.getLogger()
    root.setLevel(logging.DEBUG)
    root.addHandler(handler)
    return handler


def show_events(mapping):
    return json.dumps(
        {
            scope.value: sorted(event.value for event in types)
            for scope, types in sorted(
                mapping.items(), key=lambda item: item[0].value
            )
        },
        sort_keys=True,
    )


def main(argv):
    mode = argv[0] if argv else ""
    handler = collect()

    if mode == "convert":
        with open(argv[1]) as stream:
            cfg = json.load(stream)
        print("events=%s" % show_events(userdata_to_events(cfg.get("updates", {}))))
    elif mode == "enabled":
        cls = CLASSES[argv[1]]
        with open(argv[2]) as stream:
            cfg = json.load(stream)
        # `object.__new__` skips constructors that want a distro; every
        # attribute read below is a class attribute anyway.
        datasource = object.__new__(cls)
        datasource.paths = helpers.Paths({"cloud_dir": argv[4]})
        print(
            "enabled=%s"
            % stages.update_event_enabled(
                datasource=datasource,
                cfg=cfg,
                event_source_type=EventType(argv[3]),
                scope=EventScope.NETWORK,
            )
        )
    else:
        print("usage: events.py <convert|enabled> ...", file=sys.stderr)
        return 2

    for message in handler.messages:
        print("warning=%s" % message)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
