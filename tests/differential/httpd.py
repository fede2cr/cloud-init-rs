#!/usr/bin/env python3
"""Serve a directory over HTTP for the `#include` differential cases.

Binds an ephemeral port on the loopback interface and prints it on stdout, so
the harness does not have to guess a free port. Runs until killed.
"""
import functools
import http.server
import socketserver
import sys


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def main() -> int:
    handler = functools.partial(Quiet, directory=sys.argv[1])
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("127.0.0.1", 0), handler) as server:
        print(server.server_address[1], flush=True)
        server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
