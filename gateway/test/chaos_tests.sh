#!/usr/bin/env bash
# ==========================================================================
# SSE Streaming Chaos Tests
#
# Requires:
#   1. Mock SSE server running: python3 test/mock_sse_server.py --port 9999
#   2. Gateway running with GW_ENABLE_STREAMING=1
#   3. A model alias "chaos" in models.json pointing to the mock server
#
# These tests validate adversarial upstream behaviors that smoke tests
# can't cover: TCP fragmentation, missing usage, mid-stream errors,
# forged usage, CRLF, Responses API shape.
# ==========================================================================

set -uo pipefail

GW_URL="${GW_URL:-http://localhost:18080}"
PASS=0
FAIL=0
SKIP=0

pass() { echo -e "\033[0;32mPASS\033[0m: $1"; ((PASS++)); }
fail() { echo -e "\033[0;31mFAIL\033[0m: $1"; ((FAIL++)); }
skip() { echo -e "\033[0;33mSKIP\033[0m: $1"; ((SKIP++)); }

# Helper: send streaming request to a chaos path via Python
chaos_request() {
    local path="$1"
    local timeout="${2:-10}"
    local auth_hdr="${GW_TEST_KEY:+Authorization: Bearer ${GW_TEST_KEY}}"
    timeout "$timeout" python3 -c "
import http.client, json, os, sys, urllib.parse
parsed = urllib.parse.urlparse(os.environ.get('GW_URL', '${GW_URL}'))
host = parsed.hostname or 'localhost'
port = parsed.port or (443 if parsed.scheme == 'https' else 80)
conn = http.client.HTTPConnection(host, port, timeout=${timeout})
body = json.dumps({'model':'chaos-${path}','stream':True,'messages':[{'role':'user','content':'test'}]})
headers = {'Content-Type':'application/json','X-Budget-Key':'chaos-test'}
auth = '${auth_hdr}'
if auth:
    k, v = auth.split(': ', 1)
    headers[k] = v
conn.request('POST', '/v1/chat/completions', body, headers)
resp = conn.getresponse()
sys.stdout.write(resp.read().decode())
conn.close()
" 2>/dev/null || true
}

echo "=== SSE Streaming Chaos Tests ==="
echo "Target: ${GW_URL}"
echo ""

# --- Pre-flight: verify mock server is reachable ---
echo "0. Pre-flight: mock server reachable..."
MOCK_CHECK=$(python3 -c "
import urllib.request
try:
    r = urllib.request.urlopen(urllib.request.Request('http://localhost:9999/v1/chat/completions/normal', data=b'{}', headers={'Content-Type':'application/json'}), timeout=5)
    print(r.status)
except:
    print('000')
" 2>/dev/null)
if [ "$MOCK_CHECK" != "200" ]; then
    echo "FATAL: Mock SSE server not running on :9999"
    echo "Start it: python3 test/mock_sse_server.py --port 9999"
    exit 1
fi
pass "Mock server on :9999"

# --- #1: Normal stream — baseline ---
echo "1. Normal SSE stream (baseline)..."
RESP=$(chaos_request "normal")
if echo "$RESP" | grep -q '"usage"'; then
    pass "Normal stream — usage in response"
else
    fail "Normal stream — no usage in response"
fi

# Check cost log for real usage (not estimate)
sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-normal"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q '"tokens_in":50'; then
    pass "Normal stream — cost accounting exact (tokens_in=50)"
else
    fail "Normal stream — cost accounting wrong: $LOG"
fi

# --- #2: No usage in final chunk ---
echo "2. Missing usage (estimate fallback)..."
RESP=$(chaos_request "no-usage")
sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-no-usage"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q 'tokens_out'; then
    # Should have an estimate, not zero
    pass "Missing usage — fallback estimate produced"
else
    fail "Missing usage — no cost log entry"
fi

# --- #3: TCP-fragmented usage chunk ---
echo "3. Fragmented usage chunk..."
RESP=$(chaos_request "fragmented")
sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-fragmented"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q '"tokens_in":50'; then
    pass "Fragmented — usage extracted correctly (tokens_in=50)"
else
    fail "Fragmented — usage extraction failed: $LOG"
fi

# --- #4: Forged usage (zero after real) ---
echo "4. Forged zero-usage after real usage..."
RESP=$(chaos_request "forged-usage")
sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-forged-usage"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q '"tokens_in":50'; then
    pass "Forged usage — real usage preserved (zero rejected)"
else
    fail "Forged usage — zero usage accepted: $LOG"
fi

# --- #5: CRLF line endings ---
echo "5. CRLF line endings..."
RESP=$(chaos_request "crlf")
sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-crlf"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q '"tokens_in":50'; then
    pass "CRLF — usage extracted correctly"
else
    fail "CRLF — usage extraction failed: $LOG"
fi

# --- #6: Responses API shape (named events) ---
echo "6. Responses API named events..."
RESP=$(chaos_request "responses-api")
sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-responses-api"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q '"tokens_in":50'; then
    pass "Responses API — usage extracted from response.completed"
else
    fail "Responses API — usage extraction failed: $LOG"
fi

# --- #7: Mid-stream 5xx ---
echo "7. Provider 5xx mid-stream..."
RESP=$(chaos_request "mid-5xx")
sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-mid-5xx"' | grep 'is_streaming' | tail -1)
if [ -n "$LOG" ]; then
    pass "Mid-5xx — ledger entry exists"
else
    fail "Mid-5xx — no cost/ledger entry"
fi

# --- #8: Anthropic stream (translated to OpenAI shape) ---
echo "8. Anthropic SSE stream (translated)..."
RESP=$(chaos_request "anthropic-stream")
if echo "$RESP" | grep -q 'chat.completion.chunk'; then
    pass "Anthropic stream — OpenAI chunks in response"
else
    fail "Anthropic stream — no OpenAI chunks: ${RESP:0:200}"
fi

sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-anthropic-stream"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q '"tokens_in":50'; then
    pass "Anthropic stream — usage extracted (tokens_in=50)"
else
    fail "Anthropic stream — usage extraction failed: $LOG"
fi

# --- #9: Anthropic tool-use stream ---
echo "9. Anthropic tool-use SSE stream..."
RESP=$(chaos_request "anthropic-tool")
if echo "$RESP" | grep -q 'tool_calls'; then
    pass "Anthropic tool stream — tool_calls in response"
else
    fail "Anthropic tool stream — no tool_calls: ${RESP:0:200}"
fi

sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-anthropic-tool"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q '"tokens_in":50'; then
    pass "Anthropic tool stream — usage extracted (tokens_in=50)"
else
    fail "Anthropic tool stream — usage extraction failed: $LOG"
fi

# --- #10: Vertex stream (translated to OpenAI shape) ---
echo "10. Vertex SSE stream (translated)..."
RESP=$(chaos_request "vertex-stream")
if echo "$RESP" | grep -q 'chat.completion.chunk'; then
    pass "Vertex stream — OpenAI chunks in response"
else
    fail "Vertex stream — no OpenAI chunks: ${RESP:0:200}"
fi

sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-vertex-stream"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q '"tokens_in":50'; then
    pass "Vertex stream — usage extracted (tokens_in=50)"
else
    fail "Vertex stream — usage extraction failed: $LOG"
fi

# --- #11: Responses API stream wrapping (chat.completion.chunk → response.*) ---
echo "11. Responses API stream wrapping..."
# Send a Responses-shape request (input[] not messages[]) to a provider that
# emits chat.completion.chunk. The gateway should wrap into response.* events.
RESP=$(timeout 10 python3 -c "
import http.client, json, os, sys, urllib.parse
parsed = urllib.parse.urlparse(os.environ.get('GW_URL', '${GW_URL}'))
host = parsed.hostname or 'localhost'
port = parsed.port or (443 if parsed.scheme == 'https' else 80)
conn = http.client.HTTPConnection(host, port, timeout=10)
body = json.dumps({'model':'chaos-responses-stream-wrap','stream':True,'input':[{'role':'user','content':'test'}],'max_output_tokens':100})
headers = {'Content-Type':'application/json','X-Budget-Key':'chaos-test'}
auth_key = '${GW_TEST_KEY:-}'
if auth_key:
    headers['Authorization'] = f'Bearer {auth_key}'
conn.request('POST', '/v1/chat/completions', body, headers)
resp = conn.getresponse()
sys.stdout.write(resp.read().decode())
conn.close()
" 2>/dev/null || true)

if echo "$RESP" | grep -q 'response.output_text.delta'; then
    pass "Responses stream wrap — output_text.delta events present"
else
    fail "Responses stream wrap — no output_text.delta: ${RESP:0:300}"
fi

if echo "$RESP" | grep -q 'response.completed'; then
    pass "Responses stream wrap — response.completed event present"
else
    fail "Responses stream wrap — no response.completed: ${RESP:0:300}"
fi

if echo "$RESP" | grep -q 'response.created'; then
    pass "Responses stream wrap — response.created event present"
else
    fail "Responses stream wrap — no response.created: ${RESP:0:300}"
fi

sleep 1
LOG=$(docker compose logs gateway --tail 20 2>&1 | grep '"model":"chaos-responses-stream-wrap"' | grep 'is_streaming' | tail -1)
if echo "$LOG" | grep -q '"tokens_in":50'; then
    pass "Responses stream wrap — usage extracted (tokens_in=50)"
else
    fail "Responses stream wrap — usage extraction failed: $LOG"
fi

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed, ${SKIP} skipped ==="
[ "$FAIL" -eq 0 ] || exit 1
