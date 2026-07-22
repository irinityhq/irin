#!/usr/bin/env python3
"""
claude-proxy.py — Host-side HTTP proxy for Claude CLI pipe mode.

Accepts OpenAI-compatible chat/completions requests on localhost:9090
and pipes them through `claude -p` (Max/ProMax subscription).

This bridges the gap between the Dockerized gateway and the host's
Claude CLI auth (OAuth session, no raw API key needed).

Usage:
    python3 claude-proxy.py                     # Start on :9090
    python3 claude-proxy.py --port 9091         # Custom port
    python3 claude-proxy.py --model sonnet      # Override model

The gateway routes Anthropic requests here instead of api.anthropic.com.
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

# P1-11: shared DoS limits (per-IP bucket + bounded concurrency)
sys.path.insert(0, os.path.dirname(__file__))
from proxy_limits import create_ip_buckets, create_executor

DEFAULT_PORT = 9090
DEFAULT_MODEL = "opus"  # claude CLI alias

# Optional shared-secret bearer authentication. When
# CLAUDE_PROXY_TOKEN is set, every POST request must carry
# `X-Proxy-Auth: Bearer <token>`. Compared in constant time.
PROXY_TOKEN = os.environ.get("CLAUDE_PROXY_TOKEN", "").strip()

# Map full model strings to claude CLI aliases.
# Keep in sync with conf/models.json `provider: "claude-cli"` entries.
CLI_ALIASES = {
    # Full ID, not "opus": the CLI's bare `opus` alias tracks latest-opus, so
    # pinning matters for the tryout baseline to actually measure 4-8.
    "claude-opus-4-8": "claude-opus-4-8",
    "claude-opus-4-7": "opus",
    "claude-opus-4-6": "opus",
    "claude-opus-4-5": "opus",
    "claude-sonnet-4-6": "sonnet",
    "claude-sonnet-4-5": "sonnet",
    "claude-haiku-4-5": "haiku",
    "claude-fable-5": "fable",
    # Short names pass through
    "opus": "opus",
    "sonnet": "sonnet",
    "haiku": "haiku",
    "fable": "fable",
}

# P1-11 DoS hardening: per-IP token bucket + bounded concurrency for claude CLI.
# Same params as codex-proxy for parity.
IP_BUCKETS = create_ip_buckets(capacity=5, rate=10.0 / 60)
CONCURRENCY_LIMIT = 3
EXECUTOR = create_executor(max_workers=CONCURRENCY_LIMIT)


def _resolve_model(model_str: str):
    """Map model string to claude CLI alias.

    Unknown strings return None and the handler converts them to HTTP 400.
    The previous fuzzy match
    (startswith / substring "opus") would map e.g. "claude-opus-99-evil"
    to "opus" and silently invoke `claude -p --model opus`, which is
    confusing at best and a future-proofing liability.
    """
    # Exact-match lookup only. Aliases are CLI_ALIASES keys verbatim.
    if model_str in CLI_ALIASES:
        return CLI_ALIASES[model_str]
    return None


def _chat_to_prompt(body: dict) -> tuple[str, str, str]:
    """Extract prompt, system message, and model from chat/completions body.

    Also handles Anthropic Messages format (system as top-level string,
    messages with content blocks) since the gateway translator may have
    already converted the body.
    """
    model_str = body.get("model", DEFAULT_MODEL)
    model = _resolve_model(model_str) or ""

    # System message: could be top-level string (Anthropic format)
    # or in messages array (OpenAI format)
    system = ""
    if isinstance(body.get("system"), str):
        system = body["system"]
    elif isinstance(body.get("system"), list):
        # Anthropic format: [{type: "text", text: "..."}]
        system = "\n\n".join(
            b["text"] for b in body["system"]
            if isinstance(b, dict) and b.get("type") == "text"
        )

    # Build prompt from messages
    parts = []
    for msg in body.get("messages", []):
        role = msg.get("role", "user")
        content = msg.get("content", "")

        # Handle content blocks (Anthropic format)
        if isinstance(content, list):
            content = "\n".join(
                b.get("text", "") for b in content
                if isinstance(b, dict) and b.get("type", "text") == "text"
            )

        if role == "system":
            # OpenAI format: system message in messages array
            if not system:
                system = content
        elif role == "user":
            parts.append(content)
        elif role == "assistant":
            parts.append(f"[Previous assistant response: {content}]")

    prompt = "\n\n".join(parts)
    return prompt, system, model


def _call_claude(prompt: str, system: str, model: str,
                 max_tokens: int = 4096) -> dict:
    """Pipe prompt through claude CLI and return structured response."""
    cmd = ["claude", "-p", "--model", model,
           "--output-format", "json", "--no-session-persistence"]
    if system:
        cmd.extend(["--system-prompt", system])

    t0 = time.time()
    try:
        result = subprocess.run(
            cmd, input=prompt, capture_output=True, text=True,
            timeout=300
        )
        latency_ms = int((time.time() - t0) * 1000)

        if result.returncode != 0:
            stderr = result.stderr.strip()[:200] if result.stderr else "unknown"
            return {"error": f"claude CLI exit {result.returncode}: {stderr}",
                    "latency_ms": latency_ms}

        try:
            data = json.loads(result.stdout)
        except json.JSONDecodeError:
            # Raw text fallback
            return {
                "text": result.stdout.strip(),
                "model": f"claude-cli-{model}",
                "usage": {"input_tokens": 0, "output_tokens": 0},
                "latency_ms": latency_ms,
            }

        if data.get("is_error"):
            return {"error": data.get("result", "unknown"),
                    "latency_ms": latency_ms}

        usage = data.get("usage", {})
        return {
            "text": data.get("result", ""),
            "model": data.get("model", f"claude-cli-{model}"),
            "usage": {
                "input_tokens": usage.get("input_tokens", 0),
                "output_tokens": usage.get("output_tokens", 0),
                "cache_read_input_tokens": usage.get("cache_read_input_tokens", 0),
            },
            "cost_usd": data.get("total_cost_usd", 0),
            "latency_ms": data.get("duration_ms", latency_ms),
        }
    except subprocess.TimeoutExpired:
        return {"error": "claude CLI timeout (300s)",
                "latency_ms": int((time.time() - t0) * 1000)}
    except FileNotFoundError:
        return {"error": "claude CLI not found — install with: npm i -g @anthropic-ai/claude-code",
                "latency_ms": 0}
    except Exception as e:
        return {"error": f"claude CLI: {e}",
                "latency_ms": int((time.time() - t0) * 1000)}


def _to_openai_response(result: dict, model: str) -> dict:
    """Convert claude CLI result to OpenAI chat/completions response."""
    if "error" in result:
        return {
            "error": {
                "message": result["error"],
                "type": "proxy_error",
            }
        }

    usage = result.get("usage", {})
    return {
        "id": f"chatcmpl-{uuid.uuid4().hex[:12]}",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": result.get("model", model),
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": result.get("text", ""),
            },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": usage.get("input_tokens", 0),
            "completion_tokens": usage.get("output_tokens", 0),
            "total_tokens": (usage.get("input_tokens", 0) +
                           usage.get("output_tokens", 0)),
        },
        # Extra: pass cost through for gateway accounting
        "x_cost_usd": result.get("cost_usd", 0),
        "x_latency_ms": result.get("latency_ms", 0),
    }


class ClaudeProxyHandler(BaseHTTPRequestHandler):
    """HTTP handler for claude proxy."""

    def log_message(self, format, *args):
        """Structured logging."""
        ts = time.strftime("%Y-%m-%dT%H:%M:%S")
        msg = format % args if args else format
        sys.stderr.write(f"[{ts}] {msg}\n")

    def _json_response(self, code: int, body: dict):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(body).encode())

    def do_GET(self):
        if self.path == "/health":
            self._json_response(200, {
                "status": "ok",
                "service": "claude-proxy",
                "backend": "claude-cli",
            })
        elif self.path == "/v1/models":
            # Render from CLI_ALIASES so this cannot drift from the allow-list.
            if not self._check_auth():
                return
            self._json_response(200, {
                "data": [{"id": k, "object": "model"} for k in CLI_ALIASES],
            })
        else:
            self._json_response(404, {"error": "not found"})

    def _check_auth(self) -> bool:
        """Enforce shared-secret auth when CLAUDE_PROXY_TOKEN is set."""
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

        # P1-11: per-IP rate limit (before body parse or CLI).
        client_ip = self.client_address[0]
        if not IP_BUCKETS[client_ip].allow():
            self._json_response(429, {
                "error": {
                    "type": "rate_limited",
                    "message": "per-IP rate limit exceeded",
                }
            })
            return

        # Accept both /v1/chat/completions and /v1/messages
        if self.path not in ("/v1/chat/completions", "/v1/messages",
                             "/chat/completions", "/messages"):
            self._json_response(404, {"error": f"unknown path: {self.path}"})
            return

        # Read body
        content_length = int(self.headers.get("Content-Length", 0))
        if content_length == 0:
            self._json_response(400, {"error": "empty body"})
            return

        try:
            body = json.loads(self.rfile.read(content_length))
        except json.JSONDecodeError as e:
            self._json_response(400, {"error": f"invalid JSON: {e}"})
            return

        # Extract prompt from body
        prompt, system, model = _chat_to_prompt(body)
        max_tokens = body.get("max_tokens", 4096)

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

        # P1-11: submit to bounded executor (caps concurrent claude CLI processes).
        future = EXECUTOR.submit(_call_claude, prompt, system, model, max_tokens)
        try:
            result = future.result(timeout=300)
        except Exception as e:
            result = {"error": f"executor error: {e}", "latency_ms": 0}

        # Convert to OpenAI format
        response = _to_openai_response(result, model)

        code = 200 if "error" not in response else 502
        self.log_message(f"  → {code} {result.get('latency_ms', 0)}ms "
                        f"tokens={result.get('usage', {}).get('input_tokens', '?')}→"
                        f"{result.get('usage', {}).get('output_tokens', '?')}")
        self._json_response(code, response)


def main():
    parser = argparse.ArgumentParser(description="Claude CLI HTTP proxy")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT,
                       help=f"Listen port (default: {DEFAULT_PORT})")
    # P0-1: default to loopback; explicit opt-in for Docker bridge access.
    parser.add_argument("--bind", default="127.0.0.1",
                        help="Listen address (default: 127.0.0.1). Use "
                             "0.0.0.0 or :: for Docker bridge access; "
                             "requires CLAUDE_PROXY_TOKEN env var to be set.")
    parser.add_argument("--host",
                        help="Deprecated alias for --bind.")
    args = parser.parse_args()
    bind_addr = args.bind or args.host or "127.0.0.1"

    is_loopback = bind_addr in ("127.0.0.1", "::1", "localhost")
    if not is_loopback and not PROXY_TOKEN:
        print("ERROR: --bind {} requires CLAUDE_PROXY_TOKEN in the environment "
              "so the proxy can enforce X-Proxy-Auth on incoming requests. "
              "Set the env var or bind to 127.0.0.1.".format(bind_addr),
              file=sys.stderr)
        sys.exit(2)

    # Verify claude CLI is available
    try:
        r = subprocess.run(["claude", "--version"], capture_output=True,
                          text=True, timeout=5)
        version = r.stdout.strip() if r.returncode == 0 else "unknown"
    except FileNotFoundError:
        print("ERROR: claude CLI not found. Install: npm i -g @anthropic-ai/claude-code",
              file=sys.stderr)
        sys.exit(1)

    # `--version` proves only that the executable exists. Refuse to advertise
    # static models unless the subscription-backed CLI is actually logged in.
    auth_ready = False
    for attempt in range(3):
        try:
            auth = subprocess.run(["claude", "auth", "status"], capture_output=True,
                                  text=True, timeout=15)
            auth_data = json.loads(auth.stdout or "{}")
            if auth.returncode == 0 and auth_data.get("loggedIn") is True:
                auth_ready = True
                break
        except (subprocess.TimeoutExpired, json.JSONDecodeError):
            pass
        if attempt < 2:
            time.sleep(1)
    if not auth_ready:
        print("ERROR: claude CLI is not authenticated. Run: claude auth login",
              file=sys.stderr)
        sys.exit(1)

    # Use dual-stack IPv6 socket so Docker containers can reach us
    # (host.docker.internal resolves to IPv6 on some Docker Desktop setups)
    import socket
    class DualStackHTTPServer(HTTPServer):
        address_family = socket.AF_INET6
        def server_bind(self):
            # Allow both IPv4 and IPv6 connections
            self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
            super().server_bind()

    if ":" in bind_addr or bind_addr == "::":
        server = DualStackHTTPServer((bind_addr, args.port), ClaudeProxyHandler)
    else:
        server = HTTPServer((bind_addr, args.port), ClaudeProxyHandler)
    auth_status = "X-Proxy-Auth: enforced" if PROXY_TOKEN else "auth: disabled (loopback only)"
    print(f"╔══════════════════════════════════════════════════╗")
    print(f"║  Claude CLI Proxy — {version:<29}║")
    print(f"║  Listening on {bind_addr}:{args.port:<30}║")
    print(f"║  {auth_status:<48}║")
    print(f"║  Backend: claude -p (Max/ProMax subscription)   ║")
    print(f"╚══════════════════════════════════════════════════╝")
    print(f"Gateway config:")
    print(f"  base_url: http://host.docker.internal:{args.port}")
    if PROXY_TOKEN:
        print(f"  Set CLAUDE_PROXY_TOKEN in the gateway container env to match.")
    print(f"  POST /v1/chat/completions  (OpenAI format)")
    print(f"  POST /v1/messages          (Anthropic format)")
    print(f"  GET  /health")
    print(f"  DoS: per-IP 5-burst/10-per-min + {CONCURRENCY_LIMIT}-way concurrency cap")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down...")
        server.shutdown()
        EXECUTOR.shutdown(wait=True, cancel_futures=True)


if __name__ == "__main__":
    main()
