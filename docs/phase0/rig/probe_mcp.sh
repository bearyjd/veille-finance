#!/usr/bin/env bash
# Phase 0 MCP probe — captures golden responses and runs cross-family reach attempts.
# Usage: probe_mcp.sh <capture-dir> <bearer-token>
set -euo pipefail

BASE="http://127.0.0.1:3210/mcp"
OUT="$1"
TOKEN="$2"
mkdir -p "$OUT"

rpc() { # rpc <name> <json-body>
  local name="$1" body="$2"
  curl -s -X POST "$BASE" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$body" | python3 -m json.tool > "$OUT/$name.json" 2>"$OUT/$name.err" || true
  echo "== $name: $(head -c 120 "$OUT/$name.json" | tr -d '\n')"
}

rpc initialize '{"jsonrpc":"2.0","id":1,"method":"initialize"}'
rpc tools_list '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
rpc get_accounts '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_accounts","arguments":{}}}'
rpc get_transactions_p1 '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_transactions","arguments":{"order":"desc","page":1}}}'
rpc get_holdings_p1 '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_holdings","arguments":{"page":1}}}'
rpc get_balance_sheet '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"get_balance_sheet","arguments":{}}}'

# Cross-family reach attempts (expect: no Beta data when scoped to Alpha)
rpc xfam_search_canary '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"get_transactions","arguments":{"order":"desc","page":1,"search":"BETA-CANARY"}}}'
rpc xfam_account_filter '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"get_transactions","arguments":{"order":"desc","page":1,"accounts":["BETA-CANARY-CHECKING"]}}}'

# Auth negative case: wrong token must 401
code=$(curl -s -o "$OUT/bad_token.json" -w '%{http_code}' -X POST "$BASE" \
  -H "Authorization: Bearer wrong-token" -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":9,"method":"tools/list"}')
echo "== bad_token: HTTP $code"

# Scan all captures for canary leakage
echo "== canary scan (expect no output when scoped to Alpha):"
grep -l "BETA-CANARY" "$OUT"/*.json 2>/dev/null || echo "   no canary strings found in $OUT"
