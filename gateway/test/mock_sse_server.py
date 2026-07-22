#!/usr/bin/env python3
"""
Mock SSE server for gateway chaos tests.

Simulates adversarial upstream LLM provider behaviors:
  /normal          — clean SSE stream with usage in final chunk
  /no-usage        — stream completes but final chunk has no usage
  /fragmented      — usage chunk split across TCP writes
  /mid-5xx         — sends 2 chunks then errors
  /slow            — 1 chunk per 5 seconds (timeout test)
  /forged-usage    — real usage then a zero-usage frame after
  /crlf            — CRLF line endings
  /responses-api   — OpenAI Responses API shape (named events)

Usage:
  python3 test/mock_sse_server.py [--port 9999]
  GW_URL=http://localhost:18080 bash test/chaos_tests.sh
"""

import argparse
import json
import time
from http.server import HTTPServer, BaseHTTPRequestHandler


USAGE_CHUNK = {
    "id": "chatcmpl-test",
    "object": "chat.completion.chunk",
    "choices": [],
    "usage": {
        "prompt_tokens": 50,
        "completion_tokens": 100,
        "total_tokens": 150,
    },
}

CONTENT_CHUNK = {
    "id": "chatcmpl-test",
    "object": "chat.completion.chunk",
    "choices": [{"delta": {"content": "hello"}, "index": 0}],
}

RESPONSES_COMPLETED = {
    "sequence_number": 10,
    "type": "response.completed",
    "response": {
        "model": "mock-model",
        "status": "completed",
        "usage": {
            "input_tokens": 50,
            "output_tokens": 100,
            "total_tokens": 150,
        },
    },
}


class ChaosHandler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

    def _sse_headers(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

    def _send_sse(self, data, flush=True):
        line = f"data: {json.dumps(data)}\n\n"
        self.wfile.write(line.encode())
        if flush:
            self.wfile.flush()

    def _send_done(self):
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_POST(self):
        content_len = int(self.headers.get("Content-Length", 0))
        self.rfile.read(content_len)

        path = self.path.split("?")[0]

        if path == "/v1/chat/completions/normal":
            self._sse_headers()
            for _ in range(3):
                self._send_sse(CONTENT_CHUNK)
            self._send_sse(USAGE_CHUNK)
            self._send_done()

        elif path == "/v1/chat/completions/no-usage":
            self._sse_headers()
            for _ in range(3):
                self._send_sse(CONTENT_CHUNK)
            self._send_done()

        elif path == "/v1/chat/completions/fragmented":
            self._sse_headers()
            self._send_sse(CONTENT_CHUNK)
            usage_line = f"data: {json.dumps(USAGE_CHUNK)}\n\n"
            usage_bytes = usage_line.encode()
            mid = len(usage_bytes) // 2
            self.wfile.write(usage_bytes[:mid])
            self.wfile.flush()
            time.sleep(0.1)
            self.wfile.write(usage_bytes[mid:])
            self.wfile.flush()
            self._send_done()

        elif path == "/v1/chat/completions/mid-5xx":
            self._sse_headers()
            self._send_sse(CONTENT_CHUNK)
            self._send_sse(CONTENT_CHUNK)
            error = json.dumps({"error": {"message": "internal error", "type": "server_error"}})
            self.wfile.write(f"data: {error}\n\n".encode())
            self.wfile.flush()

        elif path == "/v1/chat/completions/slow":
            self._sse_headers()
            self._send_sse(CONTENT_CHUNK)
            time.sleep(6)
            self._send_sse(USAGE_CHUNK)
            self._send_done()

        elif path == "/v1/chat/completions/forged-usage":
            self._sse_headers()
            self._send_sse(CONTENT_CHUNK)
            self._send_sse(USAGE_CHUNK)
            forged = {
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "choices": [],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            }
            self._send_sse(forged)
            self._send_done()

        elif path == "/v1/chat/completions/crlf":
            self._sse_headers()
            line = f"data: {json.dumps(CONTENT_CHUNK)}\r\n\r\n"
            self.wfile.write(line.encode())
            line = f"data: {json.dumps(USAGE_CHUNK)}\r\n\r\n"
            self.wfile.write(line.encode())
            self.wfile.write(b"data: [DONE]\r\n\r\n")
            self.wfile.flush()

        elif path == "/v1/chat/completions/responses-api":
            self._sse_headers()
            self.wfile.write(b"event: response.created\n")
            self.wfile.write(f"data: {json.dumps({'type': 'response.created', 'response': {'usage': None}})}\n\n".encode())
            self.wfile.flush()
            self.wfile.write(b"event: response.completed\n")
            self.wfile.write(f"data: {json.dumps(RESPONSES_COMPLETED)}\n\n".encode())
            self.wfile.flush()

        elif path == "/v1/chat/completions/anthropic-stream":
            # Full Anthropic Messages API SSE event sequence
            self._sse_headers()

            # message_start (with input usage)
            self.wfile.write(b"event: message_start\n")
            msg_start = json.dumps({
                "type": "message_start",
                "message": {
                    "id": "msg_test_123",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-opus-4-6",
                    "usage": {"input_tokens": 50, "output_tokens": 0},
                    "content": [],
                }
            })
            self.wfile.write(f"data: {msg_start}\n\n".encode())
            self.wfile.flush()

            # content_block_start (text)
            self.wfile.write(b"event: content_block_start\n")
            self.wfile.write(f'data: {json.dumps({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})}\n\n'.encode())
            self.wfile.flush()

            # 3x content_block_delta (text)
            for word in ["Hello", " from", " Anthropic"]:
                self.wfile.write(b"event: content_block_delta\n")
                delta = json.dumps({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": word}})
                self.wfile.write(f"data: {delta}\n\n".encode())
                self.wfile.flush()

            # content_block_stop
            self.wfile.write(b"event: content_block_stop\n")
            self.wfile.write(f'data: {json.dumps({"type": "content_block_stop", "index": 0})}\n\n'.encode())
            self.wfile.flush()

            # message_delta (stop_reason + output usage)
            self.wfile.write(b"event: message_delta\n")
            msg_delta = json.dumps({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                "usage": {"output_tokens": 100},
            })
            self.wfile.write(f"data: {msg_delta}\n\n".encode())
            self.wfile.flush()

            # message_stop
            self.wfile.write(b"event: message_stop\n")
            self.wfile.write(f'data: {json.dumps({"type": "message_stop"})}\n\n'.encode())
            self.wfile.flush()

        elif path == "/v1/chat/completions/anthropic-tool":
            # Anthropic SSE with tool_use block (input_json_delta fragments)
            self._sse_headers()

            # message_start
            self.wfile.write(b"event: message_start\n")
            msg_start = json.dumps({
                "type": "message_start",
                "message": {
                    "id": "msg_tool_456",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-opus-4-6",
                    "usage": {"input_tokens": 50, "output_tokens": 0},
                    "content": [],
                }
            })
            self.wfile.write(f"data: {msg_start}\n\n".encode())
            self.wfile.flush()

            # content_block_start (tool_use)
            self.wfile.write(b"event: content_block_start\n")
            block_start = json.dumps({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {}}
            })
            self.wfile.write(f"data: {block_start}\n\n".encode())
            self.wfile.flush()

            # 3x input_json_delta (partial JSON fragments)
            for fragment in ['{"lo', 'cation": "San', ' Francisco"}']:
                self.wfile.write(b"event: content_block_delta\n")
                delta = json.dumps({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": fragment}
                })
                self.wfile.write(f"data: {delta}\n\n".encode())
                self.wfile.flush()

            # content_block_stop (triggers tool_calls emission)
            self.wfile.write(b"event: content_block_stop\n")
            self.wfile.write(f'data: {json.dumps({"type": "content_block_stop", "index": 0})}\n\n'.encode())
            self.wfile.flush()

            # message_delta
            self.wfile.write(b"event: message_delta\n")
            msg_delta = json.dumps({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 80},
            })
            self.wfile.write(f"data: {msg_delta}\n\n".encode())
            self.wfile.flush()

            # message_stop
            self.wfile.write(b"event: message_stop\n")
            self.wfile.write(f'data: {json.dumps({"type": "message_stop"})}\n\n'.encode())
            self.wfile.flush()

        elif path == "/v1/chat/completions/responses-stream-wrap":
            # Emits chat.completion.chunk frames (standard provider shape).
            # Gateway should wrap these into response.* events when the client
            # sent a Responses-shape request (input[] instead of messages[]).
            self._sse_headers()
            for word in ["Hello", " from", " wrapped"]:
                chunk = {
                    "id": "chatcmpl-wrap",
                    "object": "chat.completion.chunk",
                    "choices": [{"delta": {"content": word}, "index": 0}],
                }
                self._send_sse(chunk)
            # Final chunk with finish_reason and usage
            final_chunk = {
                "id": "chatcmpl-wrap",
                "object": "chat.completion.chunk",
                "choices": [{"delta": {}, "index": 0, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 50,
                    "completion_tokens": 100,
                    "total_tokens": 150,
                },
            }
            self._send_sse(final_chunk)
            self._send_done()

        elif path == "/v1/chat/completions/vertex-stream":
            # Vertex AI streamGenerateContent SSE
            self._sse_headers()

            # 3x content chunks
            for word in ["Hello", " from", " Vertex"]:
                chunk = json.dumps({
                    "candidates": [{
                        "content": {"parts": [{"text": word}], "role": "model"},
                    }]
                })
                self.wfile.write(f"data: {chunk}\n\n".encode())
                self.wfile.flush()

            # Final chunk with usageMetadata and finishReason
            final = json.dumps({
                "candidates": [{
                    "content": {"parts": [{"text": "!"}], "role": "model"},
                    "finishReason": "STOP",
                }],
                "usageMetadata": {
                    "promptTokenCount": 50,
                    "candidatesTokenCount": 100,
                    "totalTokenCount": 150,
                }
            })
            self.wfile.write(f"data: {final}\n\n".encode())
            self.wfile.flush()

        else:
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b'{"error":"unknown chaos path"}')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=9999)
    args = parser.parse_args()

    server = HTTPServer(("0.0.0.0", args.port), ChaosHandler)
    print(f"Mock SSE server on :{args.port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
