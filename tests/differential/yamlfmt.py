"""Dump JSON from stdin the way cloud-init does, for differential testing."""

import json
import sys

from cloudinit import safeyaml

sys.stdout.write(safeyaml.dumps(json.load(sys.stdin)))
