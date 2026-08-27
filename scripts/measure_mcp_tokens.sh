#!/usr/bin/env bash
# 测量 tools/list 字节数。用途:tools/list 会话级注入,是 RI 对比 cb 的固定 token 税。
# 用法: scripts/measure_mcp_tokens.sh [binary] [database]
BIN="${1:-./target/debug/repo-intelligence}"
DB="${2:-/tmp/vue-mini/.repo-intelligence/workspace.sqlite}"
printf '%s\n%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
 | "$BIN" --database "$DB" mcp 2>/dev/null | tail -1 | python3 -c "
import json,sys
d=json.load(sys.stdin); tools=d['result']['tools']
print(f'total_bytes={len(json.dumps(d))} tools={len(tools)}')
for t in sorted(tools,key=lambda x:-len(json.dumps(x))):
    print(f\"  {t['name']:22s} {len(json.dumps(t)):>6}\")
"
