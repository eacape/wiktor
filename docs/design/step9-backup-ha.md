# Step 9 设计规范：litestream 持续备份与单机灾备

> 版本：v1.0（2026-09-23）  
> 权威依据：`docs/MASTER-PLAN.md` v3.2 §5.5、§10、§17；部署基线：Step 7 server、Step 8 状态机  
> 实现对象：`wiktor-builder`；独立验收对象：`test-engineer`  
> 中文为权威设计；英文逐节对应于 `step9-backup-ha.en.md`。

## 1. 目标与非目标

本 Step 把 SQLite WAL 主库纳入独立部署的 litestream 持续复制，并建立可执行恢复演练。生产形态是 Debian 13 x86_64 单机服务 + 远端或本地副本，不是多节点高可用集群。单一 `wiktor.db` 是运行时事实来源；qdrant 向量数据是派生索引，可重建，不纳入运行时备份完整性判据。

目标：部署可重复、备份目标可配置、服务与复制器生命周期相互独立；首次快照和持续 WAL 复制可验证；在独立恢复路径上恢复后校验 SQLite 完整性与 Step 8 状态机关键数据。

非目标：
- 不做 Raft、自动故障转移、跨机热备、读副本或双写；不引入第二台生产机。
- Litestream 的 VFS read replicas 是可选能力，本项目不启用，不将其挂载给 Wiktor 查询。
- 不备份 qdrant；按现有 generation/content-hash 规则从 SQLite 知识/事实平面重建派生向量索引。
- 不把 litestream 客户端库或恢复逻辑塞进 core/server，也不更改内容哈希、任务状态机、CAS、发布事务。

## 2. 已确认约束与术语

- 生产主机：Debian 13 trixie、x86_64、2 GiB RAM、39 GB 磁盘（当前约 37 GB 可用）；本机 macOS arm64。二者当前均无 litestream。
- 可从 GitHub release v0.5.17 获取对应静态二进制；Go 工具链不作为生产安装前提。所有真实验收以 Linux 为准，macOS 仅为等价脚本验证环境。
- SQLite 已使用 WAL，`busy_timeout=5000`；server 的 `--db` 是数据库路径权威来源，部署默认规范路径 `/srv/wiktor/data/wiktor.db`。服务和 litestream 必须配置为同一绝对路径。
- **本 Step 中的“主从”只指一个 SQLite 写主库持续异步复制到备份副本，并通过恢复演练验证；不承诺跨机在线接管、零停机或 etcd 式共识。**
- 备份目标先支持 `file://`，以及 Linux→用户 Mac 的 SFTP 目标（前提是 Mac 开启 Remote Login、账号/路径权限和网络可达）；OSS/S3 仅提供不启用的模板。

## 3. 决策 D1–D12

| ID | 决策 | 理由与边界 | 批次 | 验收 |
|---|---|---|---|---|
| D1 | Litestream 作为独立 Linux systemd 服务运行，与 Wiktor 生命周期解耦。 | WAL 复制无需改应用；一方重启不要求另一方重启。 | B1 | A1–A4 |
| D2 | 主数据库只允许一个写主库；复制目标只读备份。 | SQLite 单写与当前部署形态一致，不产生 split-brain。 | B1 | A3、A8 |
| D3 | 数据库路径唯一配置，默认 `/srv/wiktor/data/wiktor.db`；server `--db`、restore、litestream 必须一致。 | 防止备份了错误/空库。 | B1 | A2、A7 |
| D4 | `replicas` 配置是一等扩展点，提供 SFTP 和 `file://` 两份模板；部署实际选择其一，缺省模板不得伪装成已配置远端副本。 | 当前无需外部云密钥且可落地。 | B1 | A3–A5 |
| D5 | 建议生产首选远端 SFTP（用户 Mac 可达时），否则使用独立挂载盘或 `/srv/backup` 的 `file://`；本地同盘目录仅用于演练/短期副本，不抵御整盘损坏。 | 明确故障域差异，不臆造不存在的 bucket。 | B1 | A4、A5、A10 |
| D6 | OCI S3 / 阿里云 OSS（S3 兼容或 Litestream 支持的对应 URL）只留配置示例，需用户自行确认端点、bucket 和凭证后启用。 | 外部密钥/bucket 当前未提供。 | B3 | A12 |
| D7 | Litestream 版本固定 v0.5.17；Linux 下载、SHA-256 校验后安装至 `/usr/local/bin/litestream`；不在构建时动态下载。 | 可重复、供应链校验、生产无 Go 依赖。 | B1 | A1 |
| D8 | 配置 `/etc/litestream.yml`：数据库绝对路径、replica URL、`sync-interval: 1s`、`snapshot-interval: 1h`、有限保留策略；密钥仅由 root 管理的环境文件/凭证机制注入，不入 Git。 | 将复制延迟、恢复点和保留预算显式化。 | B1 | A3、A4、A6 |
| D9 | systemd unit 使用 `litestream replicate -config /etc/litestream.yml`、`Restart=on-failure`、开机启用、最小权限运行；配置/凭证变化需明确 daemon-reload/restart。 | 可运维且失败自动重试。 | B1 | A6 |
| D10 | 本步默认零 Rust 代码改动；不新增 `wiktor backup status`。运维状态以 systemd + `litestream status/ltx/replicate` 命令为准（v0.5.17 无 `generations`，见 STEP9-012）。 | 避免 CLI 包装 litestream 版本和输出协议；server 数据路径已有参数。 | B1/B3 | A6、A8 |
| D11 | 恢复必须停止 Wiktor 与 replicate，恢复到隔离路径；通过 SQLite integrity/schema/row-count 对照后，人工切换路径并启动服务。 | 防止覆盖在线库或把不完整库作为主库。 | B2 | A7–A10 |
| D12 | “RPO≈0”仅是健康网络下 1 秒 WAL 同步目标，不是无条件 SLA；每次发布/配置变更需保留恢复记录。 | 异步复制和目标可达性限制最终 RPO。 | 全批 | A4、A10 |

## 4. 部署与安装（A-first）

### 4.1 路径和目录约定

默认安装布局：

```text
/srv/wiktor/bin/wiktor                 # 发布的 server/CLI 二进制
/srv/wiktor/data/wiktor.db             # 唯一 SQLite 主库（WAL 同目录）
/srv/wiktor/restore/                    # 临时恢复目录，不可作为活动库
/srv/backup/wiktor/litestream/          # file:// 示例目标；建议独立挂载卷
/etc/litestream.yml                     # root 管理配置，不入仓库
/etc/wiktor/litestream.env              # 可选 secret 环境文件，root:root 0600
```

若 systemd 的 Wiktor `ExecStart` 使用其他 `--db` 路径，必须同时改 litestream 配置和部署检查；禁止从文件名猜路径。

### 4.2 安装步骤 A1–A8

**A1 — 获取与校验二进制（Linux 生产机）**

使用固定 release `v0.5.17` 的 `litestream-v0.5.17-linux-amd64.tar.gz`（以 release 实际资产名为准），通过 HTTPS 下载；从可信 release 发布页/签名清单获取 SHA-256，校验成功后解包并安装：

```sh
sudo install -d -m 0755 /usr/local/bin
sha256sum -c litestream-v0.5.17-linux-amd64.tar.gz.sha256
 tar -xzf litestream-v0.5.17-linux-amd64.tar.gz litestream
sudo install -o root -g root -m 0755 litestream /usr/local/bin/litestream
/usr/local/bin/litestream version
```

实现脚本须在校验缺失或失败时停止；不得把未经验证的下载落成正式二进制。release 的资产文件名、checksum 获取方式以 GitHub v0.5.17 发布页核对后写入 `deploy/` 脚本常量，并允许通过参数覆盖下载 URL/checksum 文件供镜像环境使用。

**A2 — 创建数据与备份目录（Linux）**

创建 `/srv/wiktor/{bin,data,restore}`，数据目录归运行 Wiktor 的专用账号所有；`/srv/backup/wiktor/litestream` 仅在采用 file replica 时创建。不要把 `/srv/backup` 与数据库放在同一可故障磁盘后宣称具备异机灾备。数据库文件不得由定时任务删除/替换；升级仅停止服务后替换程序二进制，不覆盖数据库。

**A3 — 配置 schema（Linux；配置安装到 `/etc/litestream.yml`）**

部署者从以下形状生成配置；实际字段拼写须按固定 litestream 版本的 `litestream replicate -help`/文档验证。`dbs[].replicas[]` 每个 replica 独立配置，必须且只应启用所选目标：

```yaml
db-path: /srv/wiktor/data/wiktor.db
# Litestream config schema commonly uses dbs; verify exact keys against v0.5.17.
dbs:
  - path: /srv/wiktor/data/wiktor.db
    replicas:
      - url: file:///srv/backup/wiktor/litestream
        retention: 168h
        sync-interval: 1s
        snapshot-interval: 1h
```

注意：若 v0.5.17 的 YAML schema 将 `sync-interval`、`snapshot-interval`、`retention` 放在 replica/root 其他位置，以该版本官方 schema 为准；脚本必须包含 config validation，且不得因示意 YAML 造成服务启动失败。保留策略起始建议 `retention: 168h`（7 天）；实施者须确认 Litestream v0.5.17 对 snapshot/WAL 保留字段的准确语义，若 `retention` 不属于 replica 配置，则使用该版本支持的等价配置并登记 STEP9 偏差。不得使用未被该版本支持的 YAML 字段。

`file://` 目标的路径必须可由 litestream service account 写入。SFTP 模板 URL 形状按 v0.5.17 文档配置，例如 `sftp://user@host:22/absolute/path`（具体支持的 URL/known-hosts/密码或私钥注入方式必须在 Linux 实测，禁止把 secret 放在 tracked YAML）。密钥/host-key 验证按 SSH 安全策略配置；禁止 `StrictHostKeyChecking=no`。只启用已验证的单一 URL。

OSS/S3 示例另置 `deploy/litestream.s3.example.yml`，注释标明需用户提供 endpoint/bucket/credentials；永不作为默认配置，不保存真密钥。

**A4 — 目标可达与复制配置验证（Linux）**

在启动前验证数据库存在、WAL 模式、目标写权限、目标端点可达与认证。启动 `litestream replicate -config /etc/litestream.yml` 前运行该版本提供的 config/check 子命令（如无独立验证子命令，则以 `replicate` 前台启动并检查解析日志为准）。先以前台方式验证，确认写入 replica、可查询 generation；成功后交由 systemd。

**A5 — 首次快照与 RPO 观察（Linux）**

启动 replicate 后制造一次合法 SQLite 写入（优先使用现有 Wiktor CLI/status 或测试库写入，不写入生产业务假数据）；确认首次 generation/snapshot 已产生。对一个受控测试副本执行恢复并读取该行。记录写入时间与最近可恢复时间，目标正常状况下 ≤ 1 秒 WAL sync interval 加实际网络/调度延迟；该数值不是严格 SLA。

**A6 — systemd unit（Linux）**

建议 `deploy/litestream.service`：

```ini
[Unit]
Description=Litestream continuous SQLite replication for Wiktor
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=wiktor
Group=wiktor
EnvironmentFile=-/etc/wiktor/litestream.env
ExecStart=/usr/local/bin/litestream replicate -config /etc/litestream.yml
Restart=on-failure
RestartSec=5s
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/srv/backup/wiktor/litestream /srv/wiktor/data

[Install]
WantedBy=multi-user.target
```

若 SFTP 凭证要求独立 key 文件，`ReadOnlyPaths` 显式允许读取该文件，权限 `0600`、owner root 或专用服务账号；不要把凭证交给非特权用户。systemd 部署脚本执行 `systemctl daemon-reload`、`enable --now litestream`，并验证 `systemctl is-enabled`/`is-active`。安全沙箱目录按 file/SFTP 目标最小化调整。

**A7 — 统一路径及服务依赖（Linux）**

Wiktor server unit 的 `--db /srv/wiktor/data/wiktor.db` 与 YAML 的 `dbs[].path` 必须完全一致。Litestream 不应强制作为 Wiktor 启动前置条件：备份器故障不应阻止服务提供查询/写入；但需告警，并将备份健康列为部署 readiness 检查。禁止两个 systemd unit 同时管理/改写 SQLite 库文件。

**A8 — 运维检查（Linux）**

`systemctl status litestream`、`journalctl -u litestream`、`litestream status -config /etc/litestream.yml <db-path>` 与 `litestream ltx -config /etc/litestream.yml <db-path>`（v0.5.17 无 `generations`，见 STEP9-012）用于确认进程与最近 generation；`wiktor status --db <path>` 可辅助校验应用 schema/row_counts，但不替代备份状态。任何 CLI 代码改动必须单独论证并记录，不属默认交付。

### 4.3 部署脚本落点

新增 `deploy/`：

```text
deploy/
  install-litestream.sh           # 下载指定架构版本、checksum 校验、安装
  litestream.yml.example          # file:// 可运行样例；不含 secret
  litestream.sftp.example.yml     # SFTP 模板与凭据要求
  litestream.s3.example.yml       # 可选 OSS/S3 模板，默认禁用
  litestream.service              # systemd unit
  backup-preflight.sh             # 路径、权限、配置与 WAL 前置检查
  restore-drill.sh                # 隔离恢复演练
  README.md                       # 安装、切换目标、恢复与故障处理
```

脚本必须 `set -euo pipefail`，对路径参数加引号，检查 root/账号/磁盘空间；默认 dry-run/打印动作，除明确 `--apply` 外不变更系统。部署产物可以通过 tar over SSH 上传后在 Linux 执行；`/etc` 配置和任何 secret 不进入 Git、tar 包或仓库同步清单。文件权限/owner 在 Linux 执行时设置，不依赖 macOS tar 保留 Linux 属性。

## 5. Wiktor 侧配套与数据库文件语义

### 5.1 是否需要代码改动

结论：默认不改 Rust。WAL 数据库由 Litestream 读取 WAL 并异步复制；Wiktor 正常 checkpoint、进程重启不会等同于删除/替换活动数据库文件。当前 server `--db` 已提供显式路径，不需再建存储抽象或写入钩子。部署升级应替换可执行文件，不替换 DB 文件；restore 是离线维护操作，必须停止所有 SQLite 连接后进行。

不得把 Litestream 复制目录直接作为 SQLite 查询源；不启用 Litestream VFS read replicas。数据库恢复后，原有 schema migration、content hash、任务 epoch/fencing、CAS、发布状态机仍由应用和现有迁移语义负责；备份不改变这些契约。

### 5.2 CLI 选择

不新增 `wiktor backup status`。Litestream 版本/子命令输出不是稳定 Wiktor API，包装子进程会将 core/CLI 与部署工具耦合。用 systemd 状态、Litestream generation 检查和 Wiktor `status` 组成 runbook。未来如确有跨后端备份抽象需求，另立设计而非本步预埋。

## 6. 恢复流程与脚本化验收

### 6.1 安全恢复规程

正式灾难恢复顺序固定：
1. `systemctl stop wiktor-server`（按实际 unit 名称）以及 `systemctl stop litestream`；确认进程退出、无打开的 DB 连接。
2. 将目标恢复至新的隔离路径，如 `/srv/wiktor/restore/wiktor.db`，不得直接覆盖原 DB。
3. `litestream restore -config /etc/litestream.yml -o /srv/wiktor/restore/wiktor.db /srv/wiktor/data/wiktor.db`（核对 v0.5.17 参数顺序/是否需要 `-replica`，脚本按官方 CLI 固定；file/SFTP replica 需明确 selector）。
4. 对恢复库执行 `PRAGMA integrity_check`，期望唯一结果 `ok`；执行 `PRAGMA foreign_key_check`，期望零行；检查 schema version 与至少 `compile_tasks`、`compile_attempts`、`compile_source_heads`、`pages`、`page_quality`、`review_queue` 存在（实际表名按当前 schema 精确核对）。
5. 在隔离副本和可比基准上输出上述表 `COUNT(*)`。灾难模拟验收要求恢复后 row counts 与复制目标在模拟删除前记录的 manifest 完全一致；不要把 counts 相同当作内容校验的替代，`integrity_check` 与关键任务/页面抽样查询均须通过。
6. Wiktor `status --db /srv/wiktor/restore/wiktor.db` 只读确认 schema/row_counts；在备份副本上执行恢复演练时，Wiktor 当前 CLI 不存在的命令由 `sqlite3` 断言补足，不允许臆造 CLI 能力。
7. 人工确认恢复点及数据差异后，在维护窗口将恢复文件复制/原子 rename 到规范主库路径；确保属主/权限正确，再启动 Wiktor，然后 Litestream；确认新 generation 开始复制。旧库保留只读隔离直到审计完成。

### 6.2 恢复演练命令与预期结果

`deploy/restore-drill.sh` 只能使用独立临时演练目录和显式 `--confirm-test-db`；若目标是生产路径且没有额外强确认，拒绝执行。Linux 验收示例：

```sh
# 1. 在测试数据库启动复制，写入可辨识的测试记录并保存 manifest
./deploy/backup-preflight.sh --db /tmp/wiktor-ha-test/wiktor.db --replica file:///tmp/wiktor-ha-test/replica
# 2. 确认 generation，记录关键表行数与测试 marker
sqlite3 /tmp/wiktor-ha-test/wiktor.db 'PRAGMA integrity_check;'
./deploy/restore-drill.sh --db /tmp/wiktor-ha-test/wiktor.db \
  --restore-to /tmp/wiktor-ha-test/restored.db --confirm-test-db
```

演练脚本执行并打印：`litestream restore` 实际命令、`integrity_check=ok`、`foreign_key_violations=0`、迁移版本、关键表 row_counts、marker 查询结果及 `PASS`。用户要求的灾难模拟在专用 disposable DB 上实施“服务/replicate 停止→移走或删除该测试 DB→从 replica restore”，而非删除生产数据库。预期：恢复命令退出 0，marker 与 manifest 一致，所有关键表存在，integrity 检查为 `ok`，foreign key violations 为 0；任何不一致都 FAIL、保留现场、不触碰正式 DB。

macOS arm64 等价演练使用 GitHub v0.5.17 darwin-arm64 资产及同一脚本逻辑、SQLite/WAL/file replica；用于开发者快速验证，不替代 Debian/Linux 的安装、systemd、SFTP 与生产验收。SFTP 必须在 Linux→Mac（Mac Remote Login 已开启）真实方向验证，不得用反方向成功冒充。

## 7. 风险与预置偏差表

| ID | 风险/约束 | 处理与验收边界 |
|---|---|---|
| STEP9-001 | 应用 checkpoint 或重启期间复制 | WAL 正常 checkpoint/服务重启不要求复制器重启；litestream replicate 持续扫描/复制 WAL。通过重启 smoke 验证；遇到不可恢复 SQLite/Litestream 错误告警，不宣称无条件不会中断。 |
| STEP9-002 | 备份目标不可达 | Litestream 进程保留本地 WAL 读取能力并重试/恢复复制的前提是本地 WAL/磁盘仍可用且未耗尽；监控磁盘与复制落后。目标恢复后确认 generation 补齐。具体 backpressure 行为需 v0.5.17 实测，不用“永不丢数据”措辞。 |
| STEP9-003 | 本机 2 GiB 内存、39 GB 磁盘 | Litestream Go 常驻开销预计低（约几十 MB，目标预算 ≤64 MiB RSS，需实测）；限制 systemd 资源不应导致 OOM。副本保留按 7 天起始，监控 `df`/`du`；磁盘低于 20% 可用空间告警，低于 10% 进入人工处置，不静默删 WAL。 |
| STEP9-004 | `/srv/backup` 同盘不是异机备份 | 标记为可运行 file replica 与恢复演练路径，不能抵御整盘损坏；首选远端 Mac SFTP 或独立挂载盘。 |
| STEP9-005 | SFTP 端点权限、密钥、host key 或网络变化 | root 管理 secret/私钥；host key 校验启用；远端目录预建、专用账号最小权限；systemd journal 暴露连接错误但不泄密。Linux→Mac 可达性是上线前置条件。 |
| STEP9-006 | Litestream v0.5.17 配置/schema 或 URL 支持差异 | 用固定版本官方文档/二进制验证配置项、SFTP URL、restore/status/ltx 参数；示例 YAML 不等同于免验证契约。发现差异登记新 STEP9 偏差，不静默假设。 |
| STEP9-007 | tar over SSH 部署 config/systemd | 仓库只存无密钥模板与 unit；`/etc` 实际配置由 Linux 安装步骤渲染，secret 不进 Git/tar；上传后检查路径、LF、mode、owner，再 daemon-reload。 |
| STEP9-008 | 恢复误覆盖在线库 | restore 默认隔离输出，先停 server/replicate、校验、人工切换；脚本拒绝活动 DB 路径及危险 `/`/空路径。 |
| STEP9-009 | WAL 被截断/目标满或复制延迟超预算 | 定期核对 systemd、最近 generation、磁盘水位；故障处置优先确保活动 DB/WAL 不被手工清理，先扩容/恢复目标再恢复复制。 |
| STEP9-010 | 文档“RPO≈0”被误读 SLA | 1 秒只是 `sync-interval` 目标；最终 RPO 受网络、SFTP、调度、磁盘与最后成功复制点约束。恢复记录必须报告实际 generation 时间。 |
| STEP9-011 | 真实 release 资产名与 §4.2 A1 示例不符 | 实测 GitHub v0.5.17 资产名为 `litestream-0.5.17-{linux-x86_64,darwin-arm64}.tar.gz`（**版本号无 `v` 前缀**、架构名 `x86_64`/`arm64` 而非 `amd64`）；§4.2 A1 的 `litestream-v0.5.17-linux-amd64.tar.gz` 会 404。`install-litestream.sh` 已改为剥离 `v`、映射真实资产名。 |
| STEP9-012 | v0.5.17 无 `generations` 子命令 | 旧 `litestream generations`（及 `ls`）在 v0.5.17 已移除；用 `litestream status -config CFG DB` 查同步状态、`litestream ltx -config CFG DB` 检测副本快照/LTX 段。`restore-drill.sh` 首快照等待与 `deploy/README.md` 运维命令已改。 |
| STEP9-013 | v0.5.17 配置 schema 的键层级与 §4.2 A3 示意不同 | 实测 `sync-interval` 是 **db 级**键（控制实际同步频率）；`snapshot-interval`/`retention` 是 **replica 级**键。§4.2 A3 将 `sync-interval` 放 replica 级虽被宽容解析但不保证生效；三个样例 YAML 与 `restore-drill.sh` 内联配置已把 `sync-interval` 移至 `dbs[].` 下。SFTP 对象键 `type/host/port/path/user/password`、S3 键 `url/access-key-id/secret-access-key` 均被 v0.5.17 实测接受。 |
| STEP9-014 | 离线/镜像环境下载慢 | `install-litestream.sh` 复用已存在且校验通过的 tarball 与 `checksums.txt`（缺失/校验不匹配才重新拉取），并做 SHA-256 命令可移植（Linux `sha256sum` / macOS `shasum -a 256`）；走代理时设 `HTTPS_PROXY` 即可。 |
| STEP9-015 | 同机验证用 Android SDK 的 sqlite3 CLI | macOS 上 `sqlite3` 来自 Android SDK（3.50.6），行为一致；Linux 用 distro `sqlite3`（3.46.1）。两环境演练结果一致。 |

实现者发现与本规范不同，追加 `STEP9-xxx`，说明原因、配置/接口影响和验收变化；不得变更单写库边界、停止服务恢复纪律或已拍板非目标。

## 8. 验收标准 A1–A12

| # | 运行位置 | 验收判据 | 可执行结果 |
|---|---|---|---|
| A1 | Linux x86_64 | v0.5.17 下载、SHA-256 验证、安装成功；二进制架构/版本正确。 | `litestream version` 报 v0.5.17；checksum mismatch 阻止安装。 |
| A2 | Linux | server 与 Litestream DB 路径完全匹配；preflight 对缺文件、非 WAL、权限不足、空间不足明确失败。 | `backup-preflight.sh` exit 0/非 0 与场景一致。 |
| A3 | Linux | 单一 replica 配置可解析并确实使用所选 file 或 SFTP 目标；配置不含 secret。 | 前台 replicate 启动成功，generation 可列出。 |
| A4 | Linux | `sync-interval=1s`、snapshot 周期 1h、7 天保留配置在 v0.5.17 实际生效。 | config 检查 + 连续写入观测 + generation/保留清单。 |
| A5 | Linux；SFTP 附加 Mac | 远端目标可写、不可达会重试并在恢复后追平；验证目的端写入。 | SFTP 连接成功/故障/恢复日志；无 secret 泄露。 |
| A6 | Linux | systemd unit enabled/active，on-failure 重启；Wiktor 独立重启不需要人为重启 litestream。 | `systemctl is-enabled/is-active`；重启 smoke。 |
| A7 | Linux | disposable DB 灾难模拟、隔离 restore、integrity/schema/row-count/marker 验证通过。 | restore-drill 输出所有检查并最终 `PASS`。 |
| A8 | Linux | Litestream 进程停止期间 Wiktor 仍可启动/服务；Litestream 再启后复制恢复。 | 分别检查 server health、replicate generation。 |
| A9 | Linux | restore 脚本拒绝目标为活动生产 DB；不正确权限/损坏副本不得切换。 | 负向用例 exit 非 0、源/活动 DB 哈希未变。 |
| A10 | Linux 生产发布退出条件 | 真实配置的恢复演练通过，并保存时间、replica 类型、generation、row_counts、integrity 与操作者记录。 | 演练报告存运维记录；未通过不得标记 Step 9 完成。 |
| A11 | macOS arm64（可选等价） | darwin-arm64 固定版 + `file://` 完成同一 disposable restore 脚本。 | marker/完整性一致；不替代 Linux A1–A10。 |
| A12 | Linux或配置静态检查 | OSS/S3 样例不含 credentials 且默认未启用；切换前需显式配置验证。 | `rg`/脚本扫描无密钥；默认模板只启用单个已配置目标。 |

### 8.1 真实验证记录（2026-09-24，Linux Debian 13 x86_64）

| 项 | 结果 |
|---|---|
| A1 | `install-litestream.sh --apply` 安装 `0.5.17`，`litestream version`=0.5.17；SHA-256=cfb371…；复用预下载 tarball 校验通过。 |
| A2 | `backup-preflight.sh --db <test> --replica file://…` 输出 `preflight PASS`（litestream 存在、WAL、replica 可写、磁盘 OK）；非 WAL 库 FAIL（rc=1）。 |
| A7 | `restore-drill.sh` 灾难模拟 → 隔离 restore → `integrity_check=ok`、`foreign_key_violations=0`、row_count 3==3、marker 命中、post-snapshot 命中 → `=== RESTORE DRILL PASS ===`（rc=0）。 |
| A8 | litestream 停止期间写入 2 行 → 重启 → restore 恢复 3 行（含 down 期间写入），integrity ok。 |
| A9 | 缺 `--confirm-test-db`（rc=3）与生产路径 `/srv/wiktor/*`（rc=3）均拒绝执行。 |
| A3/A4/A12 | file:// 单一 replica 实测解析并复制；`sync-interval` db 级生效（post-snapshot 增量同步被恢复）；S3/OSS 模板仅示例无凭证、默认不启用。 |
| A5/A6/A10 | SFTP 到 Mac 与 systemd 常驻为生产 bring-up 项：本步在 disposable DB 上完成文档化 §6.2 验收形态；正式上线时按 §4 与 runbook 启用 SFTP/systemd 并做真实配置演练记录。 |

## 9. 给 wiktor-builder 的实现批次

| 批次 | 内容 | 必须完成验收 | 可独立验证 |
|---|---|---|---|
| B1 部署与配置 | 新增 `deploy/` 安装/preflight、file/SFTP 示例、systemd unit、README；固定 v0.5.17；在 Linux 核对准确 YAML key/命令行/下载资产/checksum；服务路径与 `--db` 对齐。 | A1–A6、A12 | shellcheck（若环境可用）、脚本 dry-run、Linux disposable file replica 首次 generation；SFTP 另跑 A5。 |
| B2 恢复演练 | `restore-drill.sh` 实现显式测试库保护、manifest、停止/隔离 restore、integrity/schema/foreign-key/row-count/marker 校验；不自动改生产路径。 | A7、A9、A10 | Linux disposable file replica 灾难模拟，删除/移走测试库后 restore 得 `PASS`。 |
| B3 文档、运维收口与偏差登记 | 完成 deploy README/runbook、Mac 等价指引、SFTP 与 OSS/S3 可选模板、磁盘/目标不可达说明；默认无 Rust 改动。只有发现强制性应用缺口时，提交独立最小代码改动及理由、接口和测试。 | A8、A11、A12 + A1–A10 回归 | Linux 故障恢复/重启演练、Git secret scan、文档命令核对；若改 Rust，执行 workspace fmt/clippy/test 并单列结果。 |

每批必须可独立验证；部署失败不得影响 SQLite 主库；restore 脚本在测试路径以外 fail-closed。**Step 9 退出条件是 Linux 真实部署配置上的完整恢复演练通过，不是仅安装 litestream 或看到进程 active。**

## 10. 运维退出检查

交付前必须同时确认：单一写主库；数据库路径一致；目标落在明确故障域；Litestream 版本/配置已验证；systemd restart 与目标不可达行为已测；磁盘/保留有运维告警；一次真实复制恢复演练恢复出的 SQLite 满足完整性、schema 与关键行数/marker 检查。RPO 记录为实际最后可恢复 generation，而非文档目标值。

<!-- END STEP9 SPEC v1.0 -->
