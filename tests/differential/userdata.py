#!/usr/bin/env python3
"""Dump the parts cloud-init's UserDataProcessor produces, as JSON.

Reads a user-data blob on stdin. The Rust side of this comparison is
`cargo run -p ci-userdata --example dump-userdata`.
"""
import json
import sys
import tempfile

from cloudinit import helpers, user_data


def main() -> int:
    blob = sys.stdin.buffer.read()
    with tempfile.TemporaryDirectory() as tmp:
        paths = helpers.Paths({"cloud_dir": tmp, "run_dir": tmp})
        processed = user_data.UserDataProcessor(paths).process(blob)

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
    print(json.dumps(parts, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
