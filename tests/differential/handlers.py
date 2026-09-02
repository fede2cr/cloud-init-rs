#!/usr/bin/env python3
"""Dump the files cloud-init's part handlers write, as JSON.

Reads a user-data blob on stdin; argv[1] is a scratch root. The Rust side of
this comparison is `cargo run -p ci-userdata --example dump-handlers`.

Boot hooks are written but never executed: this has to be safe to run on a live
host, so `_write_part` is called directly rather than `handle_part`.
"""
import json
import os
import stat
import sys

from cloudinit import handlers, helpers, user_data
from cloudinit.handlers.boot_hook import BootHookPartHandler
from cloudinit.handlers.jinja_template import JinjaTemplatePartHandler
from cloudinit.handlers.shell_script import ShellScriptPartHandler
from cloudinit.handlers.shell_script_by_frequency import (
    ShellScriptByFreqPartHandler,
)
from cloudinit.settings import PER_ALWAYS, PER_INSTANCE, PER_ONCE

INSTANCE_ID = "i-1"


class FakeDataSource:
    def get_instance_id(self):
        return INSTANCE_ID


class WriteOnlyBootHook(BootHookPartHandler):
    """A boot hook that is written but never run.

    Upstream executes the hook the moment it is written. This has to be safe on
    a live host, and the write is the part being compared, so drop the exec.
    """

    def handle_part(self, data, ctype, filename, payload, frequency):
        if ctype in handlers.CONTENT_SIGNALS:
            return
        self._write_part(payload, filename)


def build_tree(root):
    """Lay out the directories Paths expects, plus the jinja vars file."""
    cloud_dir = os.path.join(root, "cloud")
    run_dir = os.path.join(root, "run")
    os.makedirs(os.path.join(cloud_dir, "instances", INSTANCE_ID), exist_ok=True)
    os.makedirs(run_dir, exist_ok=True)
    link = os.path.join(cloud_dir, "instance")
    if not os.path.islink(link):
        os.symlink(os.path.join("instances", INSTANCE_ID), link)
    # The datasource has to be on Paths itself: get_ipath resolves the instance
    # id through it, and BootHookPartHandler calls that in its constructor.
    paths = helpers.Paths(
        {"cloud_dir": cloud_dir, "run_dir": run_dir}, FakeDataSource()
    )
    with open(paths.get_runpath("instance_data_sensitive"), "w") as fh:
        json.dump({"v1": {"greeting": "hi"}, "ds": {"meta-data": {"a": "b"}}}, fh)
    return paths


def dispatch(part_handlers, filename, payload, ctype):
    handler = part_handlers.get(ctype)
    if handler is None:
        return
    # handlers.run_part swallows every exception and logs it, so one bad part
    # must not stop the rest.
    try:
        if handler.handler_version == 3:
            handler.handle_part(
                None, ctype, filename, payload, PER_INSTANCE, {"Content-Type": ctype}
            )
        else:
            handler.handle_part(None, ctype, filename, payload, PER_INSTANCE)
    except Exception as e:  # noqa: BLE001
        print(f"{filename}: {e}", file=sys.stderr)


def snapshot(root):
    files = []
    for dirpath, _dirnames, filenames in os.walk(root):
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, root)
            with open(full, "rb") as fh:
                content = fh.read()
            files.append(
                {
                    "path": rel,
                    "mode": oct(stat.S_IMODE(os.lstat(full).st_mode)),
                    "content": content.decode("utf-8", "replace"),
                }
            )
    return sorted(files, key=lambda f: f["path"])


def main() -> int:
    root = sys.argv[1]
    blob = sys.stdin.buffer.read()
    paths = build_tree(root)

    script = ShellScriptPartHandler(paths)
    boothook = WriteOnlyBootHook(paths, FakeDataSource())
    by_freq = [
        ShellScriptByFreqPartHandler(freq, paths)
        for freq in (PER_ALWAYS, PER_INSTANCE, PER_ONCE)
    ]
    jinja = JinjaTemplatePartHandler(
        paths, sub_handlers=[script, boothook] + by_freq
    )

    part_handlers = {}
    for handler in [script, boothook, jinja] + by_freq:
        for ctype in handler.list_types():
            part_handlers[ctype] = handler

    processed = user_data.UserDataProcessor(paths).process(blob)
    for part in processed.walk():
        if part.get_content_maintype() == "multipart":
            continue
        ctype = part.get_content_type()
        filename = part.get_filename()
        payload = handlers.util.fully_decoded_payload(part)
        dispatch(part_handlers, filename, payload, ctype)

    print(json.dumps(snapshot(root), indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
