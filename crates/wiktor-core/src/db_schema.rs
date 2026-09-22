//! diesel 表定义（`table!` 宏）。
//! diesel table definitions (the `table!` macro).
//!
//! 与 `migrations/` 的 DDL 一一对应；FTS5 虚拟表 `pages_fts` 不在此列——
//! Corresponds one-to-one with the DDL in `migrations/`; the FTS5 virtual table `pages_fts` is omitted.
//! 它只被 raw SQL（`SqliteKernel::search`）使用，diesel 无法也不应该表达
//! it is used only by raw SQL (`SqliteKernel::search`), which diesel cannot and should not express.
//! FTS5 虚拟表。这里覆盖 ORM 层使用的实体表：pages / page_sections /
//! FTS5 virtual tables. This covers ORM-layer entity tables: pages / page_sections /
//! page_quality / facts / fact_refs，以及 Step 4 编译管线的
//! page_quality / facts / fact_refs, plus the Step 4 compile-pipeline tables:
//! compile_tasks / compile_source_heads / compile_attempts / qug_edges /
//! compile_runs / compile_daily_budget。

diesel::table! {
    pages (page_id) {
        page_id -> Text,
        entity_id -> Text,
        domain -> Text,
        entity_type -> Text,
        title -> Text,
        content -> Text,
        content_hash -> Text,
        generation -> BigInt,
        status -> Text,
        domain_pack_version -> Text,
        compiled_at -> BigInt,
        model_version -> Text,
        embedding_model -> Text,
        created_at -> BigInt,
        updated_at -> BigInt,
        // 0003 新增列（0=legacy / 产物版本 / frontmatter 载荷）。
        // Columns added in 0003 (0=legacy / artifact version / frontmatter payload).
        source_revision -> BigInt,
        artifact_version -> Text,
        frontmatter_json -> Text,
    }
}

diesel::table! {
    page_sections (section_id) {
        section_id -> Text,
        page_id -> Text,
        heading -> Text,
        content -> Text,
        section_index -> BigInt,
    }
}

diesel::table! {
    page_quality (page_id) {
        page_id -> Text,
        coverage -> Double,
        citation -> Double,
        schema_compliance -> Double,
        density -> Double,
        consistency -> Nullable<Double>,
        overall -> Double,
    }
}

diesel::table! {
    facts (entity_id, field_name) {
        entity_id -> Text,
        field_name -> Text,
        field_type -> Text,
        value_numeric -> Nullable<Double>,
        value_text -> Nullable<Text>,
        value_boolean -> Nullable<BigInt>,
        value_timestamp -> Nullable<BigInt>,
        source_revision -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    fact_refs (entity_id, field_name, ref_value) {
        entity_id -> Text,
        field_name -> Text,
        ref_value -> Text,
    }
}

// ===== Step 4 编译管线表（migrations/0003_compile_pipeline）=====
// ===== Step 4 compile-pipeline tables (migrations/0003_compile_pipeline) =====

diesel::table! {
    compile_tasks (task_id) {
        task_id -> BigInt,
        entity_id -> Text,
        source_revision -> BigInt,
        domain_pack_version -> Text,
        status -> Text,
        retry_count -> BigInt,
        max_retries -> BigInt,
        lease_expires_at -> Nullable<BigInt>,
        error_message -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
        // 0003 新增列：全依赖哈希 / epoch / 快照 / 计数 / 租约 / 预算。
        // Columns added in 0003: all-dependency hash / epoch / snapshots / counters /
        // lease / budget.
        desired_hash -> Text,
        epoch -> BigInt,
        source_json -> Text,
        dependencies_json -> Text,
        snapshot_hash -> Text,
        recompile_count -> BigInt,
        attempt_count -> BigInt,
        lease_token -> Nullable<Text>,
        next_attempt_at -> BigInt,
        result -> Nullable<Text>,
        reserved_tokens -> BigInt,
        task_token_budget -> BigInt,
    }
}

diesel::table! {
    compile_source_heads (entity_id) {
        entity_id -> Text,
        source_revision -> BigInt,
        snapshot_hash -> Text,
        desired_hash -> Text,
        task_id -> BigInt,
        epoch -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    // 复合主键 (task_id, epoch, attempt_no)；epoch/attempt_no 在 DDL 中可为 NULL，
    // Composite primary key (task_id, epoch, attempt_no); epoch/attempt_no are
    // nullable in the DDL.
    // 但本 crate 的写入路径恒提供非空值。
    // but every write path in this crate always supplies non-null values.
    compile_attempts (task_id, epoch, attempt_no) {
        task_id -> BigInt,
        epoch -> Nullable<BigInt>,
        attempt_no -> Nullable<BigInt>,
        lease_token -> Text,
        status -> Text,
        publish_status -> Nullable<Text>,
        run_id -> Nullable<Text>,
        utc_day -> Nullable<BigInt>,
        artifact_json -> Nullable<Text>,
        quality_json -> Nullable<Text>,
        issues_json -> Text,
        reserved_tokens -> BigInt,
        reported_tokens -> Nullable<BigInt>,
        error_code -> Nullable<Text>,
        created_at -> BigInt,
        finished_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    qug_edges (page_id, edge_hash) {
        page_id -> Text,
        edge_hash -> Nullable<Text>,
        edge_json -> Text,
        generation -> BigInt,
        content_hash -> Text,
    }
}

diesel::table! {
    compile_runs (run_id) {
        run_id -> Text,
        token_limit -> BigInt,
        reserved_tokens -> BigInt,
        created_at -> BigInt,
    }
}

diesel::table! {
    compile_daily_budget (utc_day) {
        utc_day -> BigInt,
        token_limit -> BigInt,
        reserved_tokens -> BigInt,
    }
}

diesel::allow_tables_to_appear_in_same_query!(
    pages,
    page_sections,
    page_quality,
    facts,
    fact_refs,
    compile_tasks,
    compile_source_heads,
    compile_attempts,
    qug_edges,
    compile_runs,
    compile_daily_budget
);
