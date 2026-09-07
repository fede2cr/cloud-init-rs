#!/usr/bin/env python3
"""Apply a #cloud-config-jsonp patch with the jsonpatch library, as JSON.

Reads {"doc": <document>, "patch": <patch as a string>} on stdin. The Rust side
of this comparison is `cargo run -p ci-core --example dump-jsonpatch`.

Not named jsonpatch.py: a script's own directory leads sys.path, so that name
would shadow the library this is supposed to be testing against.

Failures are reported by class, not by message: cloud-init only logs the
message, and the one distinction that reaches disk is whether the exception was
a ValueError -- CloudConfigPartHandler records those parts and drops the rest.
"""
import json
import sys

import jsonpatch


def main() -> int:
    case = json.load(sys.stdin)
    try:
        patch = jsonpatch.JsonPatch.from_string(case["patch"])
        outcome = {"ok": patch.apply(case["doc"], in_place=False)}
    except Exception as e:  # noqa: BLE001
        outcome = {"error": "value" if isinstance(e, ValueError) else "failed"}
    print(json.dumps(outcome, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
