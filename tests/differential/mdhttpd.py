#!/usr/bin/env python3
"""Serve a directory the way an OpenStack metadata service does.

Differs from `httpd.py` in one respect: a GET on a directory answers with its
child names, one per line, which is how `/openstack` advertises its versions.
Binds an ephemeral loopback port and prints it on stdout. Runs until killed.
"""
import functools
import http.server
import io
import os
import socketserver
import sys


class MetadataHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def list_directory(self, path):
        try:
            names = sorted(os.listdir(path))
        except OSError:
            self.send_error(404, "No permission to list directory")
            return None
        body = ("\n".join(names) + "\n").encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        return io.BytesIO(body)

    def send_head(self):
        # SimpleHTTPRequestHandler redirects a directory GET that lacks its
        # trailing slash; a metadata service just answers.
        path = self.translate_path(self.path)
        if os.path.isdir(path) and not self.path.endswith("/"):
            return self.list_directory(path)
        return super().send_head()


def main() -> int:
    handler = functools.partial(MetadataHandler, directory=sys.argv[1])
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("127.0.0.1", 0), handler) as server:
        print(server.server_address[1], flush=True)
        server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
