#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Minimal echo service for evorule-server's `service_registry.json` demo (echo_svc).

This is the missing piece that makes `call_external` (role 1/3) work end-to-end:
- Listens on 127.0.0.1:9100
- Accepts POST /api/echo
- Echoes the JSON body back under an "echo" key

The response shape matches the 2026-08-19 WAL record produced by
`rules/10_role13_demo.json` so the demo's `demo_result` lands as expected:

    {"echo": <received body>, "method": "POST", "path": "/api/echo",
     "source": "echo_server", "ts": <unix seconds>}

Run:
    python3 echo_server.py
or
    C:/Users/A/.workbuddy/binaries/python/versions/3.13.12/python.exe echo_server.py
"""
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOST, PORT = "127.0.0.1", 9100


class EchoHandler(BaseHTTPRequestHandler):
    def _send(self, code: int, obj) -> None:
        body = json.dumps(obj).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if self.path.rstrip("/") not in ("/api/echo", "/echo"):
            self._send(404, {"error": "not found", "path": self.path})
            return
        try:
            length = int(self.headers.get("Content-Length", "0") or "0")
        except ValueError:
            length = 0
        raw = self.rfile.read(length) if length > 0 else b"{}"
        try:
            payload = json.loads(raw or b"{}")
        except Exception:
            payload = {"raw": raw.decode("utf-8", "replace")}
        self._send(200, {
            "echo": payload,
            "method": "POST",
            "path": "/api/echo",
            "source": "echo_server",
            "ts": int(time.time()),
        })

    def log_message(self, fmt, *args):  # silence default stderr logging
        pass


if __name__ == "__main__":
    srv = ThreadingHTTPServer((HOST, PORT), EchoHandler)
    print(f"[START] echo_server listening on {HOST}:{PORT}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("[STOP] echo_server shutting down", flush=True)
        srv.shutdown()
