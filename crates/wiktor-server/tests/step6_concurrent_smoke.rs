//! Step 6 批7：并发与收口（spec `step6-feedback-loop.md` §9、§10 A18、§11 批7）。
//! Step 6 batch 7: concurrency and closeout (spec `step6-feedback-loop.md` §9,
//! §10 A18, §11 batch 7).
//!
//! 角色映射：偏差 STEP6-003（SQLite 反馈实现单源在 core kernel）；server
//! handler 与 CLI feedback 命令都是对 `SqliteKernel` 上同一 `FeedbackStore`
//! 面的薄封装。两个独立连接（≈ 两个进程）分别扮演 server 角色
//! （`insert_batch_idempotent` = `post_feedback` 步骤 6 的原调用）与 CLI 角色
//! （`list_reviews`/`approve_review` = `feedback list`/`review approve` 的原
//! 调用），在同一临时 DB 文件上并发交错。
//! Role mapping: deviation STEP6-003 (the SQLite feedback implementation is
//! single-source in the core kernel); the server handler and the CLI feedback
//! commands are thin wrappers over the same `FeedbackStore` face on
//! `SqliteKernel`. Two independent connections (~two processes) play the server
//! role (`insert_batch_idempotent`, the exact call of the `post_feedback`
//! step 6) and the CLI role (`list_reviews` / `approve_review`, the exact calls
//! of `feedback list` / `review approve`), interleaved concurrently on one
//! temporary DB file.
//!
//! 断言口径（A18）：无脏读（批内原子 → 读侧只见整批）、无旧值覆盖（D5 重放不
//! 改载荷）、幂等键唯一、busy/约束/状态错误显式成 `Err`（无静默重试/吞错）、
//! approve 事务与 ingestion 事务不产生半状态。
//! Assertion contract (A18): no dirty reads (batch-atomic → readers only ever
//! see whole batches), no stale-value overwrites (D5 replays never rewrite the
//! payload), idempotency keys stay unique, busy/constraint/state errors surface
//! as explicit `Err` (no silent retry / swallow), and the approve transaction
//! never interleaves with ingestion into a half state.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wiktor_core::kernel::feedback_store::{
    FeedbackEventInput, FeedbackKind, ReviewStatus, ReviewSuggestionInput,
};
use wiktor_core::kernel::QueryLogInsert;
use wiktor_core::types::error::Error;
use wiktor_core::SqliteKernel;
use wiktor_feedback::FeedbackStore;

const DOMAIN: &str = "milk-tea";

/// 并发压力档位：server 40 批 × 5 事件、CLI 40 轮读+审核尝试；短事务下足以
/// 制造频繁写竞争，整体耗时可控制在数秒内。
/// Concurrency scale: 40 server batches x 5 events and 40 CLI read+review
/// rounds; enough to force frequent write contention under short transactions
/// while the whole run stays within a few seconds.
const N_BATCHES: usize = 40;
const N_ROUNDS: usize = 40;
/// 每批事件数：slot 0 是固定幂等键（重放通道），slot 1..5 为批内新键。
/// Events per batch: slot 0 is the fixed idempotency key (the replay channel),
/// slots 1..5 are fresh keys within the batch.
const BATCH_SIZE: usize = 5;

/// 固定幂等键（批 0 落库原载荷，之后每批以**不同** metadata 重发——D5 要求
/// 永不覆盖原行）。
/// The fixed idempotency key (its original payload lands in batch 0; every
/// later batch resubmits it with a **different** metadata — D5 forbids
/// overwriting the original row).
const FIXED_KEY: &str = "fixed-key";
/// 固定键原载荷的序列化形状（kernel 用 `serde_json::to_string` 落库）。
/// The serialized shape of the fixed key's original payload (the kernel
/// persists via `serde_json::to_string`).
const FIXED_ORIGINAL_METADATA: &str = r#"{"note":"original"}"#;

/// 线程内断言走违规清单而非 panic：两条线程都跑完再统一裁决，失败信息完整。
/// In-thread assertions append to a violation list instead of panicking: both
/// threads run to completion and the verdict happens once in main, keeping the
/// full failure picture.
type Violations = Arc<Mutex<Vec<String>>>;

fn note(v: &Violations, thread: &str, what: String) {
    v.lock().unwrap().push(format!("[{thread}] {what}"));
}

/// server 角色事件输入构造。
/// Builds one event input for the server role.
fn input(key: &str, log_id: i64, metadata: serde_json::Value) -> FeedbackEventInput {
    FeedbackEventInput {
        idempotency_key: key.to_string(),
        domain: DOMAIN.to_string(),
        log_id,
        kind: FeedbackKind::Click,
        page_id: Some("milk-tea:drink:boba".to_string()),
        rating: None,
        metadata,
    }
}

/// 批 0 落库原载荷，之后每批用不同 metadata 重发（D5 重放 + 无旧值覆盖）。
/// Batch 0 persists the original payload; every later batch resubmits with a
/// different metadata (the D5 replay + no-stale-overwrite channel).
fn fixed_metadata(batch: usize) -> serde_json::Value {
    if batch == 0 {
        serde_json::json!({"note": "original"})
    } else {
        serde_json::json!({"note": format!("mutated-{batch}")})
    }
}

/// 批全部成功的累计事件数序列：批 0 = 5 行，之后每批 4 新行 + 1 重放。
/// The cumulative event-count sequence when every batch succeeds: batch 0 = 5
/// rows, each later batch = 4 new rows + 1 replay.
fn expected_count_after(batch: usize) -> i64 {
    (BATCH_SIZE + (BATCH_SIZE - 1) * batch) as i64
}

// A18 核心：server 批量 ingestion 与 CLI list/approve 在同一 WAL 库上并发交错。
// A18 core: server batch ingestion and CLI list/approve interleave concurrently
// on the same WAL database.
#[test]
fn a18_server_ingestion_cli_review_concurrent_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("wiktor.db");

    // 两个独立连接（≈ 两个进程）：server 角色与 CLI 角色，同一文件库。
    // Two independent connections (~two processes): the server role and the CLI
    // role on the same file-backed database.
    let server = Arc::new(SqliteKernel::open(&db).unwrap());
    let cli = Arc::new(SqliteKernel::open(&db).unwrap());

    // 种子：一条 query log（反馈引用锚点）+ 一条 pending query_template 建议
    // （approve 为纯审计转换，不依赖编译 subject 夹具）。
    // Seeds: one query log (the feedback anchor) plus one pending
    // query_template suggestion (approve is an audit-only transition, no
    // compile-subject fixture needed).
    let log_id = server
        .insert_query_log(&QueryLogInsert {
            query_text: "波霸奶茶",
            query_json: "{}",
            rewritten_json: None,
            rewrite_failure: false,
            hit_count: 3,
            latency_ms: 12,
            domain: DOMAIN,
            candidate_empty_initial: false,
            relaxation_attempted: false,
            relaxation_succeeded: false,
        })
        .unwrap();
    let review_id = {
        let store: &dyn FeedbackStore = &*server;
        store
            .insert_review_suggestions(
                DOMAIN,
                &[ReviewSuggestionInput {
                    action: "query_template".to_string(),
                    source_log_ids_json: format!("[{log_id}]"),
                    subject_json: r#"{"normalized_query":"波霸奶茶"}"#.to_string(),
                    reason_json: r#"{"signal":"zero_recall"}"#.to_string(),
                    created_at: 1000,
                }],
            )
            .unwrap()[0]
    };

    let violations: Violations = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let now_base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // —— server 角色：批量 ingestion（post_feedback 步骤 6 的原调用）——
    // —— Server role: batch ingestion (the exact call of post_feedback step 6)
    //       ——
    let s_kernel = server.clone();
    let s_violations = violations.clone();
    let s_barrier = barrier.clone();
    let server_handle = std::thread::spawn(move || {
        let store: &dyn FeedbackStore = &*s_kernel;
        let mut fixed_event_id: Option<i64> = None;
        let mut fixed_received_at: Option<i64> = None;
        s_barrier.wait();
        for batch in 0..N_BATCHES {
            let mut inputs = Vec::with_capacity(BATCH_SIZE);
            inputs.push(input(FIXED_KEY, log_id, fixed_metadata(batch)));
            for slot in 1..BATCH_SIZE {
                inputs.push(input(
                    &format!("sv-{batch}-{slot}"),
                    log_id,
                    serde_json::json!({}),
                ));
            }
            match store.insert_batch_idempotent(&inputs, now_base + batch as i64) {
                Ok(ingested) => {
                    if ingested.len() != BATCH_SIZE {
                        note(
                            &s_violations,
                            "server",
                            format!(
                                "batch {batch}: acked {} events, expected {BATCH_SIZE}",
                                ingested.len()
                            ),
                        );
                        continue;
                    }
                    // 固定键：批 0 新插入并记录原 id/时间；之后必须逐批重放
                    // 原 event_id/received_at（D5：不覆盖、不换 id）。
                    // Fixed key: batch 0 inserts fresh and records the original
                    // id/time; every later batch must replay that exact
                    // event_id/received_at (D5: no overwrite, no id change).
                    let ack = &ingested[0];
                    if batch == 0 {
                        if ack.replayed {
                            note(
                                &s_violations,
                                "server",
                                "batch 0 fixed key must be a fresh insert".into(),
                            );
                        }
                        fixed_event_id = Some(ack.event_id);
                        fixed_received_at = Some(ack.received_at);
                    } else {
                        match (fixed_event_id, fixed_received_at) {
                            (Some(id), Some(at)) => {
                                if !ack.replayed || ack.event_id != id || ack.received_at != at {
                                    note(
                                        &s_violations,
                                        "server",
                                        format!(
                                            "batch {batch}: fixed key replayed={} event_id={} \
                                             (want {id}) received_at={} (want {at})",
                                            ack.replayed, ack.event_id, ack.received_at
                                        ),
                                    );
                                }
                            }
                            _ => note(
                                &s_violations,
                                "server",
                                "fixed key original ack was lost".into(),
                            ),
                        }
                    }
                }
                // busy_timeout 5s 预算内的短事务竞争必须全部成功；任何 Err 都
                // 是收口缺陷（显式错误可接受，但意味着 5s 等待被耗尽——记录）。
                // Short-transaction contention inside the 5s busy_timeout budget
                // must fully succeed; any Err is a closeout defect (an explicit
                // error would be acceptable per spec, but it would mean the 5s
                // wait was exhausted — recorded as a violation).
                Err(e) => note(
                    &s_violations,
                    "server",
                    format!("batch {batch}: ingestion failed explicitly: {e}"),
                ),
            }
        }
    });

    // —— CLI 角色：list_reviews + load_window 读 + approve 循环（feedback
    //    list/review approve 的原调用；approve 每轮都取写锁，与 ingestion 的
    //    BEGIN IMMEDIATE 正面竞争）——
    // —— CLI role: list_reviews + load_window reads + an approve loop (the
    //    exact calls of `feedback list` / `review approve`; every approve takes
    //    the write lock and races the ingestion BEGIN IMMEDIATE head-on) ——
    let c_kernel = cli.clone();
    let c_violations = violations.clone();
    let c_barrier = barrier.clone();
    let cli_handle = std::thread::spawn(move || {
        let store: &dyn FeedbackStore = &*c_kernel;
        let mut approved: Option<i64> = None;
        let mut last_event_count: i64 = 0;
        let window_end = now_base + N_BATCHES as i64 + 10;
        c_barrier.wait();
        for round in 0..N_ROUNDS {
            // 读路径 1：list_reviews（Ok 必须；读不阻塞写、写不阻塞读——WAL）。
            // Read path 1: list_reviews (must be Ok; reads never block writers
            // nor vice versa — WAL).
            if let Err(e) = store.list_reviews(DOMAIN, None, 100) {
                note(
                    &c_violations,
                    "cli",
                    format!("round {round}: list_reviews failed: {e}"),
                );
            }
            // 读路径 2：load_window 一致性快照——脏读检查的两个不变量：
            // (a) 事件数 ∈ {0} ∪ {5+4k}（批原子：部分批永不可见）；
            // (b) 单调不减（快照不会回退）。
            // Read path 2: a load_window consistent snapshot — two dirty-read
            // invariants: (a) the event count is in {0} ∪ {5+4k} (batch
            // atomicity: a partial batch is never visible); (b) monotonic
            // non-decreasing (snapshots never go backwards).
            match store.load_window(DOMAIN, 0, window_end) {
                Ok((_logs, events)) => {
                    let n = events.len() as i64;
                    let on_sequence = n == 0
                        || (n >= BATCH_SIZE as i64
                            && (n - BATCH_SIZE as i64) % (BATCH_SIZE - 1) as i64 == 0);
                    if !on_sequence {
                        note(
                            &c_violations,
                            "cli",
                            format!(
                                "round {round}: saw {n} events, not a whole-batch count (dirty read?)"
                            ),
                        );
                    }
                    if n < last_event_count {
                        note(
                            &c_violations,
                            "cli",
                            format!(
                                "round {round}: event count regressed {last_event_count} -> {n}"
                            ),
                        );
                    }
                    last_event_count = n;
                    // 固定键只要可见，metadata 必须仍是原载荷（无旧值覆盖）。
                    // Whenever the fixed key is visible, its metadata must still
                    // be the original payload (no stale-value overwrite).
                    for ev in &events {
                        if ev.idempotency_key == FIXED_KEY
                            && ev.metadata_json != FIXED_ORIGINAL_METADATA
                        {
                            note(
                                &c_violations,
                                "cli",
                                format!(
                                    "round {round}: fixed key metadata was overwritten: {}",
                                    ev.metadata_json
                                ),
                            );
                        }
                    }
                }
                Err(e) => note(
                    &c_violations,
                    "cli",
                    format!("round {round}: load_window failed: {e}"),
                ),
            }
            // 写路径：approve 每轮尝试。首胜；其后重复审核必须显式
            // Validation Err（状态约束错误不被吞掉、不靠静默重试绕过幂等）。
            // Write path: an approve attempt each round. First one wins;
            // afterwards repeated reviews must fail explicitly with a
            // Validation Err (the state-constraint error is neither swallowed
            // nor silently retried past idempotency).
            match store.approve_review(review_id, "cli-operator", now_base + round as i64) {
                Ok(outcome) => {
                    if approved.replace(outcome.review_id).is_some() {
                        note(
                            &c_violations,
                            "cli",
                            "approve succeeded more than once".into(),
                        );
                    }
                }
                Err(Error::Validation(_)) => {
                    if approved.is_none() {
                        note(
                            &c_violations,
                            "cli",
                            "approve rejected as reviewed before any success (no prior win)".into(),
                        );
                    }
                }
                Err(e) => note(
                    &c_violations,
                    "cli",
                    format!("round {round}: approve failed with a non-state error: {e}"),
                ),
            }
        }
        approved
    });

    server_handle.join().unwrap();
    let approved = cli_handle.join().unwrap();

    // ===== 汇总裁决 =====
    // ===== Final verdict =====
    let violations = violations.lock().unwrap().clone();
    assert!(
        violations.is_empty(),
        "A18 concurrency invariants violated:\n{}",
        violations.join("\n")
    );
    assert_eq!(
        approved,
        Some(review_id),
        "the CLI approve must win exactly once"
    );

    let store: &dyn FeedbackStore = &*server;
    // 幂等键仍唯一：总行数 = 期望累计值，且去重后键数一致。
    // Idempotency keys stay unique: the row total equals the expected
    // cumulative value and the distinct-key count matches.
    let (_logs, events) = store
        .load_window(DOMAIN, 0, now_base + N_BATCHES as i64 + 10)
        .unwrap();
    let expected_total = expected_count_after(N_BATCHES - 1);
    assert_eq!(events.len() as i64, expected_total);
    let mut keys: Vec<&str> = events.iter().map(|e| e.idempotency_key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(
        keys.len(),
        events.len(),
        "idempotency keys must remain unique"
    );

    // 固定键终态 = 原载荷（后续所有变异 metadata 均未落库）。
    // Fixed-key final state = the original payload (none of the later mutated
    // metadata ever landed).
    let fixed = events
        .iter()
        .find(|e| e.idempotency_key == FIXED_KEY)
        .unwrap();
    assert_eq!(fixed.metadata_json, FIXED_ORIGINAL_METADATA);

    // approve 与 ingestion 不交叉产生半状态：approved + 审计字段完整 +
    // query_template 不写 compile_tasks（A16）。
    // No half state from approve/ingestion interleaving: approved + complete
    // audit fields + query_template writes no compile_tasks (A16).
    let approved_items = store
        .list_reviews(DOMAIN, Some(ReviewStatus::Approved), 100)
        .unwrap();
    assert_eq!(approved_items.len(), 1);
    assert_eq!(approved_items[0].review_id, review_id);
    assert_eq!(
        approved_items[0].reviewed_by.as_deref(),
        Some("cli-operator")
    );
    assert!(approved_items[0].reviewed_at.is_some());
    assert_eq!(approved_items[0].compile_task_id, None);

    // 重复 approve（main 线程确定性收尾）：显式 Validation Err。
    // A repeated approve (deterministic from main): an explicit Validation Err.
    let err = store
        .approve_review(review_id, "cli-operator", now_base)
        .unwrap_err();
    assert!(matches!(err, Error::Validation(_)), "got {err:?}");

    // 约束错误显式成 Err（不被吞掉）：非法 kind 命中 CHECK、缺失 log 命中
    // FK RESTRICT。
    // Constraint errors surface as explicit Err (never swallowed): an illegal
    // kind hits the CHECK and a missing log hits the FK RESTRICT.
    let check_err = cli.execute_batch(
        "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, metadata_json, received_at)
         VALUES ('bogus-check', 'milk-tea', 1, 'not-a-kind', '{}', 1)",
    );
    assert!(check_err.is_err(), "CHECK constraint must surface as Err");
    let fk_err = cli.execute_batch(
        "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, metadata_json, received_at)
         VALUES ('bogus-fk', 'milk-tea', 999999, 'click', '{}', 1)",
    );
    assert!(fk_err.is_err(), "FK RESTRICT must surface as Err");

    // 计数面终态一致（row_counts 与窗口读取同一口径）。
    // The counter surface agrees with the window read (one accounting).
    let counts = server.row_counts().unwrap();
    assert_eq!(counts["feedback_events"], expected_total);
    assert_eq!(counts["review_queue"], 1);
}

// A18 / 批7 任务 6：busy_timeout 生效——另一连接持写事务时，写竞争方在 5s
// 预算内**等待**而非立即 SQLITE_BUSY 失败（断言最终成功路径；spec §9 允许的
// 显式 Err 路径无需强制 5s+ 停顿来验证——kernel/store 无重试逻辑，任一路径都
// 不会绕过幂等）。
// A18 / batch-7 task 6: busy_timeout effectiveness — while another connection
// holds a write transaction, the contending writer **waits** within the 5s
// budget instead of failing immediately with SQLITE_BUSY (the eventual-success
// path is asserted; the spec-permitted explicit-Err path is not forced here
// with a 5s+ stall — the kernel/stores carry no retry logic, so neither path
// can bypass idempotency).
#[test]
fn a18_busy_timeout_waits_out_write_contention() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("wiktor.db");
    let holder = SqliteKernel::open(&db).unwrap();
    let contender = SqliteKernel::open(&db).unwrap();

    // 种子 log（竞争方的反馈插入需要合法 log_id）。
    // Seed log (the contender's feedback insert needs a valid log_id).
    let log_id = holder
        .insert_query_log(&QueryLogInsert {
            query_text: "波霸奶茶",
            query_json: "{}",
            rewritten_json: None,
            rewrite_failure: false,
            hit_count: 1,
            latency_ms: 5,
            domain: DOMAIN,
            candidate_empty_initial: false,
            relaxation_attempted: false,
            relaxation_succeeded: false,
        })
        .unwrap();

    // 持锁方：BEGIN IMMEDIATE 拿写锁 → 通知 → 保持 ~700ms → COMMIT。
    // 事务留在连接上（execute_batch 不经 diesel 事务簿记），期间持有方不做
    // 其他内核操作。
    // The lock holder: BEGIN IMMEDIATE takes the write lock → signals → holds
    // for ~700ms → COMMIT. The transaction stays on the connection
    // (execute_batch bypasses diesel's transaction bookkeeping); the holder
    // performs no other kernel work in that window.
    holder
        .execute_batch(
            "BEGIN IMMEDIATE;
             INSERT INTO feedback_rejections (domain, reason, payload_bytes, created_at)
             VALUES ('milk-tea', 'payload_too_large', 1, 1);",
        )
        .unwrap();
    let (lock_held_tx, lock_held_rx) = std::sync::mpsc::channel::<()>();
    let holder_handle = std::thread::spawn(move || {
        lock_held_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(700));
        holder.execute_batch("COMMIT;").unwrap();
    });
    lock_held_rx.recv().unwrap();

    // 竞争方：写锁被占时发起批量插入；必须等到 COMMIT 后成功。
    // The contender: issues a batch insert while the write lock is held; it
    // must only succeed after the COMMIT.
    let started = Instant::now();
    let inputs: Vec<FeedbackEventInput> = (1..=4)
        .map(|i| input(&format!("busy-{i}"), log_id, serde_json::json!({})))
        .collect();
    let store: &dyn FeedbackStore = &contender;
    let outcome = store.insert_batch_idempotent(&inputs, now_base_secs());
    let elapsed = started.elapsed();

    holder_handle.join().unwrap();

    // 最终成功路径：Ok 且耗时 ≥ 持锁时长（证明是等待而非立即失败），
    // 且远在 5s busy 预算之内。
    // The eventual-success path: Ok with an elapsed time of at least the hold
    // duration (proof of waiting, not an immediate failure), and well inside
    // the 5s busy budget.
    let ingested = outcome.unwrap_or_else(|e| panic!("contender must wait and succeed, got: {e}"));
    assert_eq!(ingested.len(), 4);
    assert!(
        elapsed >= Duration::from_millis(600),
        "contender finished in {elapsed:?}; it must have waited for the held write lock \
         (an immediate success would mean no contention was created)"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "contender took {elapsed:?}; the 700ms hold must resolve well within the 5s budget"
    );

    // 等待后终态一致：4 行全部落库。
    // Consistent post-wait state: all 4 rows landed.
    let counts = contender.row_counts().unwrap();
    assert_eq!(counts["feedback_events"], 4);
}

fn now_base_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
