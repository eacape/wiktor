#!/usr/bin/env bash
# Wiktor Step13 B5：生产反馈闭环端到端（spec step13 §4 B5/A4 + STEP13-002 增强）。
# 本机执行：HTTP search（log_id）→ POST /feedback（rate 低分）→
# `wiktor feedback analyze --domain-pack`（zero_recall 盲点 + 确定性实体增强，
# STEP13-002 回填五字段 subject）→ `review approve`（supplemental_compile 排队
# 编译）→ 等 worker 编译发布 → 复检该查询命中提升。
# Wiktor Step13 B5: the production feedback-loop end-to-end (spec step13 §4
# B5/A4 + the STEP13-002 enrichment). Run on the box: HTTP search (log_id) →
# POST /feedback (a low rate event) → `wiktor feedback analyze --domain-pack`
# (the zero-recall blind spot + deterministic entity enrichment, STEP13-002
# backfills the five-field subject) → `review approve` (supplemental_compile
# queues a compile task) → wait for the worker to compile and publish →
# re-check the query's hits.
set -uo pipefail

DB="${WIKTOR_SMOKE_DB:-/srv/wiktor/data/wiktor.db}"
ENV_FILE="${WIKTOR_SMOKE_ENV:-/etc/wiktor/wiktor.env}"
DOMAIN="${WIKTOR_SMOKE_DOMAIN:-tech-docs}"
PACK="${WIKTOR_SMOKE_PACK:-/srv/wiktor/examples/tech-docs/domain.yaml}"
HTTP="${WIKTOR_SMOKE_HTTP:-127.0.0.1:8080}"
# 查询 = 域包内某个未编译 document 实体的标题（seed-only 库零命中 → 反馈盲点；
# 增强按归一化标题全等匹配该实体 → approve 补编译后命中提升）。
# The query = the title of an uncompiled document entity in the pack (zero hits
# on a seed-only DB → the feedback blind spot; enrichment matches that entity
# by normalized-title equality → hits improve after the supplemental compile).
QUERY="${WIKTOR_SMOKE_QUERY:-gRPC 快速上手（concept）}"
WIKTOR="${WIKTOR_SMOKE_BIN:-/usr/local/bin/wiktor}"
KEY=$(sed -n 's/^WIKTOR_API_KEYS=.*"secret":"\([^"]*\)".*/\1/p' "${ENV_FILE}" | head -1)

log() { echo "[$(date +%H:%M:%S)] $*"; }

# 0) 基线：先查该标题（seed-only 库零命中 → 盲点来源）。
# 0) Baseline: query the title first (zero hits on a seed-only DB → the
#    blind-spot source).
resp=$(curl -sS -m 8 -G -H "Authorization: Bearer ${KEY}" \
  --data-urlencode "domain=${DOMAIN}" --data-urlencode "q=${QUERY}" \
  --data-urlencode "top_k=5" "http://${HTTP}/search")
log_id=$(echo "${resp}" | python3 -c "import json,sys; print(json.load(sys.stdin)['log_id'])" 2>/dev/null || echo "")
hits0=$(echo "${resp}" | python3 -c "import json,sys; print(len(json.load(sys.stdin)['hits']))" 2>/dev/null || echo 0)
log "baseline query='${QUERY}' log_id=${log_id} hits=${hits0}"
[ -n "${log_id}" ] && [ "${log_id}" -gt 0 ] || { echo "FAIL baseline search has no log_id"; exit 1; }
[ "${hits0}" -eq 0 ] || log "WARN baseline hits=${hits0} (expected 0; re-check still requires improvement)"

# 1) POST /feedback：对 log_id 提交 rate=2（低分）+ metadata 记录查询，触发分析信号。
# 1) POST /feedback: a rate=2 (low score) event against log_id + a metadata
#    note, to feed the analyzer signal.
ikey="smoke-${DOMAIN}-$(date +%s)"
status=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -X POST \
  -H "Authorization: Bearer ${KEY}" -H 'Content-Type: application/json' \
  -d "{\"domain\":\"${DOMAIN}\",\"events\":[{\"idempotency_key\":\"${ikey}\",\"log_id\":${log_id},\"kind\":\"rate\",\"rating\":2,\"metadata\":{\"query\":\"${QUERY}\"}}]}" \
  "http://${HTTP}/feedback")
[ "${status}" = "200" ] || { echo "FAIL POST /feedback == ${status}"; exit 1; }
log "posted feedback (rate=2) → ${status}"

# 2) analyze：零召回盲点应浮现，且（STEP13-002）按领域包标题匹配回填实体 subject。
# 2) analyze: the zero-recall blind spot should surface, and (STEP13-002) the
#    subject should be backfilled with the title-matched entity from the pack.
now=$(date +%s)
from=$((now - 3600))
analyze_out=$("${WIKTOR}" feedback analyze --db "${DB}" --domain "${DOMAIN}" \
  --from "${from}" --to "${now}" --domain-pack "${PACK}" 2>&1)
log "analyze: $(echo "${analyze_out}" | tail -3 | tr '\n' ' ')"
review_id=$("${WIKTOR}" feedback list --db "${DB}" --domain "${DOMAIN}" --json 2>/dev/null \
  | python3 -c "
import json,sys
try:
    d = json.load(sys.stdin)
    items = d if isinstance(d, list) else d.get('reviews', d.get('items', []))
    pending = [r for r in items
               if str(r.get('status','')).lower()=='pending'
               and r.get('action')=='supplemental_compile'
               and '\"entity_id\"' in str(r.get('subject_json',''))]
    print(pending[0]['review_id'] if pending else '')
except Exception: print('')
")
[ -n "${review_id}" ] || { echo "FAIL no pending enriched supplemental_compile review (blind spot not surfaced/enriched)"; exit 1; }
log "pending enriched review id=${review_id}"

# 3) approve：排队补编译任务（subject 已含五必需字段 → admit 校验通过）。
# 3) approve: queue the supplemental compile task (the subject now carries the
#    five required fields → the admit validation passes).
"${WIKTOR}" feedback review approve --db "${DB}" --review-id "${review_id}" --by smoke >/dev/null 2>&1 \
  || { echo "FAIL approve review ${review_id}"; exit 1; }
log "approved review ${review_id}"

# 4) 等 worker 编译发布（轮询复检命中）。
# 4) Wait for the worker to compile and publish (poll the re-check hits).
for i in $(seq 1 40); do
  sleep 3
  hits=$(curl -sS -m 8 -G -H "Authorization: Bearer ${KEY}" \
    --data-urlencode "domain=${DOMAIN}" --data-urlencode "q=${QUERY}" \
    --data-urlencode "top_k=5" "http://${HTTP}/search" \
    | python3 -c "import json,sys; print(len(json.load(sys.stdin)['hits']))" 2>/dev/null || echo 0)
  if [ "${hits}" -gt "${hits0}" ]; then log "re-check hits=${hits} (> baseline ${hits0}) — loop closed"; exit 0; fi
done
echo "FAIL compile worker did not close the gap (hits stayed ${hits})"
exit 1
