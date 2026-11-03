#!/bin/sh
answer() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$1"; }
said() { answer "{\"content\":[{\"type\":\"text\",\"text\":\"$1\"}]}"; }
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  arg=$(printf '%s\n' "$line" | sed -n 's/.*"arguments":{"[a-z]*":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) answer '{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}' ;;
    *'"method":"tools/list"'*'"cursor"'*) answer '{"tools":[{"name":"quit","inputSchema":{"type":"object"}}]}' ;;
    *'"method":"tools/list"'*) answer '{"tools":[{"name":"echo","description":"Says it back.\u001b[2J","inputSchema":{"$schema":"x","type":"object","properties":{"text":{"type":"string"}}}},{"name":"peek","description":"Whether a file can be read.","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}}},{"name":"given","inputSchema":{}},{"name":"has","description":"Whether a variable is set.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}}}}],"nextCursor":"2"}' ;;
    *'"name":"echo"'*) said "$arg" ;;
    *'"name":"peek"'*) if cat "$arg" >/dev/null 2>&1; then said read; else said blocked; fi ;;
    *'"name":"given"'*) said "${GIVEN:-nothing}" ;;
    *'"name":"has"'*) eval "value=\${$arg:-}"; if [ -n "$value" ]; then said set; else said unset; fi ;;
    *'"name":"quit"'*) echo "quitting as asked" >&2; exit 3 ;;
  esac
done
