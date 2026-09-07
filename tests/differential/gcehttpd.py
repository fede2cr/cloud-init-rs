#!/usr/bin/env python3
"""Serve a directory the way the GCE metadata service does.

Two differences from `httpd.py`. A request without `Metadata-Flavor: Google`
is refused, so the differential proves both sides send it. And a path ending
in `/` (which is how the recursive attribute queries are spelled) is answered
from a file named `index` in that directory, since the query string carries
the `recursive=True` rather than the path.

Binds an ephemeral loopback port and prints it on stdout. Runs until killed.
"""
import http.server
import os
import socketserver
import sys


class GceHandler(http.server.BaseHTTPRequestHandler):
    root = "."

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.headers.get("Metadata-Flavor") != "Google":
            self.send_error(403, "Missing Metadata-Flavor header")
            return
        path = self.path.split("?", 1)[0]
        if path.endswith("/"):
            path += "index"
        target = os.path.normpath(os.path.join(self.root, path.lstrip("/")))
        if not target.startswith(self.root) or not os.path.isfile(target):
            self.send_error(404, "Not Found")
            return
        with open(target, "rb") as handle:
            body = handle.read()
        self.send_response(200)
        self.send_header("Content-Type", "application/text")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main() -> int:
    GceHandler.root = os.path.abspath(sys.argv[1])
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("127.0.0.1", 0), GceHandler) as server:
        print(server.server_address[1], flush=True)
        server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
