#!/usr/bin/env bash
# Wiktor Step9 — 备份前置检查（spec step9 §4.2 A2–A4，A7）。
# Wiktor Step9 — backup preflight (spec step9 §4.2 A2–A4, A7).
#
# 校验数据库存在 + WAL 模式、目标权限、磁盘水位、litestream 可用、数据库/
# 副本目录不越界。默认只打印判定并返回退出码；不做任何写操作。
# Validates DB existence + WAL mode, target writability, disk headroom,
# litestream availability, and that the DB/replica dirs are not dangerous.
# Prints verdicts and returns an exit code by default; never writes anything.
set -euo pipefail

DB="${DB:-/srv/wiktor/data/wiktor.db}"
REPLICA_URL="${REPLICA_URL:-file:///srv/backup/wiktor/litestream}"
LITESTREAM_BIN="${LITESTREAM_BIN:-/usr/local/bin/litestream}"
MIN_FREE_PERCENT="${MIN_FREE_PERCENT:-20}"
SQLITE3="${SQLITE3:-sqlite3}"

# 解析 --db / --replica 参数（覆盖 env 默认，spec §6.2 调用形态）。
# Parse --db / --replica args (override env defaults; the §6.2 invocation shape).
while [ "$#" -gt 0 ]; do
    case "$1" in
        --db) DB="$2"; shift 2 ;;
        --replica) REPLICA_URL="$2"; shift 2 ;;
        --litestream-bin) LITESTREAM_BIN="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

fail() { echo "FAIL  $*" >&2; FAILED=1; }
pass() { echo "ok    $*"; }

# ---- 1. litestream 可用 ----
if [ ! -x "$LITESTREAM_BIN" ]; then
    fail "litestream 不在 $LITESTREAM_BIN（先跑 deploy/install-litestream.sh --apply）"
else
    v="$("$LITESTREAM_BIN" version 2>&1)"
    pass "litestream: $v"
fi

# ---- 2. 数据库存在 + WAL 模式 ----
if [ ! -f "$DB" ]; then
    fail "数据库不存在：$DB"
else
    pass "数据库存在：$DB"
    journal_mode="$("$SQLITE3" "$DB" 'PRAGMA journal_mode;' 2>/dev/null | tr -d '[:space:]')"
    if [ "$journal_mode" != "wal" ]; then
        fail "journal_mode=$journal_mode（需 wal；Litestream 复制要求 WAL）"
    else
        pass "journal_mode=wal"
    fi
fi

# ---- 3. 数据库目录可读 + 副本目标可写 ----
db_dir="$(dirname "$DB")"
if [ ! -d "$db_dir" ]; then
    fail "数据库目录不存在：$db_dir"
fi
case "$REPLICA_URL" in
    file://*)
        replica_path="${REPLICA_URL#file://}"
        if [ ! -d "$replica_path" ]; then
            fail "file replica 目录不存在：$replica_path"
        elif [ ! -w "$replica_path" ]; then
            fail "file replica 目录不可写：$replica_path"
        else
            pass "file replica 可写：$replica_path"
        fi
        ;;
    sftp://*) pass "SFTP replica 目标（可达性/凭据由 litestream 前台启动时验证）: $REPLICA_URL" ;;
    s3://*|oss://*) pass "云对象存储 replica（需用户显式验证 endpoint/bucket/凭据）: ${REPLICA_URL%%/*}" ;;
    *) fail "未知 replica scheme：$REPLICA_URL" ;;
esac

# ---- 4. 磁盘水位（数据库所在卷）----
db_vol="$(df -P "$db_dir" 2>/dev/null | awk 'NR==2 {print $5}' | tr -d '%' || echo 100)"
if [ -n "$db_vol" ] && [ "$db_vol" -ge "$((100 - MIN_FREE_PERCENT))" ]; then
    fail "磁盘可用不足：$(df -h "$db_dir" 2>/dev/null | awk 'NR==2 {print $4}')（阈值 ≥${MIN_FREE_PERCENT}% 空闲）"
else
    pass "磁盘空闲 >${MIN_FREE_PERCENT}%（当前 $(df -h "$db_dir" 2>/dev/null | awk 'NR==2 {print $4}')）"
fi

# ---- 5. 路径安全（防误伤）----
for p in "$DB" "$db_dir"; do
    case "$p" in
        /|/*/..*|"") fail "危险路径：$p" ;;
        *) : ;;
    esac
done

if [ "${FAILED:-0}" -eq 1 ]; then
    echo "=== preflight FAILED ===" >&2
    exit 1
fi
echo "=== preflight PASS ==="
exit 0