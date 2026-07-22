#!/usr/bin/env python3
"""Deterministic no-spend Council HTTP stub for Gateway smoke harnesses."""

import json
import os
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


PROFILE = os.environ.get("COUNCIL_STUB_PROFILE", "phase3")


def extract(pattern, text, default):
    match = re.search(pattern, text)
    return match.group(1).strip() if match else default


class Handler(BaseHTTPRequestHandler):
    server_version = "irin-council-stub/1"

    def log_message(self, _format, *_args):
        return

    def send_json(self, status, payload, extra_headers=None):
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        for key, value in (extra_headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/") == "/api/health":
            self.send_json(200, {"status": "ok", "service": "irin-council-stub"})
            return
        self.send_json(404, {"error": "not_found"})

    def do_POST(self):
        if self.path.rstrip("/") != "/api/deliberate":
            self.send_json(404, {"error": "not_found"})
            return
        if not self.headers.get("X-Gateway-Auth"):
            self.send_json(401, {"error": "missing X-Gateway-Auth"})
            return

        length = int(self.headers.get("content-length", "0") or "0")
        try:
            payload = json.loads(self.rfile.read(length).decode("utf-8"))
        except Exception as exc:
            self.send_json(400, {"error": f"bad_json: {exc}"})
            return

        messages = payload.get("messages") or []
        prompt = "\n".join(
            str(message.get("content", ""))
            for message in messages
            if isinstance(message, dict)
        )
        default_tenant = "irin-demo" if PROFILE == "demo" else "phase3-smoke"
        tenant = extract(
            r'Escalation tenant:\s*"?([A-Za-z0-9_.:-]+)"?', prompt, default_tenant
        )
        escalation_id = extract(
            r'Escalation id:\s*"?([A-Za-z0-9_.:-]+)"?',
            prompt,
            "phase3-startup-probe-v1-00000000000000000000000000000000",
        )

        if "startup-probe" in prompt:
            in_response_to = extract(
                r'"in_response_to":\s*"([^"]+)"', prompt, escalation_id
            )
            proposal = {
                "schema": "irin.directive.proposal.v1",
                "in_response_to": in_response_to,
                "authority": "recommend",
                "verdict": "Dismiss",
                "rationale": "Deterministic startup-probe response from the no-spend stub.",
            }
        else:
            subject = "irin-demo" if PROFILE == "demo" else "phase3-smoke"
            proposal = {
                "schema": "irin.directive.proposal.v1",
                "in_response_to": escalation_id,
                "authority": "recommend",
                "verdict": "Act",
                "job": "Record the smoke escalation and continue closed-loop processing.",
                "scope": {
                    "tenant": tenant,
                    "subject": subject,
                    "allowed_actions": ["report"],
                },
                "stop_condition": "One signed directive_outbox row exists for this escalation.",
                "return_expectation": "Return directive status through the gateway watch outbox surface.",
                "rationale": "Deterministic response proving the signed path without provider spend.",
            }

        content = "```json\n" + json.dumps(proposal, separators=(",", ":")) + "\n```"
        response = {
            "id": f"chatcmpl-{PROFILE}-stub",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": "council-triage",
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": content},
                    "finish_reason": "stop",
                }
            ],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
        headers = {
            "X-Council-Session-Id": f"{PROFILE}-stub-{int(time.time())}",
            "X-Total-Cost-Usd": "0",
            "X-Chair-Tokens": "0",
        }
        self.send_json(200, response, headers)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
