#!/usr/bin/env python3
"""Dump the parts cloud-init's UserDataProcessor produces, as JSON.

Reads a user-data blob on stdin. The optional argument is a `cloud_dir`, which
only matters for `#include-once`: that is where the URL cache lives. With
`--mime` the accumulated message is printed instead, with its random boundary
masked. The Rust side of this comparison is
`cargo run -p ci-userdata --example dump-userdata`.
"""
import json
import re
import sys
import tempfile

from cloudinit import helpers, user_data


def main() -> int:
    blob = sys.stdin.buffer.read()
    args = sys.argv[1:]
    mime = "--mime" in args
    args = [a for a in args if a != "--mime"]
    with tempfile.TemporaryDirectory() as tmp:
        cloud_dir = args[0] if args else tmp
        paths = helpers.Paths({"cloud_dir": cloud_dir, "run_dir": tmp})
        processed = user_data.UserDataProcessor(paths).process(blob)

    if mime:
        text = re.sub(r"===============\d+==", "BOUND", str(processed))
        sys.stdout.write(text)
        return 0

    parts = []
    for part in processed.walk():
        if part.get_content_maintype() == "multipart":
            continue
        payload = part.get_payload(decode=True)
        if payload is None:
            payload = b""
        index = part.get("Launch-Index")
        parts.append(
            {
                "content_type": part.get_content_type(),
                "filename": part.get_filename(),
                "launch_index": int(index) if index is not None else None,
                "payload": payload.decode("utf-8", "replace"),
            }
        )
    # `dump-userdata` prints with `serde_json::to_string_pretty`, which is not
    # the CPython-compatible writer the other dumpers use and emits UTF-8
    # verbatim.
    print(json.dumps(parts, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
