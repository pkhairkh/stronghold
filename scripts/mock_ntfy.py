#!/usr/bin/env python3
"""Mock ntfy.sh server: accepts any POST and returns 200 OK.

Listens on 0.0.0.0:8090. Used to satisfy the gateway's push_approval_request
so /agent/order can proceed past the ntfy step.
"""
import http.server
import socketserver
import sys
import threading

class NtfyMock(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get('Content-Length', 0) or 0)
        body = self.rfile.read(length) if length else b''
        # Echo a JSON message so callers can see we accepted
        sys.stderr.write(f"[ntfy-mock] POST {self.path} ({len(body)} bytes)\n")
        sys.stderr.flush()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(b'{"id":"mock","time":1,"event":"message","topic":"mock"}')

    def do_GET(self):
        # SSE subscription — return an empty event stream so the phone-pending
        # SSE endpoint doesn't fail. We just send a heartbeat every 30s, but
        # for tests an immediate empty 200 is fine.
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        try:
            self.wfile.write(b': heartbeat\n\n')
            self.wfile.flush()
        except Exception:
            pass

    def log_message(self, fmt, *args):
        pass  # silence access log

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8090
    server = socketserver.ThreadingTCPServer(("0.0.0.0", port), NtfyMock)
    server.daemon_threads = True
    print(f"[ntfy-mock] listening on 0.0.0.0:{port}", flush=True)
    server.serve_forever()
