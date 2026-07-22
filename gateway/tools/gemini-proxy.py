#!/usr/bin/env python3
"""
gemini-proxy.py — Host-side HTTP proxy for Gemini CLI headless mode.

Accepts OpenAI-compatible chat/completions requests on localhost:9092
and pipes them through `gemini -p "" -m <model> -o json` (Google
Code Assist / Gemini subscription OAuth, no GCP API key needed).

Mirrors tools/claude-proxy.py and tools/codex-proxy.py for the Google
side — three CLIs, one shape.

Usage:
    python3 gemini-proxy.py                                # :9092 loopback
    python3 gemini-proxy.py --bind 0.0.0.0 --port 9092     # docker bridge
        (requires GEMINI_PROXY_TOKEN env var set)
"""

import argparse
import hmac
import json
import os
import subprocess
import sys
import time
import uuid
from http.server import HTTPServer, BaseHTTPRequestHandler

DEFAULT_PORT = 9092
DEFAULT_MODEL = "gemini-3.1-pro-preview"

PROXY_TOKEN = os.environ.get("GEMINI_PROXY_TOKEN", "").strip()

CLI_ALIASES = {
    "gemini-3.1-pro-preview": "gemini-3.1-pro-preview",
    "gemini-3.1-pro-preview-customtools": "gemini-3.1-pro-preview-customtools",
    "gemini-3-flash-preview": "gemini-3-flash-preview",
    "gemini-pro-latest": "gemini-3.1-pro-preview",
    "gemini-flash-latest": "gemini-3-flash-preview",
    "gemini": "gemini-3.1-pro-preview",
    "gemini-pro": "gemini-3.1-pro-preview",
    "gemini-flash": "gemini-3-flash-preview",
}


def _resolve_model(model_str: str):
    return CLI_ALIASES.get(model_str)


def _chat_to_prompt(body: dict):
    model_str = body.get("model", DEFAULT_MODEL)
    model = _resolve_model(model_str) or ""

    system = ""
    parts = []
    for msg in body.get("messages", []):
        role = msg.get("role", "user")
        content = msg.get("content", "")
        if isinstance(content, list):
            content = "\n".join(
                b.get("text", "") for b in content
                if isinstance(b, dict) and b.get("type", "text") == "text"
            )
        if role == "system":
            system = system + "\n\n" + content if system else content
        elif role == "user":
            parts.append(content)
        elif role == "assistant":
            parts.append(f"[Previous assistant response: {content}]")

    if system:
        prompt = f"<system>\n{system}\n</system>\n\n{chr(10).join(parts)}"
    else:
        prompt = "\n\n".join(parts)
    return prompt, model


def _call_gemini(prompt: str, model: str) -> dict:
    """Run `gemini -p "" -m <model> -o json` with prompt on stdin.

    `--approval-mode=plan --skip-trust -e ''` gives a read-only headless
    session with no extensions loaded. cwd=/tmp keeps any project
    GEMINI.md out of the context. Global ~/.gemini/skills will still
    load — that's the flat-rate overhead we accept.
    """
    cmd = [
        "gemini",
        "-p", "",
        "-m", model,
        "-o", "json",
        "--approval-mode=plan",
        "--skip-trust",
        "-e", "",
    ]
    t0 = time.time()
    try:
        result = subprocess.run(
            cmd, input=prompt, capture_output=True, text=True,
            timeout=600, cwd="/tmp",
        )
        latency_ms = int((time.time() - t0) * 1000)

        if result.returncode != 0:
            stderr = (result.stderr or "").strip()[:400]
            return {
                "error": f"gemini CLI exit {result.returncode}: {stderr}",
                "latency_ms": latency_ms,
            }

        try:
            data = json.loads(result.stdout)
        except json.JSONDecodeError:
            return {
                "error": f"gemini CLI returned non-JSON: {result.stdout[:200]!r}",
                "latency_ms": latency_ms,
            }

        text = data.get("response", "") or ""
        # Stats may be keyed by the *resolved* model name (e.g., a
        # `-latest` alias resolves to a versioned ID). Pull the first
        # entry rather than re-keying.
        stats_models = (data.get("stats") or {}).get("models") or {}
        tokens = {}
        resolved_model = model
        if stats_models:
            resolved_model, model_stats = next(iter(stats_models.items()))
            tokens = (model_stats or {}).get("tokens") or {}

        return {
            "text": text,
            "model": f"gemini-cli-{resolved_model}",
            "usage": {
                "input_tokens": int(tokens.get("input", 0) or 0),
                "output_tokens": int(tokens.get("candidates", 0) or 0),
                "cached_input_tokens": int(tokens.get("cached", 0) or 0),
                "reasoning_tokens": int(tokens.get("thoughts", 0) or 0),
            },
            "latency_ms": latency_ms,
        }
    except subprocess.TimeoutExpired:
        return {
            "error": "gemini CLI timeout (600s)",
            "latency_ms": int((time.time() - t0) * 1000),
        }
    except FileNotFoundError:
        return {"error": "gemini CLI not found in PATH", "latency_ms": 0}
    except Exception as e:
        return {
            "error": f"gemini CLI: {e}",
            "latency_ms": int((time.time() - t0) * 1000),
        }


def _to_openai_response(result: dict, model: str) -> dict:
    if "error" in result:
        return {"error": {"message": result["error"], "type": "proxy_error"}}

    usage = result.get("usage", {})
    completion = usage.get("output_tokens", 0) + usage.get("reasoning_tokens", 0)
    return {
        "id": f"chatcmpl-{uuid.uuid4().hex[:12]}",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": result.get("model", model),
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": result.get("text", "")},
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": usage.get("input_tokens", 0),
            "completion_tokens": completion,
            "total_tokens": usage.get("input_tokens", 0) + completion,
        },
        "x_latency_ms": result.get("latency_ms", 0),
    }


class GeminiProxyHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        ts = time.strftime("%Y-%m-%dT%H:%M:%S")
        sys.stderr.write(f"[{ts}] {fmt % args if args else fmt}\n")

    def _json_response(self, code: int, body: dict):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(body).encode())

    def do_GET(self):
        if self.path == "/health":
            self._json_response(200, {
                "status": "ok",
                "service": "gemini-proxy",
                "backend": "gemini-cli",
            })
        elif self.path == "/v1/models":
            if not self._check_auth():
                return
            self._json_response(200, {
                "data": [{"id": k, "object": "model"} for k in CLI_ALIASES],
            })
        else:
            self._json_response(404, {"error": "not found"})

    def _check_auth(self) -> bool:
        if not PROXY_TOKEN:
            return True
        auth = self.headers.get("X-Proxy-Auth", "")
        if auth.startswith("Bearer "):
            auth = auth[len("Bearer "):]
        if not hmac.compare_digest(auth, PROXY_TOKEN):
            self._json_response(401, {
                "error": {
                    "type": "unauthorized",
                    "message": "X-Proxy-Auth bearer required",
                }
            })
            return False
        return True

    def do_POST(self):
        if not self._check_auth():
            return
        if self.path not in ("/v1/chat/completions", "/chat/completions",
                             "/v1/messages", "/messages"):
            self._json_response(404, {"error": f"unknown path: {self.path}"})
            return

        content_length = int(self.headers.get("Content-Length", 0))
        if content_length == 0:
            self._json_response(400, {"error": "empty body"})
            return
        try:
            body = json.loads(self.rfile.read(content_length))
        except json.JSONDecodeError as e:
            self._json_response(400, {"error": f"invalid JSON: {e}"})
            return

        prompt, model = _chat_to_prompt(body)
        if not prompt:
            self._json_response(400, {"error": "no prompt content found"})
            return
        if not model:
            self._json_response(400, {
                "error": {
                    "type": "invalid_request_error",
                    "message": "model not in allow-list",
                    "allowed_models": sorted(CLI_ALIASES.keys()),
                }
            })
            return

        self.log_message(f"POST {self.path} model={model} prompt={len(prompt)}ch")
        result = _call_gemini(prompt, model)
        response = _to_openai_response(result, model)
        code = 200 if "error" not in response else 502
        self.log_message(
            f"  → {code} {result.get('latency_ms', 0)}ms tokens="
            f"{result.get('usage', {}).get('output_tokens', '?')}"
        )
        self._json_response(code, response)


def main():
    parser = argparse.ArgumentParser(description="Gemini CLI HTTP proxy")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT,
                        help=f"Listen port (default: {DEFAULT_PORT})")
    parser.add_argument("--bind", default="127.0.0.1",
                        help="Listen address (default: 127.0.0.1). Use "
                             "0.0.0.0 or :: for Docker bridge access; "
                             "requires GEMINI_PROXY_TOKEN env var to be set.")
    parser.add_argument("--host",
                        help="Deprecated alias for --bind.")
    args = parser.parse_args()
    bind_addr = args.bind or args.host or "127.0.0.1"

    is_loopback = bind_addr in ("127.0.0.1", "::1", "localhost")
    if not is_loopback and not PROXY_TOKEN:
        print("ERROR: --bind {} requires GEMINI_PROXY_TOKEN in the environment "
              "so the proxy can enforce X-Proxy-Auth on incoming requests. "
              "Set the env var or bind to 127.0.0.1.".format(bind_addr),
              file=sys.stderr)
        sys.exit(2)

    try:
        r = subprocess.run(["gemini", "--version"], capture_output=True,
                           text=True, timeout=5)
        version = r.stdout.strip() if r.returncode == 0 else "unknown"
    except FileNotFoundError:
        print("ERROR: gemini CLI not found in PATH.", file=sys.stderr)
        sys.exit(1)

    import socket
    class DualStackHTTPServer(HTTPServer):
        address_family = socket.AF_INET6
        def server_bind(self):
            self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
            super().server_bind()

    if ":" in bind_addr or bind_addr == "::":
        server = DualStackHTTPServer((bind_addr, args.port), GeminiProxyHandler)
    else:
        server = HTTPServer((bind_addr, args.port), GeminiProxyHandler)

    auth_status = ("X-Proxy-Auth: enforced" if PROXY_TOKEN
                   else "auth: disabled (loopback only)")
    print(f"╔══════════════════════════════════════════════════╗")
    print(f"║  Gemini CLI Proxy — {version:<29}║")
    print(f"║  Listening on {bind_addr}:{args.port:<30}║")
    print(f"║  {auth_status:<48}║")
    print(f"║  Backend: gemini -p (Code Assist subscription)  ║")
    print(f"╚══════════════════════════════════════════════════╝")
    print(f"Gateway config:")
    print(f"  base_url: http://host.docker.internal:{args.port}")
    if PROXY_TOKEN:
        print(f"  Set GEMINI_PROXY_TOKEN in the gateway container env to match.")
    print(f"  POST /v1/chat/completions  (OpenAI format)")
    print(f"  GET  /health")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down...")
        server.shutdown()


if __name__ == "__main__":
    main()
