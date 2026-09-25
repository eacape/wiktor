# Wiktor Step9 — litestream 备份部署与运维

> 依据 `docs/design/step9-backup-ha.md`（英文 `step9-backup-ha.en.md`）。本目录是
> 落地脚本；/etc 实际配置由 Linux 执行时渲染，**secret 不进 Git/tar 包**。
> Per `docs/design/step9-backup-ha.md` (EN: `step9-backup-ha.en.md`). This
> directory holds the deployment scripts; /etc is rendered on Linux at deploy
> time — **secrets never enter Git/tar**.
>
> **已实测（2026-09-24，Linux Debian 13 x86_64）**：`install-litestream.sh`
> 安装 0.5.17、`backup-preflight.sh` PASS、`restore-drill.sh` 灾难模拟恢复
> PASS（A1/A2/A7/A8/A9）。偏差见 step9-backup-ha.md §7（STEP9-011..015）。
> **Verified (2026-09-24, Linux Debian 13 x86_64)**: installer installs 0.5.17,
> preflight PASS, recovery drill PASS (A1/A2/A7/A8/A9). Deviations in
> step9-backup-ha.md §7 (STEP9-011..015).

## 文件 / Files

| 文件 | 作用 |
|---|---|
| `install-litestream.sh` | 下载固定 v0.5.17 + SHA-256 校验 + 安装（`--apply` 落盘，默认 dry-run） |
| `litestream.yml.example` | `file://` 可运行样例（演练/短期副本；同盘不抵御整盘损坏） |
| `litestream.sftp.example.yml` | 生产首选：Linux→Mac SFTP 模板（凭据经 root 环境文件注入） |
| `litestream.s3.example.yml` | S3 / 阿里 OSS 模板（默认禁用；需用户 bucket + 显式验证） |
| `litestream.service` | systemd unit（常驻 replicate，on-failure 重启，沙箱加固） |
| `backup-preflight.sh` | 前置检查：DB 存在 + WAL、目标权限、磁盘水位、路径安全（只读） |
| `restore-drill.sh` | 隔离恢复演练：disposable DB 灾难模拟 → restore → 完整校验 → PASS |

## 快速开始 / Quick start（Linux）

```sh
# 1. 安装 litestream（默认 dry-run 打印动作；加 --apply 实际落盘）
sudo ./deploy/install-litestream.sh --apply

# 2. 前置检查（只读；按需覆盖 DB/REPLICA_URL）
sudo DB=/srv/wiktor/data/wiktor.db ./deploy/backup-preflight.sh

# 3. 选择备份目标并渲染 /etc/litestream.yml
#    （root 建 /etc/wiktor/litestream.env + 0600 才放 SFTP 密码；file:// 无需 secret）
sudo install -d -o root -g root -m 0755 /etc/wiktor
sudo install -m 0644 deploy/litestream.yml.example /etc/litestream.yml
#    用 SFTP 时：安装 deploy/litestream.sftp.example.yml 并填真实 host/path/user

# 4. 安装 systemd unit 并启动
sudo install -m 0644 deploy/litestream.service /etc/systemd/system/litestream.service
sudo systemctl daemon-reload
sudo systemctl enable --now litestream
systemctl is-enabled litestream && systemctl is-active litestream

# 5. 恢复演练（必须显式 --confirm-test-db；用 disposable 库，不碰生产）
#    —— 等价 macOS：本机装 darwin-arm64 版后同一脚本跑 file:// ——
sudo ./deploy/restore-drill.sh \
  --db /tmp/wiktor-ha-test/wiktor.db \
  --restore-to /tmp/wiktor-ha-test/restored.db \
  --confirm-test-db
```

## 运维检查 / Ops（Linux）

```sh
systemctl status litestream          # 进程状态
journalctl -u litestream -n 50       # replicate 日志
litestream status -config /etc/litestream.yml /srv/wiktor/data/wiktor.db
litestream ltx -config /etc/litestream.yml /srv/wiktor/data/wiktor.db
```
> v0.5.17 没有 `litestream generations`（STEP9-014）：看同步状态用 `status`，
> 看副本里已有的 LTX/WAL 段用 `ltx`（旧 `generations` 命令在 v0.5.17 已移除）。

## 正式灾难恢复流程 / Formal disaster recovery

1. `systemctl stop wiktor-server litestream`（确认无打开的 DB 连接）
2. 恢复到隔离路径（**不直接覆盖主库**）：
   `litestream restore -config /etc/litestream.yml -o /srv/wiktor/restore/wiktor.db /srv/wiktor/data/wiktor.db`
3. 校验恢复库：`PRAGMA integrity_check`=ok、关键表存在、`wiktor status --db <恢复库>`
4. 人工确认后，维护窗口内原子替换主库路径并重启 server → litestream

## Wiktor 服务部署（Step12）/ Wiktor service deployment (Step12)

> 依据 `docs/design/step12-tui-console-prod.md` §4 B3（EN `step12-tui-console-prod.en.md`）。
> Per `docs/design/step12-tui-console-prod.md` §4 B3 (EN:
> `step12-tui-console-prod.en.md`).

### 新增文件 / New files

| 文件 | 作用 |
|---|---|
| `install-wiktor.sh` | 服务器 release 构建（`-j2`）→ 安装 `/usr/local/bin/wiktor` + 两个 systemd 单元 → 生成 env（幂等，不覆盖已有）→ `enable --now` |
| `wiktor-server.service` | `wiktor serve`（HTTP 8080 + gRPC 50051，沙箱与 litestream.service 对齐，数据 `/srv/wiktor/data`） |
| `wiktor-console.service` | `wiktor console`（只读监督面，仅绑 `127.0.0.1:8081`） |
| `wiktor.env.example` | `/etc/wiktor/wiktor.env` 模板：`WIKTOR_API_KEYS`（方法级 key）+ `WIKTOR_COMPILE_WORKERS` |
| `smoke-deploy.sh` | 部署冒烟：systemd 活性 / `/health` 200 / 无 key 401、有 key 200 / console 200（全 PASS → `SMOKE PASS`） |

### Bring-up runbook（全新机器 / fresh box）

```sh
# 0. Rust 工具链（服务器构建需要；Debian 13 建议 rustup + 国内镜像，
#    见 wiktor-project 记忆的 rsproxy/ tuna 配置）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable

# 1. 上传本仓库到 /srv/wiktor（代码纪律：本机 commit → Linux apply → push）
# 2. （2G 内存机器）加 swap 再构建，避免 release OOM：
fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile

# 3. 安装（构建 + 装 unit + 起服务；首次会生成 /etc/wiktor/wiktor.env）
sudo ./deploy/install-wiktor.sh

# 4. 编辑 /etc/wiktor/wiktor.env：把 WIKTOR_API_KEYS 的 secret 换成强随机值，
#    然后 systemctl restart wiktor-server
# 5. 冒烟（本机 loopback；全部 PASS 才算部署完成）
sudo ./deploy/smoke-deploy.sh

# 6. 远程访问 console（只走 SSH 隧道，不暴露公网）：
ssh -L 8081:127.0.0.1:8081 root@<host>   # 本机浏览器打开 http://127.0.0.1:8081
#    公网暴露 HTTP/gRPC = 改单元监听为 0.0.0.0 + 云防火墙放行 + 强 key
```

### 升级 / Upgrade

```sh
cd /srv/wiktor && git pull   # 或 apply 新 patch
sudo ./deploy/install-wiktor.sh   # 重建 + 重装 + 重启（幂等）
sudo ./deploy/smoke-deploy.sh
```

### 与 litestream 的顺序 / Ordering with litestream

- 先 `backup-preflight.sh` → `restore-drill.sh` 验证备份链，再长期起服务；灾难恢复流程见上文（先停 `wiktor-server litestream` 再 restore）。
- Validate the backup chain (`backup-preflight.sh` → `restore-drill.sh`) before
  running services long-term; disaster recovery stops `wiktor-server litestream`
  first (see above).

## 边界 / 非目标

- 不做 Raft / 自动 failover / 跨机热备：只保证单写库持续复制 + 可恢复。
- 不备份 qdrant：从 SQLite 知识/事实平面按 generation/content-hash 重建。
- 统一路径纪律：`server --db`、litestream YAML、restore 必须用同一绝对路径。
- RPO≈0 是 1s `sync-interval` 的健康网络目标，不是 SLA；实际 RPO 以最后成功
  generation 时间为准。