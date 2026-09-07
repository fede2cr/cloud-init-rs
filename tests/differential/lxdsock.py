"""A stand-in for LXD's /dev/lxd/sock, serving a fixture tree.

Fixture layout, one directory per case:
  meta-data     served at /1.0/meta-data
  devices       served at /1.0/devices
  config/<key>  served at /1.0/config/<key>
  config.json   optional raw body for /1.0/config, else generated from config/
  flaky         optional, one route per line, answered 500 on first request
"""

import http.server
import os
import socketserver
import sys

API = "/1.0"


def build_handler(root, flaky):
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, *args):
            pass

        def send_body(self, code, body):
            self.send_response(code)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def resolve(self):
            path = self.path.split("?")[0]
            if path == API + "/meta-data":
                return os.path.join(root, "meta-data")
            if path == API + "/devices":
                return os.path.join(root, "devices")
            if path.startswith(API + "/config/"):
                key = path[len(API + "/config/") :]
                if "/" in key or key in ("", ".", ".."):
                    return None
                return os.path.join(root, "config", key)
            return None

        def config_listing(self):
            override = os.path.join(root, "config.json")
            if os.path.isfile(override):
                with open(override, "rb") as handle:
                    return handle.read()
            config_dir = os.path.join(root, "config")
            keys = sorted(os.listdir(config_dir)) if os.path.isdir(config_dir) else []
            routes = ['"%s/config/%s"' % (API, key) for key in keys]
            return ("[" + ",".join(routes) + "]").encode()

        def do_GET(self):
            path = self.path.split("?")[0]
            if path in flaky:
                flaky.discard(path)
                self.send_body(500, b"try again")
                return
            if path == API + "/config":
                self.send_body(200, self.config_listing())
                return
            target = self.resolve()
            if target is None or not os.path.isfile(target):
                self.send_body(404, b"not found")
                return
            with open(target, "rb") as handle:
                self.send_body(200, handle.read())

    return Handler


class Server(socketserver.ThreadingUnixStreamServer):
    allow_reuse_address = True

    # BaseHTTPRequestHandler wants a peer address to log; AF_UNIX has none.
    def get_request(self):
        request, _ = super().get_request()
        return request, ("localhost", 0)


def main(argv):
    root, socket_path = argv[0], argv[1]
    flaky_file = os.path.join(root, "flaky")
    flaky = set()
    if os.path.isfile(flaky_file):
        with open(flaky_file) as handle:
            flaky = {line.strip() for line in handle if line.strip()}

    if os.path.exists(socket_path):
        os.unlink(socket_path)
    server = Server(socket_path, build_handler(root, flaky))
    print("ready", flush=True)
    try:
        server.serve_forever()
    finally:
        server.server_close()
        if os.path.exists(socket_path):
            os.unlink(socket_path)


if __name__ == "__main__":
    main(sys.argv[1:])
