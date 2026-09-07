"""Dump ET.tostring round trips and the Azure password redaction.

Usage: `ettree.py <file>`

The file holds one base64 blob per line, so a case can carry newlines, quotes
and non-ASCII without the shell in between having an opinion.

Not named after the package it imports: this directory is `sys.path[0]`, and
an `xml.py` here would shadow the stdlib `xml` package.
"""

import base64
import json
import sys
import xml.etree.ElementTree as ET

REDACTION = "REDACTED"


def dump(value):
    return json.dumps(value, indent=1, sort_keys=True, separators=(",", ": "))


def redact(root):
    for elem in root.iter():
        if "UserPassword" in elem.tag and elem.text != REDACTION:
            elem.text = REDACTION
    return root


def main(argv):
    with open(argv[1]) as fp:
        lines = [line.strip() for line in fp if line.strip()]

    out = []
    for line in lines:
        text = base64.b64decode(line).decode("utf-8", "replace")
        # The base64, not the text: a case can carry non-ASCII, and the two
        # JSON encoders do not agree about how to spell it (`ensure_ascii`).
        # What is under test is the XML, so the label stays ASCII.
        case = {"case": line}
        try:
            root = ET.fromstring(text)  # nosec B314
        except Exception:
            case["tostring"] = None
            case["redacted"] = None
        else:
            case["tostring"] = ET.tostring(root).decode("ascii")
            case["redacted"] = ET.tostring(redact(root)).decode("ascii")
        out.append(case)

    print(dump(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
