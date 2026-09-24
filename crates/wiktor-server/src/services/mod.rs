//! Step 7 六 gRPC service 实现（spec step7 §3 D1/D12，§5 模块边界）。
//! The six Step 7 gRPC service implementations (spec step7 §3 D1/D12, §5
//! module boundary).
//!
//! 依赖面：只依赖 core 公开类型 + 本 crate 的 auth/state/metrics；不依赖 CLI
//! （§5：禁止 server 依赖 CLI）。
//! Dependency surface: only core public types plus this crate's auth/state/
//! metrics; never the CLI (§5: server must not depend on the CLI).

pub mod compatibility;
pub mod compile;
pub mod qug_build;
pub mod review;
pub mod search;
pub mod status;
