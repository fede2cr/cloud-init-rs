#!/usr/bin/env python3
"""Serve a directory over HTTPS for the `#include` differential cases.

    httpsd.py <root> <cert.pem> <key.pem> [ca.pem]

Binds an ephemeral port on the loopback interface and prints it on stdout, so
the harness does not have to guess a free port. With a CA argument the server
asks for a client certificate but does not require one, and `/whoami` answers
with the common name it saw, so the mutual-TLS case shows up in the fetched
user-data rather than only in a log line. Runs until killed.
"""
import functools
import http.server
import socketserver
import ssl
import sys


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path != "/whoami":
            return super().do_GET()
        cert = self.connection.getpeercert() or {}
        name = "none"
        for rdn in cert.get("subject", ()):
            for key, value in rdn:
                if key == "commonName":
                    name = value
        body = ("#cloud-config\nruncmd: [%s]\n" % name).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/cloud-config")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main() -> int:
    root, certfile, keyfile = sys.argv[1:4]
    ca = sys.argv[4] if len(sys.argv) > 4 else None

    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certfile, keyfile)
    if ca:
        context.verify_mode = ssl.CERT_OPTIONAL
        context.load_verify_locations(ca)

    handler = functools.partial(Handler, directory=root)
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("127.0.0.1", 0), handler) as server:
        server.socket = context.wrap_socket(server.socket, server_side=True)
        print(server.server_address[1], flush=True)
        # A rejected handshake must not take the server down with it.
        server.handle_error = lambda *args: None
        server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
