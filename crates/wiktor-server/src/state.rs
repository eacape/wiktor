//! 服务状态（spec `step6-feedback-loop.md` §3 D6/D8、§7、§11 批5）：API keys
//! 解析、时钟注入、健康探针与共享状态句柄。
//! Server state (spec `step6-feedback-loop.md` §3 D6/D8, §7, §11 batch 5): API
//! key parsing, clock injection, the health probe and the shared state handle.
//!
//! ## API keys（D6）
//! ## API keys (D6)
//!
//! 环境变量 `WIKTOR_FEEDBACK_API_KEYS` 为 JSON `{"domain":"secret"}`；启动时
//! 一次解析，空 secret / 重复 domain / 重复 secret / 非法形状 → 启动失败
//! （fail-closed）。key 标签用 BLAKE3 截断 hex，**原文不进标签/日志/指标**
//! （D8）。每个 secret 精确绑定一个 domain（D7：不接受 `*`，不做通配）。
//! The `WIKTOR_FEEDBACK_API_KEYS` env var is JSON `{"domain":"secret"}`; it is
//! parsed once at startup, and an empty secret / duplicate domain / duplicate
//! secret / illegal shape fails startup (fail-closed). Key labels are BLAKE3
//! truncated hex and **raw secrets never enter labels/logs/metrics** (D8). Each
//! secret is bound to exactly one domain (D7: no `*`, no wildcards).

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use wiktor_core::SqliteKernel;

use crate::rate_limit::FixedWindowLimiter;

/// domain 上限（与 core `MAX_DOMAIN_CHARS` 同口径；secret 不设上限，只要求
/// 非空且无控制符）。
/// Domain cap (same contract as core's `MAX_DOMAIN_CHARS`; secrets carry no cap
/// beyond non-empty and control-character free).
const MAX_DOMAIN_CHARS: usize = 128;
/// key 标签长度（BLAKE3 hex 截断前 16 字符 = 64 bit；仅用于限流键与指标，
/// 不承载任何可逆信息）。
/// Key-label length (first 16 hex chars of BLAKE3 = 64 bits; used only for
/// limiter keys and metrics, carrying no reversible information).
const KEY_LABEL_HEX_CHARS: usize = 16;

/// 可注入时钟（D8/A8：限流窗口与 `received_at` 的时间源，测试可拨动）。
/// Injectable clock (D8/A8: the time source for rate-limit windows and
/// `received_at`, adjustable in tests).
pub trait Clock: Send + Sync {
    /// 当前 Unix 秒。
    /// Current Unix seconds.
    fn now_secs(&self) -> i64;
}

/// 生产时钟：系统 UTC 秒。
/// Production clock: system UTC seconds.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// 测试时钟：手拨的 AtomicI64（A8「窗口结束恢复（时间可注入）」）。
/// Test clock: a hand-settable AtomicI64 (A8 "window recovery with an
/// injectable clock").
pub struct MockClock(AtomicI64);

impl MockClock {
    pub fn new(start_secs: i64) -> Arc<Self> {
        Arc::new(MockClock(AtomicI64::new(start_secs)))
    }

    /// 拨动（可前进也可后退；测试窗口推进用加法）。
    /// Sets the time (forward or backward; window-advance tests add deltas).
    pub fn set(&self, secs: i64) {
        self.0.store(secs, Ordering::SeqCst);
    }
}

impl Clock for MockClock {
    fn now_secs(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// 一条 API key 的身份（D6/D7/D8）：允许 domain + BLAKE3 截断标签。
/// One API key's identity (D6/D7/D8): the allowed domain plus the BLAKE3
/// truncated label.
#[derive(Debug, Clone)]
pub struct KeyIdentity {
    pub domain: String,
    pub label: String,
}

/// 静态 key 集（D6）：secret → 身份的精确映射（HashMap 查找即精确匹配）。
/// The static key set (D6): an exact secret → identity map (a HashMap lookup
/// is an exact match).
#[derive(Debug, Clone, Default)]
pub struct ApiKeys {
    by_secret: HashMap<String, KeyIdentity>,
}

/// key 配置错误（启动失败路径；message 不含 secret 原文，只含 domain）。
/// Key-configuration error (the startup-failure path; messages carry no raw
/// secrets, only domains).
#[derive(Debug)]
pub struct ApiKeysError(String);

impl fmt::Display for ApiKeysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid WIKTOR_FEEDBACK_API_KEYS: {}", self.0)
    }
}

impl std::error::Error for ApiKeysError {}

impl ApiKeys {
    /// 从环境变量解析（缺变量/空值 → 错误；启动时一次，D6）。
    /// Parses from the env var (missing/empty → error; once at startup, D6).
    pub fn from_env() -> Result<Self, ApiKeysError> {
        let raw = std::env::var("WIKTOR_FEEDBACK_API_KEYS").unwrap_or_default();
        Self::parse(&raw)
    }

    /// 解析 key 配置（fail-closed）：
    /// - 必须是 JSON 对象（其余形状 → 非法）；
    /// - domain 重复 → 非法（serde_json::Map 会静默去重，故用 visitor 原文收集）；
    /// - secret 重复 → 非法（同一 secret 绑定多 domain 会使租户映射二义）；
    /// - secret 非空且无控制符；domain 1..=128 Unicode scalar。
    ///
    /// Parses the key configuration (fail-closed):
    /// - must be a JSON object (any other shape is illegal);
    /// - duplicate domains are illegal (serde_json::Map would silently dedup,
    ///   so a visitor collects raw entries);
    /// - duplicate secrets are illegal (one secret across domains makes the
    ///   tenant mapping ambiguous);
    /// - secrets are non-empty and control-character free; domains are
    ///   1..=128 Unicode scalars.
    pub fn parse(raw: &str) -> Result<Self, ApiKeysError> {
        if raw.trim().is_empty() {
            return Err(ApiKeysError(
                "missing or empty (expected JSON {\"domain\":\"secret\"})".into(),
            ));
        }
        let entries = parse_key_entries(raw).map_err(|e| ApiKeysError(e.to_string()))?;
        if entries.is_empty() {
            return Err(ApiKeysError("must define at least one key".into()));
        }
        let mut by_secret = HashMap::new();
        for (domain, secret) in entries {
            let domain_chars = domain.chars().count();
            if domain_chars == 0 || domain_chars > MAX_DOMAIN_CHARS {
                return Err(ApiKeysError(format!(
                    "domain must be 1..={MAX_DOMAIN_CHARS} chars"
                )));
            }
            if secret.is_empty() || secret.chars().any(char::is_control) {
                return Err(ApiKeysError(format!(
                    "secret for domain {domain:?} must be non-empty without control characters"
                )));
            }
            if by_secret.contains_key(&secret) {
                return Err(ApiKeysError(format!(
                    "duplicate secret is not allowed (also bound to another domain, \
                     conflicts with {domain:?})"
                )));
            }
            let label = key_label(&secret);
            by_secret.insert(secret, KeyIdentity { label, domain });
        }
        Ok(ApiKeys { by_secret })
    }

    /// 精确匹配 secret（未命中 → None，调用方回 401，不区分缺失与错误）。
    /// Exact-matches the secret (a miss → None; callers answer 401 without
    /// distinguishing missing vs wrong).
    pub fn lookup(&self, secret: &str) -> Option<&KeyIdentity> {
        self.by_secret.get(secret)
    }
}

/// BLAKE3 截断 hex 标签（D8：原文不进标签/日志/指标）。
/// The BLAKE3 truncated hex label (D8: raw secrets never enter labels/logs/
/// metrics).
pub fn key_label(secret: &str) -> String {
    let hex = blake3::hash(secret.as_bytes()).to_hex();
    hex.as_str()[..KEY_LABEL_HEX_CHARS].to_string()
}

/// 用 serde visitor 按原文收集键值对（Map 直接反序列化会静默去重重复 domain，
/// fail-closed 要求显式拒绝）。
/// Collects raw entries via a serde visitor (deserializing into a Map silently
/// dedups duplicate domains; fail-closed requires explicit rejection).
fn parse_key_entries(raw: &str) -> Result<Vec<(String, String)>, serde_json::Error> {
    struct EntriesVisitor;
    impl<'de> serde::de::Visitor<'de> for EntriesVisitor {
        type Value = Vec<(String, String)>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "a JSON object of domain to secret")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut access: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(domain) = access.next_key::<String>()? {
                if out.iter().any(|(d, _)| d == &domain) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate domain {domain:?}"
                    )));
                }
                let secret: String = access.next_value()?;
                out.push((domain, secret));
            }
            Ok(out)
        }
    }
    let mut de = serde_json::Deserializer::from_str(raw);
    // `deserialize_map` 是 serde trait 方法，需经 trait 显式调用。
    // `deserialize_map` is a serde trait method, called through the trait
    // explicitly.
    serde::de::Deserializer::deserialize_map(&mut de, EntriesVisitor)
}

/// 健康探针（A17）：SQLite 可用 → Ok；任何错误 → 503。
/// Health probe (A17): SQLite available → Ok; any error → 503.
pub trait HealthCheck: Send + Sync {
    fn check(&self) -> Result<(), wiktor_core::types::error::Error>;
}

/// 内核健康探针：读 schema 版本即验证连接与迁移账本可达。
/// The kernel health probe: reading the schema version proves the connection
/// and the migration ledger are reachable.
pub struct KernelHealthCheck {
    pub kernel: Arc<SqliteKernel>,
}

impl HealthCheck for KernelHealthCheck {
    fn check(&self) -> Result<(), wiktor_core::types::error::Error> {
        self.kernel.schema_version().map(|_| ())
    }
}

/// 共享服务状态（handler/middleware 经 `Arc<ServerState>` 取用；DB 句柄是
/// kernel，反馈方法走 `FeedbackStore` trait，见 `crate::lib` 的 handler）。
/// The shared server state (used by handlers/middleware through
/// `Arc<ServerState>`; the DB handle is the kernel and feedback methods go
/// through the `FeedbackStore` trait, see the handlers in `crate::lib`).
pub struct ServerState {
    pub kernel: Arc<SqliteKernel>,
    pub keys: ApiKeys,
    pub limiter: FixedWindowLimiter,
    pub metrics: Arc<crate::metrics::Metrics>,
    pub clock: Arc<dyn Clock>,
    pub health: Arc<dyn HealthCheck>,
}

impl ServerState {
    /// 生产构造（D8 默认：60s 窗口 120 次；系统时钟；内核健康探针）。
    /// Production constructor (D8 defaults: 120 requests per 60s window; system
    /// clock; kernel health probe).
    pub fn new(kernel: Arc<SqliteKernel>, keys: ApiKeys) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let health = Arc::new(KernelHealthCheck {
            kernel: kernel.clone(),
        });
        Self::with_parts(kernel, keys, clock, health)
    }

    /// 全量构造（测试注入 MockClock / 失败健康探针，A8/A17）。
    /// Full constructor (tests inject a MockClock / a failing health probe,
    /// A8/A17).
    pub fn with_parts(
        kernel: Arc<SqliteKernel>,
        keys: ApiKeys,
        clock: Arc<dyn Clock>,
        health: Arc<dyn HealthCheck>,
    ) -> Self {
        ServerState {
            kernel,
            keys,
            limiter: FixedWindowLimiter::new(clock.clone(), 60, 120),
            metrics: Arc::new(crate::metrics::Metrics::default()),
            clock,
            health,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // D6：正常解析 + 标签与 secret 原文解耦（标签是 hex、不含原文）。
    // D6: normal parsing + labels decoupled from raw secrets (a label is hex,
    // never the secret).
    #[test]
    fn parses_valid_config_and_labels_by_blake3() {
        let keys = ApiKeys::parse(r#"{"milk-tea":"s3cret-milk-tea"}"#).unwrap();
        let identity = keys.lookup("s3cret-milk-tea").unwrap();
        assert_eq!(identity.domain, "milk-tea");
        assert_eq!(identity.label.len(), KEY_LABEL_HEX_CHARS);
        assert!(!identity.label.contains("s3cret"));
        assert!(keys.lookup("wrong").is_none());
    }

    // D6：空/非法/重复 domain/重复 secret 全部启动失败。
    // D6: empty / illegal / duplicate domain / duplicate secret all fail
    // startup.
    #[test]
    fn rejects_empty_illegal_and_duplicate_configs() {
        assert!(ApiKeys::parse("").is_err());
        assert!(ApiKeys::parse("   ").is_err());
        assert!(ApiKeys::parse("[]").is_err());
        assert!(ApiKeys::parse(r#"{"milk-tea":42}"#).is_err());
        assert!(ApiKeys::parse(r#"{"milk-tea":""}"#).is_err());
        assert!(ApiKeys::parse(r#"{"":"x"}"#).is_err());
        // 同一 domain 出现两次：visitor 必须显式拒绝而非静默去重。
        // The same domain twice: the visitor must reject explicitly instead of
        // silently deduping.
        assert!(ApiKeys::parse(r#"{"milk-tea":"a","milk-tea":"b"}"#).is_err());
        // 同一 secret 绑定两个 domain：租户映射二义，拒绝。
        // One secret bound to two domains: ambiguous tenant mapping, rejected.
        assert!(ApiKeys::parse(r#"{"a":"same","b":"same"}"#).is_err());
        // secret 含控制符：拒绝。
        // Secrets with control characters: rejected.
        assert!(ApiKeys::parse("{\"a\":\"x\\u0000y\"}").is_err());
        assert!(ApiKeys::parse("{\"a\":\"x\\ny\"}").is_err());
    }

    // MockClock 拨动生效（A8 时间注入前提）。
    // MockClock setting takes effect (the A8 time-injection premise).
    #[test]
    fn mock_clock_is_settable() {
        let clock = MockClock::new(3600);
        assert_eq!(clock.now_secs(), 3600);
        clock.set(3661);
        assert_eq!(clock.now_secs(), 3661);
    }
}
