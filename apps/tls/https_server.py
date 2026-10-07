#!/usr/bin/env python3
"""A loopback HTTPS server, and a plain HTTP one beside it, for `test_https.sh`.

    python3 apps/tls/https_server.py <leaf.pem> <key.pem> [rest.pem]

Prints `ready <tls port> <plain port>` once both listen, then serves until
killed. The TLS server speaks TLS 1.3 only. Routes, on both:

    /hello          200, `hello over tls` (`hello over plain` on the plain port)
    /empty          204
    /chunked        200, the body `0123456789` in three chunks
    /big            200, 300000 octets of `x`
    /echo           200, the request's Host field, then its method
    /same           302 to /hello on the same origin
    /to-http        302 to the plain port's /hello (on the TLS port: a downgrade)
    /upgrade        301 to https://localhost:<tls port>/hello (plain port only)
    /elsewhere      302 to https://127.0.0.2:<tls port>/hello (another host)
    /loop           302 to /loop
"""
import http.server
import socket
import socketserver
import ssl
import sys
import threading

TLS_PORT = 0
PLAIN_PORT = 0


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, code, body=b"", extra=()):
        self.send_response(code)
        for name, value in extra:
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def do_GET(self):
        secure = isinstance(self.connection, ssl.SSLSocket)
        route = self.path
        if route == "/hello":
            self.reply(200, b"hello over " + (b"tls" if secure else b"plain") + b"\n")
        elif route == "/empty":
            self.reply(204)
        elif route == "/chunked":
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "close")
            self.end_headers()
            for piece in (b"0123", b"4567", b"89"):
                self.wfile.write(b"%x\r\n%s\r\n" % (len(piece), piece))
            self.wfile.write(b"0\r\n\r\n")
        elif route == "/big":
            self.reply(200, b"x" * 300000)
        elif route == "/echo":
            self.reply(200, (self.headers.get("Host", "") + " " + self.command + "\n").encode())
        elif route == "/same":
            self.reply(302, extra=[("Location", "/hello")])
        elif route == "/to-http":
            self.reply(302, extra=[("Location", "http://localhost:%d/hello" % PLAIN_PORT)])
        elif route == "/upgrade":
            self.reply(301, extra=[("Location", "https://localhost:%d/hello" % TLS_PORT)])
        elif route == "/elsewhere":
            self.reply(302, extra=[("Location", "https://127.0.0.2:%d/hello" % TLS_PORT)])
        elif route == "/loop":
            self.reply(302, extra=[("Location", "/loop")])
        else:
            self.reply(404, b"no such route\n")

    do_HEAD = do_GET


# Where `localhost` goes on this machine: the client resolves the name and
# connects to the first address, which is `::1` on many systems.
LOCAL = socket.getaddrinfo("localhost", None, type=socket.SOCK_STREAM)[0]


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    address_family = LOCAL[0]
    daemon_threads = True
    allow_reuse_address = True


class TlsServer(Server):
    def get_request(self):
        raw, address = super().get_request()
        try:
            return self.context.wrap_socket(raw, server_side=True), address
        except (ssl.SSLError, OSError):
            raw.close()
            raise

    def handle_error(self, request, client_address):
        pass


def main():
    global TLS_PORT, PLAIN_PORT
    leaf, key = sys.argv[1], sys.argv[2]
    rest = sys.argv[3] if len(sys.argv) > 3 else None
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    chain = leaf
    if rest:
        chain = leaf + ".chain"
        with open(chain, "w") as out:
            out.write(open(leaf).read() + open(rest).read())
    context.load_cert_chain(chain, key)
    tls = TlsServer((LOCAL[4][0], 0), Handler)
    tls.context = context
    plain = Server((LOCAL[4][0], 0), Handler)
    TLS_PORT = tls.server_address[1]
    PLAIN_PORT = plain.server_address[1]
    threading.Thread(target=plain.serve_forever, daemon=True).start()
    print("ready %d %d" % (TLS_PORT, PLAIN_PORT), flush=True)
    tls.serve_forever()


main()
