#!/usr/bin/env python3
"""Serve the prototype and proxy /api to a running AURCache.

Same-origin, so the browser makes no cross-origin request and no CORS setup is
needed on the server. Prototype scaffolding only.
"""
import http.server, os, socketserver, urllib.request, urllib.error, sys

BACKEND = sys.argv[1] if len(sys.argv) > 1 else "http://localhost:8080"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8099

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory="dist", **kw)

    def _proxy(self, method):
        """Forward an /api call to the backend, body and all."""
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        req = urllib.request.Request(BACKEND + self.path, data=body, method=method)
        if self.headers.get("Content-Type"):
            req.add_header("Content-Type", self.headers["Content-Type"])
        try:
            with urllib.request.urlopen(req) as r:
                payload = r.read()
                self.send_response(r.status)
                self.send_header("Content-Type", r.headers.get("Content-Type", "application/json"))
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
        except urllib.error.HTTPError as e:
            payload = e.read()
            self.send_response(e.code)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except Exception as e:
            payload = str(e).encode()
            self.send_response(502)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    # Everything the UI actually sends. Without these, a write from the browser
    # gets 501 from *this* proxy while the same request against the server
    # succeeds -- which reads exactly like an application bug and is not one.
    def do_POST(self):
        self._proxy("POST")

    def do_PATCH(self):
        self._proxy("PATCH")

    def do_PUT(self):
        self._proxy("PUT")

    def do_DELETE(self):
        self._proxy("DELETE")

    def do_GET(self):
        if self.path.startswith("/api/"):
            self._proxy("GET")
            return
        # SPA fallback, matching the server's rule in `aurcache_api::spa`:
        # anything that is not an existing file is a frontend route.
        #
        # Deliberately not the usual "no file extension" heuristic. Package
        # names are part of these URLs and contain dots -- `2048.c`,
        # `python-3.11` -- so that rule would 404 real routes.
        rel = self.path.lstrip("/").split("?")[0]
        if rel and not os.path.isfile(os.path.join("dist", rel)):
            self.path = "/index.html"
        super().do_GET()

    def log_message(self, *a):
        pass

socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("", PORT), Handler) as httpd:
    print(f"prototype: http://localhost:{PORT}  (api -> {BACKEND})", flush=True)
    httpd.serve_forever()
