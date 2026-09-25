#!/usr/bin/env bash
# Wiktor Step12 生产安装脚本（spec step12 §4 B3，D5）：服务器上 release 构建 →
# 安装二进制/单元 → 写 /etc/wiktor/wiktor.env → enable --now。幂等：重复执行
# 只重建产物，不覆盖已有 env。
# Wiktor Step12 production install script (spec step12 §4 B3, D5): release build
# on the server → install the binary/units → write /etc/wiktor/wiktor.env →
# enable --now. Idempotent: re-runs rebuild artifacts but never overwrite an
# existing env file.
set -euo pipefail

REPO_DIR="${WIKTOR_REPO_DIR:-/srv/wiktor}"
DATA_DIR="/srv/wiktor/data"
ENV_DIR="/etc/wiktor"
ENV_FILE="${ENV_DIR}/wiktor.env"
BIN=/usr/local/bin/wiktor
BUILD_JOBS="${WIKTOR_BUILD_JOBS:-2}"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 2; }
[ -d "${REPO_DIR}" ] || { echo "repo not found: ${REPO_DIR}" >&2; exit 2; }
command -v cargo >/dev/null || {
  echo "cargo not found; install a Rust toolchain first (see deploy/README.md)" >&2
  exit 2
}

# 1) 运行账号（litestream.service 同样假设 wiktor 用户）。
# 1) The service account (litestream.service assumes the wiktor user too).
id wiktor >/dev/null 2>&1 || useradd --system --home /srv/wiktor --shell /usr/sbin/nologin wiktor
mkdir -p "${DATA_DIR}" "${ENV_DIR}"
chown wiktor:wiktor "${DATA_DIR}"

# 2) env 文件：存在则保留（幂等），缺失则从模板生成。
# 2) The env file: keep it if present (idempotent); generate from the template
# when missing.
if [ ! -f "${ENV_FILE}" ]; then
  install -Dm640 -o root -g wiktor deploy/wiktor.env.example "${ENV_FILE}"
  echo "env generated at ${ENV_FILE} — edit WIKTOR_API_KEYS before going live"
fi

# 3) release 构建（2G 内存机器建议 -j2；失败可 WIKTOR_BUILD_JOBS=1 重试）。
# 3) The release build (-j2 suggested on a 2G-memory box; retry with
# WIKTOR_BUILD_JOBS=1 on failure).
cd "${REPO_DIR}"
cargo build --release --locked -p wiktor-cli --features server,console -j "${BUILD_JOBS}"

# 4) 安装二进制与单元并启动。
# 4) Install the binary and units, then start.
install -Dm755 target/release/wiktor "${BIN}"
install -Dm644 deploy/wiktor-server.service /etc/systemd/system/wiktor-server.service
install -Dm644 deploy/wiktor-console.service /etc/systemd/system/wiktor-console.service
systemctl daemon-reload
systemctl enable --now wiktor-server.service wiktor-console.service

systemctl --no-pager --lines 0 status wiktor-server.service wiktor-console.service || true
echo "install complete: ${BIN} (server :8080/:50051, console :8081, all loopback)"
