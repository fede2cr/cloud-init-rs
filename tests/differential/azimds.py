#!/usr/bin/env python3
"""Run a command against a stand-in for Azure's instance metadata service.

A request without `Metadata: true` is refused, so the differential proves both
sides send it. The fixture directory decides what each API version answers:

    extended.json   body for api-version=2021-08-01&extended=true
    extended.code   status for that URL when extended.json is absent (else 400)
    plain.json      body for api-version=2019-06-01
    retries         integer N: the first N requests answer 404

The `retries` file is what exercises the poll loop, since IMDS answers 404
until the platform has finished provisioning. It is a budget per server, which
is why the server is started per command rather than shared: each side has to
see the same sequence of failures.

Usage: azimds.py DIR command... , with `{base}` replaced by the metadata URL.
"""
import http.server
import os
import socketserver
import subprocess
import sys
import threading


class ImdsHandler(http.server.BaseHTTPRequestHandler):
    root = "."
    remaining_404s = 0

    def log_message(self, *args):
        pass

    def send_body(self, code, body):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def read(self, name):
        target = os.path.join(self.root, name)
        if not os.path.isfile(target):
            return None
        with open(target, "rb") as handle:
            return handle.read()

    def do_GET(self):
        if self.headers.get("Metadata") != "true":
            self.send_error(403, "Missing Metadata header")
            return

        if ImdsHandler.remaining_404s > 0:
            ImdsHandler.remaining_404s -= 1
            self.send_body(404, b"")
            return

        if "extended=true" in self.path:
            body = self.read("extended.json")
            if body is None:
                code = self.read("extended.code")
                self.send_body(int(code or b"400"), b"")
                return
            self.send_body(200, body)
            return

        body = self.read("plain.json")
        if body is None:
            self.send_body(404, b"")
            return
        self.send_body(200, body)


def main(argv) -> int:
    ImdsHandler.root = os.path.abspath(argv[0])
    retries = os.path.join(ImdsHandler.root, "retries")
    if os.path.isfile(retries):
        with open(retries) as handle:
            ImdsHandler.remaining_404s = int(handle.read().strip())

    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("127.0.0.1", 0), ImdsHandler) as server:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = "http://127.0.0.1:%d/metadata" % server.server_address[1]
        command = [arg.replace("{base}", base) for arg in argv[1:]]
        try:
            return subprocess.call(command, cwd="/tmp")
        finally:
            server.shutdown()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
