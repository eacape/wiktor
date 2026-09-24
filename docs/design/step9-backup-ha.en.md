# Step 9 Design Specification: Litestream Continuous Backup and Single-Node Disaster Recovery

> Version: v1.0 (2026-09-23)  
> Authority: `docs/MASTER-PLAN.md` v3.2 §5.5, §10, §17; deployment baseline: Step 7 server and Step 8 state machine  
> Implementation target: `wiktor-builder`; independent acceptance target: `test-engineer`  
> Chinese is the authoritative design; this document mirrors `step9-backup-ha.md` section by section.

## 1. Goals and non-goals

This step puts the SQLite WAL primary under continuous replication by a separately deployed Litestream process and establishes an executable recovery drill. Production is a single Debian 13 x86_64 host with a remote or local replica, not a multi-node HA cluster. The sole `wiktor.db` is the runtime source of truth; qdrant vector data is a derived index that can be rebuilt and is excluded from runtime-backup integrity criteria.

Goals: reproducible deployment, configurable replica targets, independent application/replicator lifecycles, verifiable first snapshot and ongoing WAL replication, and recovery to an isolated path followed by SQLite integrity and Step 8 state-machine data checks.

Non-goals:
- No Raft, automatic failover, cross-host hot standby, read replica, or dual writes; no second production host.
- Litestream VFS read replicas are an optional capability and are not enabled or mounted into Wiktor queries.
- No qdrant backup; rebuild derived vector indexes from SQLite knowledge/fact planes using existing generation/content-hash rules.
- No Litestream client in core/server and no change to content hashes, task state machine, CAS, or publish transactions.

## 2. Confirmed constraints and terminology

- Production host: Debian 13 trixie, x86_64, 2 GiB RAM, 39 GB disk (about 37 GB currently free); developer host: macOS arm64. Neither currently has Litestream.
- Matching static binaries are available from GitHub release v0.5.17. A Go toolchain is not a production installation prerequisite. All real acceptance runs on Linux; macOS is only an equivalent script-verification environment.
- SQLite already uses WAL and `busy_timeout=5000`; server `--db` is authoritative for the database path. The deployment default is `/srv/wiktor/data/wiktor.db`. Server and Litestream must use the same absolute path.
- **“Primary/replica” in this step means one SQLite write-primary continuously replicating to backup replicas, validated through a recovery drill. It does not promise online cross-host takeover, zero downtime, or etcd-style consensus.**
- Targets first support `file://` and Linux-to-user-Mac SFTP (only if Remote Login, account/path permissions, and network reachability are configured). OSS/S3 are inactive templates only.

## 3. Decisions D1–D12

| ID | Decision | Reason and boundary | Batch | Acceptance |
|---|---|---|---|---|
| D1 | Run Litestream as a standalone Linux systemd service, independent of Wiktor's lifecycle. | WAL replication requires no application changes; restarting either process does not require restarting the other. | B1 | A1–A4 |
| D2 | Permit one writable primary database only; replicas are backup destinations. | Matches SQLite's single-writer model and avoids split brain. | B1 | A3, A8 |
| D3 | Use one authoritative database path, default `/srv/wiktor/data/wiktor.db`; server `--db`, restore, and Litestream must match. | Prevents backing up the wrong or empty database. | B1 | A2, A7 |
| D4 | Make `replicas` a first-class configuration extension point, with both SFTP and `file://` templates; deployment selects one, and an unconfigured template must not masquerade as a remote replica. | Deployable now without external cloud credentials. | B1 | A3–A5 |
| D5 | Prefer remote SFTP when the user's Mac is reachable; otherwise use a separately mounted disk or `file://` at `/srv/backup`. A same-disk local directory is for drills/short-term replication only and does not protect against disk loss. | Makes failure-domain limitations explicit without assuming a nonexistent bucket. | B1 | A4, A5, A10 |
| D6 | Keep OCI S3 / Alibaba Cloud OSS (S3-compatible or supported endpoint URL) as examples only; enable only after the user supplies endpoint, bucket, and credentials. | External credentials/bucket are not currently available. | B3 | A12 |
| D7 | Pin Litestream v0.5.17; download Linux binary, verify SHA-256, install to `/usr/local/bin/litestream`; do not download dynamically during build. | Reproducibility, supply-chain verification, no production Go dependency. | B1 | A1 |
| D8 | Configure `/etc/litestream.yml` with absolute database path, replica URL, `sync-interval: 1s`, `snapshot-interval: 1h`, and finite retention; inject secrets only via root-managed environment/credential mechanism, never Git. | Makes replication lag, recovery points, and retention budget explicit. | B1 | A3, A4, A6 |
| D9 | Use a systemd unit invoking `litestream replicate -config /etc/litestream.yml`, `Restart=on-failure`, boot enablement, and least privilege; config/credential changes require explicit reload/restart. | Operational and self-retrying. | B1 | A6 |
| D10 | Default to zero Rust changes; add no `wiktor backup status`. Use systemd plus `litestream status/ltx/replicate` for operations (no `generations` in v0.5.17, see STEP9-012). | Avoids wrapping Litestream's version/output protocol; server already has a path option. | B1/B3 | A6, A8 |
| D11 | Restore to an isolated path with both Wiktor and replicate stopped; switch paths manually only after SQLite integrity/schema/row-count comparison. | Avoids overwriting a live DB or promoting an incomplete copy. | B2 | A7–A10 |
| D12 | “RPO≈0” is only a one-second WAL sync target on a healthy network, not an unconditional SLA; record recovery drills and config changes. | Async replication and target reachability constrain actual RPO. | All | A4, A10 |

## 4. Deployment and installation (A-first)

### 4.1 Path and directory convention

Default layout:

```text
/srv/wiktor/bin/wiktor                 # released server/CLI binary
/srv/wiktor/data/wiktor.db             # sole SQLite primary (WAL alongside it)
/srv/wiktor/restore/                    # temporary recovery directory, never active DB
/srv/backup/wiktor/litestream/          # file:// example target; preferably separate mount
/etc/litestream.yml                     # root-managed configuration, not in repository
/etc/wiktor/litestream.env              # optional secret environment file, root:root 0600
```

If the Wiktor systemd `ExecStart` uses another `--db` path, update Litestream configuration and deployment checks together; never infer the path from a filename. Scheduled jobs must not delete/replace the database. Upgrades replace the executable only after stopping the service; they do not overwrite the DB.

### 4.2 Installation steps A1–A8

**A1 — Download and verify the binary (Linux production host)**

Use the fixed `v0.5.17` release asset `litestream-v0.5.17-linux-amd64.tar.gz` (confirm the actual asset name on the release), download over HTTPS, obtain SHA-256 from the trusted release page/checksum manifest, verify, extract, and install:

```sh
sudo install -d -m 0755 /usr/local/bin
sha256sum -c litestream-v0.5.17-linux-amd64.tar.gz.sha256
 tar -xzf litestream-v0.5.17-linux-amd64.tar.gz litestream
sudo install -o root -g root -m 0755 litestream /usr/local/bin/litestream
/usr/local/bin/litestream version
```

The installer must stop if the checksum is missing or invalid; it must never promote an unverified download to the installed binary. Verify asset names and checksum procedure against the GitHub v0.5.17 release and record them in `deploy/` script constants. Allow URL/checksum-file overrides for mirrors.

**A2 — Create data and backup directories (Linux)**

Create `/srv/wiktor/{bin,data,restore}`; the data directory belongs to the dedicated Wiktor runtime account. Create `/srv/backup/wiktor/litestream` only when using a file replica. A `/srv/backup` directory on the same failing disk is not off-host disaster recovery. Do not run two systemd units that both manage or rewrite database files.

**A3 — Configuration schema (Linux; install at `/etc/litestream.yml`)**

Render the following shape; verify exact field names using the pinned Litestream version's official schema. Each `dbs[].replicas[]` entry is independent, and exactly the selected target should be enabled:

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

If v0.5.17 places `sync-interval`, `snapshot-interval`, or `retention` at another replica/root level, use the official schema for that release. The script must validate the config; the illustrative YAML must not cause service startup failure. Initial retention recommendation is `168h` (7 days). The implementer must confirm the exact retention semantics and supported field placement in v0.5.17. If `retention` is not a replica setting, use the supported equivalent and register a STEP9 deviation. Never use unsupported YAML fields.

The `file://` target must be writable by the Litestream service account. Configure the SFTP template URL according to v0.5.17 documentation, e.g. `sftp://user@host:22/absolute/path` (test exact URL, known-hosts, password/private-key injection behavior on Linux). Do not put secrets in tracked YAML. Configure SSH host-key validation; never use `StrictHostKeyChecking=no`. Enable exactly one tested URL.

Put OSS/S3 examples in `deploy/litestream.s3.example.yml`, annotated that endpoint/bucket/credentials must be user-supplied. It is never the default and contains no real secret.

**A4 — Validate target and replication config (Linux)**

Before launch, verify the DB exists, WAL mode, target write permissions, endpoint reachability, and authentication. Before running `litestream replicate -config /etc/litestream.yml`, use the config/check subcommand offered by that version; if no independent validator exists, run replicate in the foreground and inspect its configuration parsing logs. Validate foreground first, then hand off to systemd after confirming replica writes and queryable generations.

**A5 — First snapshot and RPO observation (Linux)**

Start replicate and produce a legitimate SQLite write (prefer an existing Wiktor CLI/status path or test database write; do not add fake business data to production). Verify a first generation/snapshot. Restore a controlled test replica and read the marker. Record write time and latest recoverable point. Under normal conditions the target is at most one second sync interval plus real network/scheduling delay; this is not a strict SLA.

**A6 — systemd unit (Linux)**

Recommended `deploy/litestream.service`:

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

If SFTP credentials require a separate key file, explicitly allow read access with `ReadOnlyPaths`; use mode `0600` and root or dedicated-service ownership. Never expose credentials to unprivileged users. The deployment script runs `systemctl daemon-reload`, `enable --now litestream`, and verifies `systemctl is-enabled`/`is-active`. Minimize sandbox paths for the selected file/SFTP target.

**A7 — Shared path and service dependencies (Linux)**

Wiktor server unit `--db /srv/wiktor/data/wiktor.db` must exactly match YAML `dbs[].path`. Litestream must not be a hard prerequisite for Wiktor startup: backup failure must not block service queries/writes; it must alert and remain a deployment-readiness check. Do not let two systemd units concurrently manage/rewrite the SQLite database.

**A8 — Operational checks (Linux)**

Use `systemctl status litestream`, `journalctl -u litestream`, `litestream status -config /etc/litestream.yml <db-path>`, and `litestream ltx -config /etc/litestream.yml <db-path>` (no `generations` in v0.5.17, see STEP9-012) to verify process and latest generation. `wiktor status --db <path>` may verify application schema/row counts but does not replace backup status. Any CLI code change requires separate justification and registration; it is not default scope.

### 4.3 Deployment script locations

Add `deploy/`:

```text
deploy/
  install-litestream.sh           # pinned architecture/version, checksum, install
  litestream.yml.example          # runnable file:// sample; no secret
  litestream.sftp.example.yml     # SFTP template and credential requirements
  litestream.s3.example.yml       # optional OSS/S3 template, inactive by default
  litestream.service              # systemd unit
  backup-preflight.sh             # path, permission, config, WAL preflight
  restore-drill.sh                # isolated restore drill
  README.md                       # install, target selection, recovery, troubleshooting
```

Scripts must use `set -euo pipefail`, quote path arguments, check root/account/disk space, and default to dry-run/printing actions; system changes require explicit `--apply`. Deployment artifacts may be uploaded to Linux via tar over SSH and run there. Actual `/etc` configuration is rendered by the Linux installation process; secrets must never enter Git, the tarball, or repository sync list. Set ownership/mode on Linux rather than relying on macOS tar to preserve Linux attributes.

## 5. Wiktor-side support and database file semantics

### 5.1 Whether code changes are required

Conclusion: no Rust change by default. Litestream reads a WAL database and replicates WAL asynchronously; normal Wiktor checkpoints or process restarts are not equivalent to deleting/replacing the active DB file. Server `--db` already provides an explicit path, so no new storage abstraction or write hook is needed. Deployment upgrades replace the executable, not the DB; restore is offline maintenance and requires all SQLite connections to be stopped.

Never use the Litestream replica directory directly as a SQLite query source; do not enable Litestream VFS read replicas. After recovery, existing schema migrations, content hashes, task epoch/fencing, CAS, and publish-state-machine semantics remain the application's responsibility; backup does not change those contracts.

### 5.2 CLI decision

Do not add `wiktor backup status`. Litestream version/subcommand output is not a stable Wiktor API, and wrapping a subprocess would couple core/CLI to a deployment tool. Use systemd state, Litestream generation inspection, and Wiktor `status` as a runbook. If a cross-backend backup abstraction is later needed, design it separately rather than prebuilding it here.

## 6. Recovery procedure and scripted acceptance

### 6.1 Safe recovery procedure

The production recovery sequence is fixed:
1. Stop `wiktor-server` (use the actual unit name) and `litestream`; verify process exit and no open DB connections.
2. Restore to a new isolated path such as `/srv/wiktor/restore/wiktor.db`; never overwrite the original DB directly.
3. Run `litestream restore -config /etc/litestream.yml -o /srv/wiktor/restore/wiktor.db /srv/wiktor/data/wiktor.db` (verify v0.5.17 argument order and whether `-replica` is required; pin official CLI syntax in the script, with an explicit selector for file/SFTP).
4. Run `PRAGMA integrity_check` on the recovered DB; expect the single result `ok`. Run `PRAGMA foreign_key_check`; expect zero rows. Check schema version and existence of at least `compile_tasks`, `compile_attempts`, `compile_source_heads`, `pages`, `page_quality`, and `review_queue` (verify exact current table names against the actual schema).
5. Print `COUNT(*)` for those tables from the restored DB and comparable baseline. In the disaster simulation, recovered row counts must exactly match a manifest captured before test DB deletion. Counts do not replace content validation: `integrity_check` and sampled key task/page queries must also pass.
6. Use `wiktor status --db /srv/wiktor/restore/wiktor.db` for read-only schema/row-count confirmation. If current CLI lacks a command, supplement with `sqlite3` assertions; do not invent CLI capabilities.
7. After manual approval of recovery point and data differences, during a maintenance window copy/atomically rename the recovered DB to the canonical primary path; fix ownership/permissions, then start Wiktor and Litestream. Confirm replication creates a new generation. Retain the old DB read-only and isolated until audit completes.

### 6.2 Drill commands and expected output

`deploy/restore-drill.sh` must use a separate temporary drill directory and explicit `--confirm-test-db`; reject production paths unless an additional explicit confirmation is supplied. Linux acceptance example:

```sh
# 1. Start replication for a test database; write marker and save manifest
./deploy/backup-preflight.sh --db /tmp/wiktor-ha-test/wiktor.db --replica file:///tmp/wiktor-ha-test/replica
# 2. Verify generation, record key-table counts and test marker
sqlite3 /tmp/wiktor-ha-test/wiktor.db 'PRAGMA integrity_check;'
./deploy/restore-drill.sh --db /tmp/wiktor-ha-test/wiktor.db \
  --restore-to /tmp/wiktor-ha-test/restored.db --confirm-test-db
```

The script prints the actual `litestream restore` invocation, `integrity_check=ok`, `foreign_key_violations=0`, migration version, key-table row counts, marker result, and final `PASS`. Simulate disaster on a disposable test DB by stopping service/replicate, moving or deleting that test DB, then restoring from replica—not by deleting production data. Expected: restore exits 0, marker and manifest match, every required table exists, integrity is `ok`, and foreign-key violations are zero. Any mismatch is `FAIL`; preserve evidence and do not touch the production DB.

The equivalent macOS arm64 drill uses the GitHub v0.5.17 darwin-arm64 asset and the same script logic with SQLite/WAL/file replica. It is a developer quick check and does not replace Debian/Linux installation, systemd, SFTP, or production acceptance. Test SFTP in the actual Linux→Mac direction with Remote Login enabled; success in the reverse direction is not equivalent.

## 7. Risks and seeded deviation table

| ID | Risk/constraint | Handling and acceptance boundary |
|---|---|---|
| STEP9-001 | Replication during application checkpoint/restart | Normal WAL checkpoint or service restart does not require restarting the replicator; Litestream continuously scans/replicates WAL. Verify with restart smoke. Alert on unrecoverable SQLite/Litestream errors; do not claim interruption is impossible under all conditions. |
| STEP9-002 | Replica target unreachable | Litestream can retain local WAL-reading ability and retry/resume only while local WAL/disk remains available and does not fill; monitor disk and replication lag. After target recovers, confirm generations catch up. Measure exact v0.5.17 backpressure behavior; never claim “no data loss ever.” |
| STEP9-003 | 2 GiB RAM / 39 GB disk | Expected Go resident cost is low (tens of MB; target budget ≤64 MiB RSS, measure it); systemd limits must not cause OOM. Start at seven-day retention; monitor `df`/`du`; alert below 20% free disk, manual action below 10%; never silently delete WAL. |
| STEP9-004 | `/srv/backup` on same disk is not off-host backup | Allowed as a runnable file replica and drill path, but not protection against whole-disk failure; prefer remote Mac SFTP or separate mounted disk. |
| STEP9-005 | SFTP permissions, keys, host key, or network changes | Root-managed secret/private key; host-key verification on; pre-created remote directory and least-privilege account; journal may report connection errors but never secrets. Linux-to-Mac reachability is a go-live prerequisite. |
| STEP9-006 | Litestream v0.5.17 schema or URL differences | Validate exact config keys, SFTP URL, restore/status/ltx flags with the pinned release docs/binary; sample YAML is not an unverified guarantee. Register a new STEP9 deviation for differences rather than silently assuming. |
| STEP9-007 | Shipping config/systemd with tar over SSH | Repository contains only keyless templates and unit; render actual `/etc` config on Linux; secrets never enter Git/tar; after upload verify paths, LF, mode, owner, then daemon-reload. |
| STEP9-008 | Recovery overwrites active DB | Restore to isolated output by default; stop server/replicate, validate, then manually switch; script rejects active DB path and dangerous `/`/empty paths. |
| STEP9-009 | WAL truncation, full target, or replication lag over budget | Regularly inspect systemd, latest generation, disk watermarks; never manually clean active DB/WAL during incident. Expand/restore target before resuming replication. |
| STEP9-010 | “RPO≈0” misread as an SLA | One second is only the `sync-interval` target; actual RPO depends on network, SFTP, scheduling, disk, and last successful replication point. Recovery report must state actual generation time. |
| STEP9-011 | Real release asset name differs from the §4.2 A1 example | Measured on GitHub v0.5.17: asset name is `litestream-0.5.17-{linux-x86_64,darwin-arm64}.tar.gz` (**no `v` prefix**, arch is `x86_64`/`arm64` not `amd64`); the §4.2 A1 `litestream-v0.5.17-linux-amd64.tar.gz` would 404. `install-litestream.sh` strips `v` and maps the real asset names. |
| STEP9-012 | No `generations` subcommand in v0.5.17 | The old `litestream generations` (and `ls`) are removed in v0.5.17; use `litestream status -config CFG DB` for sync status and `litestream ltx -config CFG DB` to detect replica snapshots/LTX segments. `restore-drill.sh` first-snapshot wait and `deploy/README.md` ops commands are updated. |
| STEP9-013 | v0.5.17 config schema key level differs from the §4.2 A3 sketch | Measured: `sync-interval` is a **db-level** key (controls actual sync frequency); `snapshot-interval`/`retention` are **replica-level**. §4.2 A3 placed `sync-interval` at replica level; it parses leniently but is not guaranteed effective. All three sample YAMLs and the `restore-drill.sh` inline config now put `sync-interval` under `dbs[].`. SFTP object keys `type/host/port/path/user/password` and S3 keys `url/access-key-id/secret-access-key` are all accepted by v0.5.17. |
| STEP9-014 | Slow downloads in offline/mirror environments | `install-litestream.sh` reuses an existing tarball and `checksums.txt` that pass verification (re-fetches only when missing or mismatched) and uses a portable SHA-256 command (Linux `sha256sum` / macOS `shasum -a 256`); set `HTTPS_PROXY` to go through a proxy. |
| STEP9-015 | Local `sqlite3` comes from the Android SDK | On macOS the `sqlite3` CLI is the Android SDK build (3.50.6), behavior is identical; Linux uses the distro `sqlite3` (3.46.1). Drill results match across both. |

If implementation differs from this spec, append `STEP9-xxx` with cause, configuration/interface impact, and acceptance changes; do not change the single-writer boundary, stop-before-recovery discipline, or agreed non-goals.

## 8. Acceptance criteria A1–A12

| # | Run location | Criterion | Executable result |
|---|---|---|---|
| A1 | Linux x86_64 | Download, SHA-256 verification, and installation of v0.5.17 succeed; binary architecture/version match. | `litestream version` reports v0.5.17; checksum mismatch blocks installation. |
| A2 | Linux | Server and Litestream paths match; preflight fails clearly on missing DB, non-WAL mode, insufficient permissions, or insufficient space. | `backup-preflight.sh` exit status matches each scenario. |
| A3 | Linux | One-replica config parses and uses the selected file or SFTP target; no secret appears in config. | Foreground replicate starts; snapshot/LTX segments are queryable via `ltx`. |
| A4 | Linux | `sync-interval=1s`, one-hour snapshots, and seven-day retention are effective in actual v0.5.17. | Config check + continuous-write observation + generation/retention listing. |
| A5 | Linux; Mac additionally for SFTP | Remote target is writable; unreachable target retries and catches up after recovery; destination writes are verified. | SFTP success/failure/recovery logs; no secret leakage. |
| A6 | Linux | systemd unit is enabled/active and restarts on failure; independent Wiktor restart does not require manual Litestream restart. | `systemctl is-enabled/is-active`; restart smoke. |
| A7 | Linux | Disposable-DB disaster simulation, isolated restore, integrity/schema/row-count/marker validation pass. | restore-drill prints all checks and final `PASS`. |
| A8 | Linux | Wiktor can start/serve while Litestream is stopped; replication resumes after Litestream restarts. | Check server health and subsequent replicate generation. |
| A9 | Linux | Restore script rejects the active production DB target; bad permissions/corrupt replica cannot be promoted. | Negative case exits nonzero and source/active DB hashes remain unchanged. |
| A10 | Linux production exit gate | Recovery drill passes with real configuration and records time, replica type, generation, row counts, integrity, and operator. | Drill report is retained in operations record; Step 9 cannot be marked complete without it. |
| A11 | macOS arm64 (optional equivalent) | Pinned darwin-arm64 binary and `file://` complete the same disposable restore script. | Marker/integrity match; does not replace Linux A1–A10. |
| A12 | Linux or static config check | OSS/S3 example has no credentials and is inactive by default; explicit config validation is required before selection. | `rg`/script scan finds no keys; default enables only one configured target. |

### 8.1 Live verification record (2026-09-24, Linux Debian 13 x86_64)

| Item | Result |
|---|---|
| A1 | `install-litestream.sh --apply` installed `0.5.17`; `litestream version`=0.5.17; SHA-256=cfb371…; reused a pre-downloaded, checksum-verified tarball. |
| A2 | `backup-preflight.sh --db <test> --replica file://…` printed `preflight PASS` (litestream present, WAL, replica writable, disk OK); non-WAL DB FAILed (rc=1). |
| A7 | `restore-drill.sh` disaster simulation → isolated restore → `integrity_check=ok`, `foreign_key_violations=0`, row_count 3==3, marker hit, post-snapshot hit → `=== RESTORE DRILL PASS ===` (rc=0). |
| A8 | Wrote 2 rows while litestream was stopped → restarted → restore recovered all 3 rows (including the down-time writes), integrity ok. |
| A9 | Missing `--confirm-test-db` (rc=3) and production path `/srv/wiktor/*` (rc=3) are both refused. |
| A3/A4/A12 | file:// single replica parsed and replicated; db-level `sync-interval` in effect (post-snapshot delta was recovered); S3/OSS templates are credential-free examples, inactive by default. |
| A5/A6/A10 | SFTP to Mac and systemd residency are production bring-up items: this step completed the documented §6.2 acceptance form on a disposable DB; on go-live, enable SFTP/systemd per §4 and the runbook, and record a real-configuration drill. |

## 9. Implementation batches for wiktor-builder

| Batch | Scope | Required acceptance | Independently verifiable |
|---|---|---|---|
| B1 Deployment and configuration | Add `deploy/` installer/preflight, file/SFTP examples, systemd unit, README; pin v0.5.17; verify exact YAML keys/CLI/download asset/checksum on Linux; align server path with `--db`. | A1–A6, A12 | shellcheck if available, script dry-run, first generation from Linux disposable file replica; separately run A5 for SFTP. |
| B2 Recovery drill | Implement `restore-drill.sh` with explicit test-DB guard, manifest, stop/isolated restore, integrity/schema/foreign-key/row-count/marker validation; never automatically switch production path. | A7, A9, A10 | Linux disposable file-replica disaster simulation: remove/move test DB, restore, receive `PASS`. |
| B3 Documentation, operational closeout, and deviation tracking | Complete deploy README/runbook, Mac equivalent guidance, SFTP and optional OSS/S3 templates, disk/target failure guidance; default is no Rust change. Only if a mandatory application gap is proven, submit a separately justified minimal code change, interface, and tests. | A8, A11, A12 plus A1–A10 regression | Linux failure-recovery/restart drill, Git secret scan, command verification; if Rust changes, report workspace fmt/clippy/test separately. |

Each batch must be independently verifiable. Deployment failure must not impair the SQLite primary; restore script must fail closed outside test paths. **Step 9 exits only after a complete recovery drill passes with the real Linux deployment configuration—not merely after installing Litestream or seeing an active process.**

## 10. Operational exit checks

Before delivery, verify together: exactly one writable primary; matching database paths; replica in an explicit failure domain; validated Litestream version/config; tested systemd restart and unreachable-target behavior; operational disk/retention alerts; and a real replication recovery drill whose recovered SQLite passes integrity, schema, and key row-count/marker checks. Report actual last recoverable generation as RPO evidence, not the documentation target.

<!-- END STEP9 SPEC v1.0 -->
