#!/usr/bin/env bash
# Wiktor Step9 — 隔离恢复演练（spec step9 §6，A7/A9/A10，D11）。
# Wiktor Step9 — isolated restore drill (spec step9 §6, A7/A9/A10, D11).
#
# 在专用 disposable 测试库上模拟灾难：停 replicate → 移走/删除测试库 →
# litestream restore 到隔离路径 → integrity/schema/foreign-key/row-count/
# marker 校验 → PASS。绝不触碰正式 DB（A9：目标为活动库时直接拒绝）。
# Simulates a disaster on a dedicated disposable test DB: stop replicate →
# move/delete the test DB → litestream restore to an isolated path →
# integrity/schema/foreign-key/row-count/marker checks → PASS. Never touches
# the production DB (A9: refuses when the target is an active DB).
set -euo pipefail

LITESTREAM_BIN="${LITESTREAM_BIN:-/usr/local/bin/litestream}"
SQLITE3="${SQLITE3:-sqlite3}"
CONFIRM_TEST_DB="${CONFIRM_TEST_DB:-0}"

# ---- 参数 ----
DB=""
RESTORE_TO=""
CONFIG="${LITESTREAM_CONFIG:-}"
MARKER="wiktor-step9-drill-$(date +%s)"
FAILED=0

usage() { echo "usage: $0 --db <path> --restore-to <path> [--config <litestream.yml>] [--confirm-test-db]" >&2; exit 2; }
fail() { echo "FAIL  $*" >&2; FAILED=1; }
pass() { echo "ok    $*"; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --db) DB="$2"; shift 2 ;;
        --restore-to) RESTORE_TO="$2"; shift 2 ;;
        --config) CONFIG="$2"; shift 2 ;;
        --confirm-test-db) CONFIRM_TEST_DB=1; shift ;;
        *) echo "unknown arg: $1" >&2; usage ;;
    esac
done
[ -n "$DB" ] && [ -n "$RESTORE_TO" ] || usage

# ---- A9：目标必须是测试库 ----
if [ "$CONFIRM_TEST_DB" -ne 1 ]; then
    echo "拒绝执行：必须显式 --confirm-test-db（保护生产库，spec A9）。" >&2
    exit 3
fi
case "$DB" in
    /srv/wiktor/*|/var/lib/wiktor/*) echo "拒绝执行：$DB 可能是生产路径。" >&2; exit 3 ;;
    /|/*/..*|"") echo "拒绝执行：危险路径 $DB。" >&2; exit 3 ;;
    *) : ;;
esac
# restore 目标也不得覆盖输入
[ "$DB" != "$RESTORE_TO" ] || { echo "拒绝执行：restore 目标与源相同。" >&2; exit 3; }

restore_to_dir="$(dirname "$RESTORE_TO")"
mkdir -p "$restore_to_dir"

# ---- 1. 目录：源库 + replica（file:// 或由 config 指定）----
db_dir="$(dirname "$DB")"
replica="file://$db_dir/replica"
echo "[drill] 测试库：$DB"
echo "[drill] 临时 replicate 目录：$replica"

# ---- 2. 建立一个 WAL 数据库并插入 marker + 对照行 ----
"$SQLITE3" "$DB" "PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS drill_rows (id INTEGER PRIMARY KEY, tag TEXT);"
"$SQLITE3" "$DB" "INSERT INTO drill_rows (tag) VALUES ('$MARKER'), ('baseline-1');"
echo "marker = $MARKER"
rows_before="$( "$SQLITE3" "$DB" 'SELECT COUNT(*) FROM drill_rows;' )"
echo "rows_before = $rows_before"

# ---- 3. 配置（就地渲染：源 + file replica）----
# schema 已对照 v0.5.17 实测（STEP9-013）：sync-interval 是 db 级键；生成/快照
# 检测用 `litestream ltx`（v0.5.17 无 `generations` 子命令，STEP9-014）。
rendered_config="$(mktemp)"
cat > "$rendered_config" <<YAML
dbs:
  - path: $DB
    sync-interval: 100ms
    replicas:
      - url: $replica
        snapshot-interval: 1m
YAML
if [ -n "$CONFIG" ]; then
    echo "[drill] 使用外部 config：$CONFIG"
    rendered_config="$CONFIG"
fi
echo "[drill] config = $rendered_config"

# ---- 4. 启动 litestream replicate（后台，等待首快照）----
"$LITESTREAM_BIN" replicate -config "$rendered_config" &
LS_PID=$!
trap '[ -n "${LS_PID:-}" ] && kill "$LS_PID" 2>/dev/null || true' EXIT

# 等待首快照（replica 出现 LTX 段，最多 30s）。v0.5.17 用 `ltx` 替代旧
# `generations`（STEP9-014）。
# Wait for the first snapshot (replica holds an LTX segment, up to 30s). v0.5.17
# uses `ltx` in place of the old `generations` command (STEP9-014).
for _ in $(seq 1 30); do
    if "$LITESTREAM_BIN" ltx -config "$rendered_config" "$DB" 2>/dev/null | grep -q .; then
        break
    fi
    sleep 1
done

# 再写入一条，确保 WAL 有增量且已同步。
"$SQLITE3" "$DB" "INSERT INTO drill_rows (tag) VALUES ('post-snapshot-$MARKER');"
sleep 2

# ---- 5. 停 replicate、记录 manifest、模拟灾难（移走测试库）----
kill "$LS_PID" 2>/dev/null || true
wait "$LS_PID" 2>/dev/null || true
LS_PID=""
echo "[drill] replicate 已停止"

manifest="$(mktemp)"
{
    echo "marker=$MARKER"
    echo "rows_before=$rows_before"
    echo "expect_rows=$((rows_before + 1))"
    echo "expect_marker=$MARKER"
    echo "expect_post=post-snapshot-$MARKER"
} > "$manifest"
echo "[drill] manifest = $manifest"
cat "$manifest"

"$SQLITE3" "$DB" "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null 2>&1 || true
rm -f "$DB" "$DB-wal" "$DB-shm"
echo "[drill] 模拟灾难：测试库已删除"

# ---- 6. 隔离 restore ----
rm -f "$RESTORE_TO"
if "$LITESTREAM_BIN" restore -config "$rendered_config" -o "$RESTORE_TO" "$DB" >/dev/null 2>&1; then
    pass "litestream restore 成功 → $RESTORE_TO"
else
    fail "litestream restore 失败（详见上方错误）"
fi

# ---- 7. 完整性 / schema / 外键 / row-count / marker ----
if [ "$FAILED" -eq 0 ]; then
    integrity="$("$SQLITE3" "$RESTORE_TO" 'PRAGMA integrity_check;' 2>/dev/null | tr -d '[:space:]')"
    [ "$integrity" = "ok" ] && pass "integrity_check=ok" || fail "integrity_check=$integrity"

    fk="$("$SQLITE3" "$RESTORE_TO" 'PRAGMA foreign_key_check;' 2>/dev/null | wc -l | tr -d ' ')"
    [ "$fk" -eq 0 ] && pass "foreign_key_violations=0" || fail "foreign_key_violations=$fk"
fi

if [ "$FAILED" -eq 0 ]; then
    rows_after="$( "$SQLITE3" "$RESTORE_TO" 'SELECT COUNT(*) FROM drill_rows;' )"
    expect_rows="$(grep '^expect_rows=' "$manifest" | cut -d= -f2)"
    [ "$rows_after" = "$expect_rows" ] \
        && pass "row_count=$rows_after == 期望 $expect_rows" \
        || fail "row_count=$rows_after != 期望 $expect_rows"

    marker_found="$( "$SQLITE3" "$RESTORE_TO" "SELECT COUNT(*) FROM drill_rows WHERE tag='$MARKER';" )"
    [ "$marker_found" -ge 1 ] && pass "marker 命中" || fail "marker 丢失"

    post_found="$( "$SQLITE3" "$RESTORE_TO" "SELECT COUNT(*) FROM drill_rows WHERE tag='post-snapshot-$MARKER';" )"
    [ "$post_found" -ge 1 ] && pass "post-snapshot 增量命中" || fail "post-snapshot 增量丢失"
fi

# ---- 8. 清理 ----
rm -f "$rendered_config" "$manifest"
rm -rf "$db_dir/replica"
rm -f "$RESTORE_TO" "$RESTORE_TO-wal" "$RESTORE_TO-shm"

if [ "$FAILED" -eq 1 ]; then
    echo "=== RESTORE DRILL FAILED ===" >&2
    exit 1
fi
echo "=== RESTORE DRILL PASS ==="
exit 0