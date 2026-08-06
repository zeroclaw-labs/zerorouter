#!/usr/bin/env python3
"""A deliberately misbehaving upstream, for failure injection against a
REAL router process.

The in-process fakes (`router/src/testing.rs`) script provider OUTCOMES;
they cannot produce transport pathologies, because they never touch a
socket. This does: point a provider at it with the test seam

    ZEROROUTER_PROVIDER_BASE_URL_OPENAI=http://127.0.0.1:9500

and run the router normally. It speaks the OpenAI *Responses* dialect,
which is what ZeroRouter's owned wire sends.

    MODE=ok              healthy control (both streaming and not)
    MODE=drop-midstream  emits two deltas, then goes silent WITHOUT
                         closing — the half-open socket that used to hold
                         a customer stream for the full request budget
                         (fixed by the wire's idle ceiling)
    MODE=storm-429       every request rate-limited, with Retry-After
    MODE=malformed       200 OK carrying neither JSON nor SSE
    MODE=slow            healthy, but delays SLOW_SECONDS (default 6)
                         before answering — the window a database-outage
                         injection needs to land mid-request

Usage:
    MODE=drop-midstream PORT=9500 python3 scripts/chaos-upstream.py

What to assert after each mode: the client gets a clean typed error (not
a hang), `usage_reservations` holds nothing for the test user, and the
balance moved by exactly the sum of that user's settled `usage_events`.
"""
import json, os, socketserver, time
from http.server import BaseHTTPRequestHandler

MODE = os.environ.get("MODE", "ok")
PORT = int(os.environ.get("PORT", "9500"))

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        stream = bool(body.get("stream"))
        if MODE == "storm-429":
            payload = json.dumps({"error": {"message": "Rate limit reached", "type": "rate_limit_error", "code": "rate_limit_exceeded"}}).encode()
            self.send_response(429)
            self.send_header("retry-after", "1")
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if MODE == "slow":
            time.sleep(float(os.environ.get("SLOW_SECONDS", "6")))
        if MODE == "malformed":
            garbage = b"<<<this is not json or sse>>>"
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(garbage)))
            self.end_headers()
            self.wfile.write(garbage)
            return
        # ZeroRouter's owned OpenAI wire speaks the RESPONSES dialect: the
        # override URL is the full endpoint, requests carry `input`, and the
        # terminal stream event is response.completed with usage attached.
        if stream:
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("transfer-encoding", "chunked")
            self.end_headers()
            def chunk(data):
                encoded = ("data: " + json.dumps(data) + "\n\n").encode()
                self.wfile.write(hex(len(encoded))[2:].encode() + b"\r\n" + encoded + b"\r\n")
                self.wfile.flush()
            chunk({"type": "response.output_text.delta", "delta": "part "})
            chunk({"type": "response.output_text.delta", "delta": "one"})
            if MODE == "drop-midstream":
                # Vanish without response.completed or a clean chunked EOF.
                self.wfile.flush()
                self.connection.close()
                return
            chunk({"type": "response.completed", "response": {"status": "completed",
                   "usage": {"input_tokens": 20, "output_tokens": 5,
                              "input_tokens_details": {"cached_tokens": 0}}}})
            self.wfile.write(b"0\r\n\r\n")
            return
        payload = json.dumps({
            "status": "completed",
            "output": [{"type": "message", "role": "assistant",
                         "content": [{"type": "output_text", "text": "control answer"}]}],
            "usage": {"input_tokens": 20, "output_tokens": 5,
                       "input_tokens_details": {"cached_tokens": 0}},
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True

Server(("127.0.0.1", PORT), Handler).serve_forever()
