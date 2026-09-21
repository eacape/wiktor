//! 知识平面查询辅助（页面发布状态机相关）。
//! Knowledge-plane query helpers (related to the page publish state machine).
//! DDL 见 `migrations/0001_create_core/up.sql`。
//! DDL lives in `migrations/0001_create_core/up.sql`.

/// 将发布状态写入 pages.status 的 SQL 常量（供 kernel 复用）。
/// SQL constant that writes publish status to pages.status (reused by the kernel).
/// 注：发布状态机属后续步骤，当前阶段允许 dead_code。
/// Note: the publish state machine is a later step; dead_code is allowed at this stage.
#[allow(dead_code)]
pub const SQL_UPDATE_STATUS: &str =
    "UPDATE pages SET status = ?2, updated_at = ?3 WHERE page_id = ?1";
