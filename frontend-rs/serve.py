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

    def do_GET(self):
        if self.path.startswith("/api/"):
            try:
                with urllib.request.urlopen(BACKEND + self.path) as r:
                    body = r.read()
                    self.send_response(r.status)
                    self.send_header("Content-Type", r.headers.get("Content-Type", "application/json"))
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
            except urllib.error.HTTPError as e:
                self.send_response(e.code); self.end_headers(); self.wfile.write(e.read())
            except Exception as e:
                self.send_response(502); self.end_headers(); self.wfile.write(str(e).encode())
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
