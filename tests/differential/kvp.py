"""Dump Hyper-V KVP pool records the way cloud-init builds them.

Reads one JSON request on stdin and prints, per record, the masked key on one
line and the hex of its 2048 value bytes on the next. See dump-kvp.rs for the
request shapes.
"""

import binascii
import json
import os
import sys
import tempfile

from cloudinit.reporting.handlers import HyperVKvpReportingHandler

# Fixed so the two implementations agree; upstream derives the incarnation from
# the boot time, which moves between two runs of the same case.
INCARNATION = 1700000000
VM_ID = "11111111-2222-3333-4444-555555555555"


class Event:
    """Only what `_encode_event` reads. A start event has no `result` and no
    `duration`, and upstream finds that out with `hasattr`."""

    def __init__(self, request):
        self.name = request.get("name", "")
        self.event_type = request.get("type", "")
        self.description = request.get("description", "")
        self.timestamp = request.get("timestamp", 0.0)
        if "result" in request:
            self.result = request["result"]
        if "duration" in request:
            self.duration = request["duration"]


def is_uuid(text):
    if len(text) != 36:
        return False
    for index, char in enumerate(text):
        if index in (8, 13, 18, 23):
            if char != "-":
                return False
        elif char not in "0123456789abcdefABCDEF":
            return False
    return True


def mask(key):
    """The last uuid in an event key is `uuid.uuid4()`, different on every call.
    It sits at the end, optionally followed by `|<slice index>`; the vm id is
    the same shape but never last, so anchoring keeps it visible."""
    head, tail = key, ""
    prefix, sep, last = key.rpartition("|")
    if sep and last.isdigit():
        head, tail = prefix, "|" + last
    prefix, sep, last = head.rpartition("|")
    if sep and is_uuid(last):
        return prefix + "|<uuid>" + tail
    return key


def main():
    request = json.load(sys.stdin)
    pool = os.path.join(tempfile.gettempdir(), "ci-kvp-py.%d" % os.getpid())
    handler = HyperVKvpReportingHandler(kvp_file_path=pool)
    handler.incarnation_no = INCARNATION
    handler.event_key_prefix = "{0}|{1}".format(
        handler.EVENT_PREFIX, INCARNATION
    )
    handler._vm_id = VM_ID

    op = request.get("op")
    key = request.get("key", "")
    value = request.get("value", "")
    try:
        if op == "item":
            records = [handler._encode_kvp_item(key, value)]
        elif op == "write_key":
            handler.write_key(key, value)
            with open(pool, "rb") as handle:
                blob = handle.read()
            size = handler.HV_KVP_RECORD_SIZE
            records = [blob[i : i + size] for i in range(0, len(blob), size)]
        elif op == "event":
            records = handler._encode_event(Event(request))
        else:
            sys.stderr.write("unknown op: %r\n" % (op,))
            return 1
    finally:
        try:
            os.unlink(pool)
        except OSError:
            pass

    out = []
    for record in records:
        split = handler.HV_KVP_EXCHANGE_MAX_KEY_SIZE
        name = record[:split].rstrip(b"\x00").decode("utf-8", "replace")
        out.append(mask(name))
        out.append(binascii.hexlify(record[split:]).decode("ascii"))
    sys.stdout.write("".join(line + "\n" for line in out))
    return 0


sys.exit(main())
