#!/usr/bin/env bash
# Wiktor Step9 — litestream 安装脚本（spec step9 §4.2 A1）。
# Wiktor Step9 — litestream install script (spec step9 §4.2 A1).
#
# 固定版本 0.5.17：下载对应架构静态二进制 + checksums.txt，SHA-256 校验后
# 安装到 /usr/local/bin/litestream。默认 dry-run 打印动作；--apply 才落盘。
# 若 DOWNLOAD_DIR 已存在同资产且校验通过，则跳过下载（支持离线/镜像复用）。
# Pins 0.5.17: downloads the arch-matching static binary + checksums.txt,
# verifies SHA-256, then installs to /usr/local/bin/litestream. Prints actions
# by default; only --apply writes to the system. If the same asset already
# exists in DOWNLOAD_DIR and passes checksum, the download is skipped (offline /
# mirror reuse).
set -euo pipefail

# 版本号不含 "v" 前缀（GitHub release 资产名如此，STEP9-012）；tag URL 用 v 前缀。
# Version has no "v" prefix (that's the release asset name, STEP9-012); the tag
# URL keeps the "v".
LITESTREAM_VERSION="${LITESTREAM_VERSION:-0.5.17}"
TAG="v${LITESTREAM_VERSION}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
DOWNLOAD_DIR="${DOWNLOAD_DIR:-/tmp/litestream-install}"
APPLY="${APPLY:-0}"

# 解析参数（无 getopt 依赖，保持纯 bash）。
# Parse args (no getopt dependency, plain bash).
while [ "$#" -gt 0 ]; do
    case "$1" in
        --apply) APPLY=1 ;;
        --version) LITESTREAM_VERSION="$2"; shift ;;
        --install-dir) INSTALL_DIR="$2"; shift ;;
        --download-dir) DOWNLOAD_DIR="$2"; shift ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

# 架构 → 资产名映射（0.5.17；linux-x86_64 / darwin-arm64，资产名不带 v，STEP9-012）。
# arch → asset-name mapping (0.5.17; linux-x86_64 / darwin-arm64, no "v" in the
# asset name, STEP9-012).
arch="$(uname -s)-$(uname -m)"
case "$arch" in
    Linux-x86_64) OSARCH="linux-x86_64" ;;
    Darwin-arm64) OSARCH="darwin-arm64" ;;
    *)
        echo "unsupported platform: $arch" >&2
        exit 2
        ;;
esac
ASSET="litestream-${LITESTREAM_VERSION}-${OSARCH}.tar.gz"

BASE_URL="https://github.com/benbjohnson/litestream/releases/download/${TAG}"

# 可移植 SHA-256（Linux 用 sha256sum，macOS 用 shasum -a 256）。
# Portable SHA-256 (sha256sum on Linux, shasum -a 256 on macOS).
if command -v sha256sum >/dev/null 2>&1; then SHA256="sha256sum"; else SHA256="shasum -a 256"; fi

run() {
    echo "[install] $*"
    if [ "$APPLY" -ne 1 ]; then
        echo "[dry-run]  未执行 --apply"
        return 0
    fi
    "$@"
}

mkdir -p "$DOWNLOAD_DIR" "$INSTALL_DIR"

# 拉取 checksums.txt（含 SHA-256 校验清单）。已存在且含本资产条目则复用，避免
# 慢网反复拉取；缺条目/缺失才重新下载。
# Fetch checksums.txt (holds the SHA-256 manifest). Reuse an existing copy that
# already lists this asset; only re-fetch when missing or missing the entry.
if [ -f "$DOWNLOAD_DIR/checksums.txt" ]; then
    expected="$(awk -v a="$ASSET" '$2==a {print $1}' "$DOWNLOAD_DIR/checksums.txt")"
fi
if [ -z "${expected:-}" ]; then
    curl -fsSL -o "$DOWNLOAD_DIR/checksums.txt" "$BASE_URL/checksums.txt"
    expected="$(awk -v a="$ASSET" '$2==a {print $1}' "$DOWNLOAD_DIR/checksums.txt")"
fi
if [ -z "$expected" ]; then
    echo "checksums.txt 缺 $ASSET 条目" >&2
    exit 3
fi

# 若已存在同资产且校验通过，跳过下载（离线/镜像复用，STEP9-012；需要离线走
# 代理更快时先 HTTPS_PROXY=... 再跑本脚本）。
# If the same asset already exists and verifies, skip the download (offline /
# mirror reuse; to use a proxy run with HTTPS_PROXY=... first).
if [ -f "$DOWNLOAD_DIR/$ASSET" ]; then
    actual="$("$SHA256" "$DOWNLOAD_DIR/$ASSET" | awk '{print $1}')"
    if [ "$actual" = "$expected" ]; then
        echo "[install] 复用已下载且校验通过：$DOWNLOAD_DIR/$ASSET"
    else
        echo "[install] 已有文件校验不匹配，重新下载 $ASSET"
        curl -fsSL -o "$DOWNLOAD_DIR/$ASSET" "$BASE_URL/$ASSET"
    fi
else
    echo "[install] 拉取 $ASSET from $BASE_URL"
    curl -fsSL -o "$DOWNLOAD_DIR/$ASSET" "$BASE_URL/$ASSET"
fi

# SHA-256 校验：缺失或不匹配 → 直接失败，绝不安装未验证二进制（A1）。
# SHA-256 verify: missing or mismatched → fail, never install unverified
# binaries (A1).
actual="$("$SHA256" "$DOWNLOAD_DIR/$ASSET" | awk '{print $1}')"
if [ "$actual" != "$expected" ]; then
    echo "SHA-256 不匹配：expected=$expected actual=$actual" >&2
    exit 3
fi
echo "[install] SHA-256 ok: ${actual:0:16}…"

run tar -xzf "$DOWNLOAD_DIR/$ASSET" -C "$DOWNLOAD_DIR" litestream
run install -o root -g root -m 0755 "$DOWNLOAD_DIR/litestream" "$INSTALL_DIR/litestream"

echo "[install] 版本："
"$INSTALL_DIR/litestream" version
