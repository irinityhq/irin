#!/usr/bin/env python3
"""
codex-proxy.py — Host-side HTTP proxy for Codex CLI non-interactive mode.

Accepts OpenAI-compatible chat/completions requests on localhost:9091
and pipes them through `codex exec` (ChatGPT Pro / Plus OAuth session,
no raw OPENAI_API_KEY needed).

This bridges the gap between the Dockerized gateway and the host's
Codex CLI auth — mirrors tools/claude-proxy.py for the Anthropic side.

Usage:
    python3 codex-proxy.py                  # Start on :9091
    python3 codex-proxy.py --port 9092      # Custom port
    python3 codex-proxy.py --model gpt-5.5  # Override default model

The gateway routes gpt-cli provider requests here instead of api.openai.com.
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

# Use shared DoS limits (P1-11)
sys.path.insert(0, os.path.dirname(__file__))
from proxy_limits import create_ip_buckets, create_executor

DEFAULT_PORT = 9091
DEFAULT_MODEL = "gpt-5.5"

# Optional shared-secret bearer authentication.
# When CODEX_PROXY_TOKEN is set in the environment, every POST request must
# carry `X-Proxy-Auth: Bearer <token>`. Compared in constant time to defeat
# timing oracles. When the env var is absent, auth is disabled — only safe
# for --bind 127.0.0.1 (the default).
PROXY_TOKEN = os.environ.get("CODEX_PROXY_TOKEN", "").strip()

# Map full model strings to codex CLI model args. Codex's `-m <model>` accepts
# the full ID; we keep a passthrough map plus short aliases for convenience.
CLI_ALIASES = {
    "gpt-5.6-sol": "gpt-5.6-sol",
    "gpt-5.5": "gpt-5.5",
    "gpt-5.5-pro": "gpt-5.5-pro",
    "gpt-5.4": "gpt-5.4",
    "gpt-5.4-pro": "gpt-5.4-pro",
    "gpt-5.4-mini": "gpt-5.4-mini",
    "gpt-5.4-nano": "gpt-5.4-nano",
    "gpt-5.3-codex": "gpt-5.3-codex",
    # Short names
    "gpt": "gpt-5.5",
}

# Reasoning effort per pinned rule (`feedback_codex_effort_medium_minimum.md`).
# Override per-request via `x-reasoning-effort` header or `reasoning_effort`
# body field. Codex accepts: minimal / low / medium / high.
DEFAULT_EFFORT = "medium"
ALLOWED_EFFORT = {"minimal", "low", "medium", "high"}

# P1-11 DoS hardening via shared.
IP_BUCKETS = create_ip_buckets(capacity=5, rate=10.0 / 60)
CONCURRENCY_LIMIT = 3
EXECUTOR = create_executor(max_workers=CONCURRENCY_LIMIT)

def _resolve_model(model_str: str) -> str | None:
    """Map a request model string to a codex -m argument.

    Unknown model strings return None and the handler turns them into HTTP 400.
    The
    previous gpt-* passthrough let any string starting with `gpt-`
    through to `subprocess.run` — not a shell-injection vector under
    a list-form argv, but it surrendered any defense against weird
    or hostile model strings reaching codex.
    """
    if model_str in CLI_ALIASES:
        return CLI_ALIASES[model_str]
    return None


def _chat_to_prompt(body: dict) -> tuple[str, str, str, str]:
    """Extract prompt, system, model, effort from a chat/completions body.

    A `model` of "" in the returned tuple signals the request specified a
    model that's not in CLI_ALIASES — the handler converts that into
    HTTP 400 instead of silently rerouting to DEFAULT_MODEL.
    """
    model_str = body.get("model", DEFAULT_MODEL)
    model = _resolve_model(model_str) or ""

    # Effort: request body wins; otherwise default.
    effort = body.get("reasoning_effort", DEFAULT_EFFORT)
    if effort not in ALLOWED_EFFORT:
        effort = DEFAULT_EFFORT

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
            if system:
                system += "\n\n" + content
            else:
                system = content
        elif role == "user":
            parts.append(content)
        elif role == "assistant":
            parts.append(f"[Previous assistant response: {content}]")

    # Codex has no first-class system slot in `exec`; fold it into the prompt
    # the same way claude-proxy does for callers that don't supply one.
    if system:
        prompt = f"<system>\n{system}\n</system>\n\n{chr(10).join(parts)}"
    else:
        prompt = "\n\n".join(parts)
    return prompt, system, model, effort


def _call_codex(prompt: str, model: str, effort: str) -> dict:
    """Pipe prompt through `codex exec --json` and parse the event stream.

    `--json` emits one JSON object per line: thread.started, turn.started,
    item.completed (with agent_message text), turn.completed (with usage).
    Non-JSON lines from CLI wrappers or stderr are ignored.
    """
    cmd = [
        "codex", "exec",
        "--skip-git-repo-check",
        "--json",
        "-c", f"model_reasoning_effort={effort}",
        "-m", model,
        "-",  # read prompt from stdin
    ]

    t0 = time.time()
    try:
        result = subprocess.run(
            cmd, input=prompt, capture_output=True, text=True, timeout=600,
        )
        latency_ms = int((time.time() - t0) * 1000)

        if result.returncode != 0:
            stderr = (result.stderr or "").strip()[:400]
            return {
                "error": f"codex CLI exit {result.returncode}: {stderr}",
                "latency_ms": latency_ms,
            }

        text = ""
        input_tokens = 0
        cached_input_tokens = 0
        output_tokens = 0
        for line in (result.stdout or "").splitlines():
            if not line.strip().startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            etype = ev.get("type")
            if etype == "item.completed":
                item = ev.get("item", {})
                if item.get("type") == "agent_message":
                    # If multiple agent_message items arrive, concatenate.
                    chunk = item.get("text", "")
                    text = text + chunk if text else chunk
            elif etype == "turn.completed":
                u = ev.get("usage", {})
                input_tokens = int(u.get("input_tokens", 0) or 0)
                cached_input_tokens = int(u.get("cached_input_tokens", 0) or 0)
                # output_tokens does NOT include reasoning_output_tokens by
                # default in codex's emission — sum both so billing-equivalent
                # accounting captures the full chair effort.
                output_tokens = int(u.get("output_tokens", 0) or 0) + \
                                int(u.get("reasoning_output_tokens", 0) or 0)

        return {
            "text": text,
            "model": f"codex-cli-{model}",
            "usage": {
                "input_tokens": input_tokens,
                "cached_input_tokens": cached_input_tokens,
                "output_tokens": output_tokens,
            },
            "latency_ms": latency_ms,
        }
    except subprocess.TimeoutExpired:
        return {
            "error": "codex CLI timeout (600s)",
            "latency_ms": int((time.time() - t0) * 1000),
        }
    except FileNotFoundError:
        return {
            "error": "codex CLI not found in PATH",
            "latency_ms": 0,
        }
    except Exception as e:
        return {
            "error": f"codex CLI: {e}",
            "latency_ms": int((time.time() - t0) * 1000),
        }


def _to_openai_response(result: dict, model: str) -> dict:
    """Convert codex CLI result to OpenAI chat/completions response."""
    if "error" in result:
        return {"error": {"message": result["error"], "type": "proxy_error"}}

    usage = result.get("usage", {})
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
            "completion_tokens": usage.get("output_tokens", 0),
            "total_tokens": usage.get("input_tokens", 0) + usage.get("output_tokens", 0),
        },
        "x_latency_ms": result.get("latency_ms", 0),
    }


class CodexProxyHandler(BaseHTTPRequestHandler):
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
                "service": "codex-proxy",
                "backend": "codex-cli",
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
        """Enforce shared-secret auth when CODEX_PROXY_TOKEN is set.

        Returns True when the request may proceed, False after sending 401.
        Uses constant-time comparison so attempted brute-force can't be
        timed. Disabled when PROXY_TOKEN is empty — only safe with the
        default --bind 127.0.0.1.
        """
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

        # P1-11: per-IP rate limit before we spend CPU on body parse or CLI.
        client_ip = self.client_address[0]
        if not IP_BUCKETS[client_ip].allow():
            self._json_response(429, {
                "error": {
                    "type": "rate_limited",
                    "message": "per-IP rate limit exceeded",
                }
            })
            return

        # Accept both chat/completions and /v1/responses paths; the latter is
        # what conf/models.json currently sets for gpt-* entries (OpenAI's
        # newer Responses API). We treat them identically here — the prompt
        # we forward to codex is plain text either way.
        if self.path not in ("/v1/chat/completions", "/chat/completions",
                             "/v1/responses", "/responses"):
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

        prompt, _system, model, effort = _chat_to_prompt(body)
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

        self.log_message(f"POST {self.path} model={model} effort={effort} prompt={len(prompt)}ch")

        # P1-11: submit to bounded executor so we never have more than
        # CONCURRENCY_LIMIT simultaneous codex exec processes.
        future = EXECUTOR.submit(_call_codex, prompt, model, effort)
        try:
            result = future.result(timeout=600)
        except Exception as e:  # includes concurrent.futures.TimeoutError
            result = {"error": f"executor error: {e}", "latency_ms": 0}

        response = _to_openai_response(result, model)
        code = 200 if "error" not in response else 502
        self.log_message(
            f"  → {code} {result.get('latency_ms', 0)}ms tokens="
            f"{result.get('usage', {}).get('output_tokens', '?')}"
        )
        self._json_response(code, response)


def main():
    parser = argparse.ArgumentParser(description="Codex CLI HTTP proxy")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT,
                        help=f"Listen port (default: {DEFAULT_PORT})")
    # P0-1: default to loopback so an unconfigured proxy is not exposed on
    # any reachable network interface. Set `--bind 0.0.0.0` (or `--bind ::`)
    # to expose to the Docker bridge for gateway-in-container access; in
    # that case, CODEX_PROXY_TOKEN must be set so the proxy enforces the
    # shared-secret check.
    parser.add_argument("--bind", default="127.0.0.1",
                        help="Listen address (default: 127.0.0.1). Use "
                             "0.0.0.0 or :: for Docker bridge access; "
                             "requires CODEX_PROXY_TOKEN env var to be set.")
    parser.add_argument("--host",
                        help="Deprecated alias for --bind (kept for "
                             "compatibility; --bind takes precedence).")
    args = parser.parse_args()
    bind_addr = args.bind or args.host or "127.0.0.1"

    # Refuse to start a token-less proxy on a non-loopback interface.
    is_loopback = bind_addr in ("127.0.0.1", "::1", "localhost")
    if not is_loopback and not PROXY_TOKEN:
        print("ERROR: --bind {} requires CODEX_PROXY_TOKEN in the environment "
              "so the proxy can enforce X-Proxy-Auth on incoming requests. "
              "Set the env var or bind to 127.0.0.1.".format(bind_addr),
              file=sys.stderr)
        sys.exit(2)

    try:
        r = subprocess.run(["codex", "--version"], capture_output=True,
                           text=True, timeout=5)
        version = r.stdout.strip() if r.returncode == 0 else "unknown"
    except FileNotFoundError:
        print("ERROR: codex CLI not found in PATH.", file=sys.stderr)
        sys.exit(1)

    # A present binary can still be logged out. The status command is
    # zero-spend and exits non-zero when no ChatGPT/API login is usable.
    auth_ready = False
    for attempt in range(3):
        try:
            auth = subprocess.run(["codex", "login", "status"], capture_output=True,
                                  text=True, timeout=15)
            if auth.returncode == 0:
                auth_ready = True
                break
        except subprocess.TimeoutExpired:
            pass
        if attempt < 2:
            time.sleep(1)
    if not auth_ready:
        print("ERROR: codex CLI is not authenticated. Run: codex login",
              file=sys.stderr)
        sys.exit(1)

    # Dual-stack IPv6 so Docker containers reach us via host.docker.internal,
    # which can resolve to IPv6 on Docker Desktop. When --bind specifies an
    # IPv4 address we fall back to the plain HTTPServer for compatibility.
    import socket

    class DualStackHTTPServer(HTTPServer):
        address_family = socket.AF_INET6

        def server_bind(self):
            self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
            super().server_bind()

    if ":" in bind_addr or bind_addr == "::":
        server = DualStackHTTPServer((bind_addr, args.port), CodexProxyHandler)
    else:
        server = HTTPServer((bind_addr, args.port), CodexProxyHandler)
    auth_status = "X-Proxy-Auth: enforced" if PROXY_TOKEN else "auth: disabled (loopback only)"
    print("╔══════════════════════════════════════════════════╗")
    print(f"║  Codex CLI Proxy — {version:<30}║")
    print(f"║  Listening on {bind_addr}:{args.port:<30}║")
    print(f"║  {auth_status:<48}║")
    print("║  Backend: codex exec (ChatGPT Pro/Plus OAuth)   ║")
    print("╚══════════════════════════════════════════════════╝")
    print("Gateway config:")
    print(f"  base_url: http://host.docker.internal:{args.port}")
    if PROXY_TOKEN:
        print("  Set CODEX_PROXY_TOKEN in the gateway container env to match.")
    print("  POST /v1/chat/completions  (OpenAI format)")
    print(f"  DoS: per-IP 5-burst/10-per-min + {CONCURRENCY_LIMIT}-way concurrency cap")
    print("  POST /v1/responses         (OpenAI Responses API path passthrough)")
    print("  GET  /health")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down...")
        server.shutdown()
        EXECUTOR.shutdown(wait=True, cancel_futures=True)


if __name__ == "__main__":
    main()
