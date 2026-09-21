//! diesel 表定义（`table!` 宏）。
//! diesel table definitions (the `table!` macro).
//!
//! 与 `migrations/` 的 DDL 一一对应；FTS5 虚拟表 `pages_fts` 不在此列——
//! Corresponds one-to-one with the DDL in `migrations/`; the FTS5 virtual table `pages_fts` is omitted.
//! 它只被 raw SQL（`SqliteKernel::search`）使用，diesel 无法也不应该表达
//! it is used only by raw SQL (`SqliteKernel::search`), which diesel cannot and should not express.
//! FTS5 虚拟表。这里覆盖 ORM 层使用的实体表：pages / page_sections /
//! FTS5 virtual tables. This covers ORM-layer entity tables: pages / page_sections /
//! page_quality / facts / fact_refs。
//! page_quality / facts / fact_refs.

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

diesel::allow_tables_to_appear_in_same_query!(pages, page_sections, page_quality, facts, fact_refs);
