#!/usr/bin/env python3
"""Serve a directory tree the way the EC2 instance metadata service does.

A directory answers with one child name per line, with a trailing `/` on the
names that are themselves directories, and no redirect when the request path
lacks a trailing slash. A file answers with its bytes and no trailing newline.

Binds an ephemeral loopback port and prints it on stdout. Runs until killed.
"""
import http.server
import os
import socketserver
import sys


class Ec2Handler(http.server.BaseHTTPRequestHandler):
    root = "."

    def log_message(self, *args):
        pass

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        target = os.path.normpath(os.path.join(self.root, path.lstrip("/")))
        if not target.startswith(self.root):
            self.send_error(404, "Not Found")
            return
        if os.path.isdir(target):
            # `public-keys/` lists `0=name`, which no filename can express.
            override = os.path.join(target, ".listing")
            if os.path.isfile(override):
                with open(override, "rb") as handle:
                    body = handle.read()
            else:
                names = []
                for name in sorted(os.listdir(target)):
                    child = os.path.join(target, name)
                    names.append(name + ("/" if os.path.isdir(child) else ""))
                body = "\n".join(names).encode()
        elif os.path.isfile(target):
            with open(target, "rb") as handle:
                body = handle.read()
        else:
            self.send_error(404, "Not Found")
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main() -> int:
    Ec2Handler.root = os.path.abspath(sys.argv[1])
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("127.0.0.1", 0), Ec2Handler) as server:
        print(server.server_address[1], flush=True)
        server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
