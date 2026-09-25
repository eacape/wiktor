#!/usr/bin/env bash
# Wiktor Step12 部署冒烟（spec step12 §4 B3，D6/A3）：systemd 活性、健康检查、
# API key 认证、console 可达。全部 PASS 输出 SMOKE PASS 并退出 0；任一 FAIL
# 退出 1。本机执行（所有端口都是 loopback）。
# Wiktor Step12 deployment smoke (spec step12 §4 B3, D6/A3): systemd liveness,
# health check, API-key auth, and console reachability. All PASS prints
# "SMOKE PASS" and exits 0; any FAIL exits 1. Run on the box itself (every port
# is loopback).
set -uo pipefail

HTTP="${WIKTOR_SMOKE_HTTP:-127.0.0.1:8080}"
DOMAIN="${WIKTOR_SMOKE_DOMAIN:-tech-docs}"
CONSOLE="${WIKTOR_SMOKE_CONSOLE:-127.0.0.1:8081}"
ENV_FILE="${WIKTOR_SMOKE_ENV:-/etc/wiktor/wiktor.env}"
fail=0

check() { # check <label> <exit-ok? 0/1> <output>
  if [ "$2" -eq 0 ]; then echo "PASS  $1"; else echo "FAIL  $1 :: $3"; fail=1; fi
}

# 1) systemd 活性。
# 1) systemd liveness.
for unit in wiktor-server wiktor-console; do
  systemctl is-active --quiet "${unit}.service"
  check "systemd active: ${unit}" $? "$(systemctl is-active "${unit}.service" 2>&1)"
done

# 2) /health。
# 2) /health.
out=$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "http://${HTTP}/health" 2>&1)
check "GET /health == 200" "$([ "$out" = "200" ]; echo $?)" "$out"

# 3) 无 key 必须 401；有 key 必须 200（HTTP GET /search）。
# 3) No key must 401; a valid key must 200 (HTTP GET /search).
code=$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "http://${HTTP}/search?domain=${DOMAIN}&q=smoke" 2>&1)
check "GET /search without key == 401" "$([ "$code" = "401" ]; echo $?)" "$code"

secret=$(sed -n 's/^WIKTOR_API_KEYS=.*"secret":"\([^"]*\)".*/\1/p' "${ENV_FILE}" | head -1)
[ -n "${secret}" ] || { echo "FAIL  cannot read secret from ${ENV_FILE}"; exit 1; }
code=$(curl -sS -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${secret}" \
  "http://${HTTP}/search?domain=${DOMAIN}&q=smoke" 2>&1)
check "GET /search with key == 200" "$([ "$code" = "200" ]; echo $?)" "$code"

# 4) console。
# 4) The console.
out=$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "http://${CONSOLE}/" 2>&1)
check "GET console / == 200" "$([ "$out" = "200" ]; echo $?)" "$out"
out=$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "http://${CONSOLE}/api/overview" 2>&1)
check "GET console /api/overview == 200" "$([ "$out" = "200" ]; echo $?)" "$out"

[ "$fail" -eq 0 ] && echo "SMOKE PASS" || echo "SMOKE FAIL"
exit "$fail"
