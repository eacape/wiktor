//! 服务状态（spec `step6-feedback-loop.md` §3 D6/D8、§7、§11 批5；Step 7
//! 落地依赖 #7 spec `step7-server-grpc.md` §3 D2/D3）：API keys 解析（方法级
//! 授权）、时钟注入、健康探针与共享状态句柄。
//! Server state (spec `step6-feedback-loop.md` §3 D6/D8, §7, §11 batch 5; Step 7
//! dependency #7 spec `step7-server-grpc.md` §3 D2/D3): API key parsing (with
//! method-level authorization), clock injection, the health probe and the
//! shared state handle.
//!
//! ## API keys（D6 + Step7 D2）
//! ## API keys (D6 + Step7 D2)
//!
//! 环境变量 `WIKTOR_API_KEYS`（新，优先）为 JSON
//! `{"domain":{"secret":"...","methods":["search","compile",...]}}`；旧
//! `WIKTOR_FEEDBACK_API_KEYS`（`{"domain":"secret"}`）仅作为兼容输入，旧 key
//! 默认只授权 `feedback`。两变量同时存在时用新变量；新变量解析失败直接启动
//! 失败。空 secret / 重复 domain / 重复 secret / 未知或重复 methods / 非法
//! 形状 → 启动失败（fail-closed）。key 标签用 BLAKE3 截断 hex，**原文不进
//! 标签/日志/指标**（D8）。每个 secret 精确绑定一个 domain（D7：不接受 `*`，
//! 不做通配）。
//! The `WIKTOR_API_KEYS` env var (new, preferred) is JSON
//! `{"domain":{"secret":"...","methods":["search","compile",...]}}`; the legacy
//! `WIKTOR_FEEDBACK_API_KEYS` (`{"domain":"secret"}`) is accepted as a
//! compatibility input whose keys only get `feedback`. When both exist the new
//! variable wins; a new-variable parse failure fails startup. Empty secrets /
//! duplicate domains / duplicate secrets / unknown or duplicate methods / an
//! illegal shape all fail startup (fail-closed). Key labels are BLAKE3
//! truncated hex and **raw secrets never enter labels/logs/metrics** (D8). Each
//! secret is bound to exactly one domain (D7: no `*`, no wildcards).

use std::collections::{BTreeSet, HashMap};
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

/// 方法权限白名单（Step7 spec §3.1 方法权限映射；新增权限必须同步 proto 映射
/// 表、测试与英文 spec，STEP7-008）。
/// The method-permission allowlist (Step7 spec §3.1 method-permission mapping;
/// a new permission must sync the proto mapping table, tests and the English
/// spec, STEP7-008).
pub const PERMITTED_METHODS: &[&str] = &[
    "search",
    "compile",
    "qug_build",
    "review",
    "compatibility",
    "status",
    "feedback",
];

/// 旧 `WIKTOR_FEEDBACK_API_KEYS` 的默认权限（仅 feedback，STEP7-004）。
/// The default permission of a legacy `WIKTOR_FEEDBACK_API_KEYS` key (feedback
/// only, STEP7-004).
pub const LEGACY_METHODS: &[&str] = &["feedback"];

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

/// 一条 API key 的身份（D6/D7/D8 + Step7 D2）：允许 domain + BLAKE3 截断标签
/// + 方法权限集。
///
/// One API key's identity (D6/D7/D8 + Step7 D2): the allowed domain plus the
/// BLAKE3 truncated label plus the method-permission set.
#[derive(Debug, Clone)]
pub struct KeyIdentity {
    pub domain: String,
    pub label: String,
    /// 授权的方法权限名（Step7 §3.1；空集 = 无任何方法权限，fail-closed）。
    /// The authorized method permissions (Step7 §3.1; an empty set authorizes
    /// nothing, fail-closed).
    pub methods: BTreeSet<String>,
}

impl KeyIdentity {
    /// 方法授权判定（D3：HTTP middleware 与 gRPC interceptor 共用同一语义）。
    /// Method-authorization check (D3: the same semantics shared by the HTTP
    /// middleware and the gRPC interceptor).
    pub fn authorize(&self, method: &str) -> bool {
        self.methods.contains(method)
    }
}

/// 静态 key 集（D6 + Step7 D2）：secret → 身份的精确映射（HashMap 查找即精确
/// 匹配）。
/// The static key set (D6 + Step7 D2): an exact secret → identity map (a
/// HashMap lookup is an exact match).
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
    /// 从环境变量解析（Step7 D2：`WIKTOR_API_KEYS` 优先；缺失时回退旧
    /// `WIKTOR_FEEDBACK_API_KEYS`；两者皆缺/空值 → 错误；启动时一次）。
    /// Parses from the env (Step7 D2: `WIKTOR_API_KEYS` wins; falls back to the
    /// legacy `WIKTOR_FEEDBACK_API_KEYS`; both missing/empty → error; once at
    /// startup).
    pub fn from_env() -> Result<Self, ApiKeysError> {
        let new = std::env::var("WIKTOR_API_KEYS").ok();
        match new {
            Some(raw) if !raw.trim().is_empty() => Self::parse(&raw),
            _ => {
                let legacy = std::env::var("WIKTOR_FEEDBACK_API_KEYS").unwrap_or_default();
                Self::parse_legacy(&legacy)
            }
        }
    }

    /// 解析新格式 key 配置（fail-closed）：
    /// - 必须是 JSON 对象（其余形状 → 非法）；
    /// - domain 重复 → 非法；secret 重复 → 非法；
    /// - secret 非空且无控制符；domain 1..=128 Unicode scalar；
    /// - methods 非空、只允许 [`PERMITTED_METHODS`]、无重复。
    ///
    /// Parses the new-format key configuration (fail-closed):
    /// - must be a JSON object (any other shape is illegal);
    /// - duplicate domains and duplicate secrets are illegal;
    /// - secrets are non-empty and control-character free; domains are
    ///   1..=128 Unicode scalars;
    /// - methods are non-empty, restricted to [`PERMITTED_METHODS`], and unique.
    pub fn parse(raw: &str) -> Result<Self, ApiKeysError> {
        if raw.trim().is_empty() {
            return Err(ApiKeysError(
                "missing or empty (expected JSON {\"domain\":{\"secret\":\"...\",\"methods\":[...]}})"
                    .into(),
            ));
        }
        let entries = parse_key_entries(raw).map_err(|e| ApiKeysError(e.to_string()))?;
        Self::build(entries)
    }

    /// 解析旧格式（`{"domain":"secret"}`），所有 key 只获得 `feedback`
    /// （STEP7-004）。
    /// Parses the legacy format (`{"domain":"secret"}`); every key only gets
    /// `feedback` (STEP7-004).
    pub fn parse_legacy(raw: &str) -> Result<Self, ApiKeysError> {
        let entries = parse_legacy_entries(raw).map_err(|e| ApiKeysError(e.to_string()))?;
        let with_methods = entries
            .into_iter()
            .map(|(d, s)| (d, s, LEGACY_METHODS.iter().map(|m| m.to_string()).collect()))
            .collect();
        Self::build(with_methods)
    }

    /// 共享构建：domain/secret/methods 校验 + secret→身份映射。
    /// Shared build: domain/secret/methods validation plus the secret→identity
    /// map.
    fn build(entries: Vec<(String, String, BTreeSet<String>)>) -> Result<Self, ApiKeysError> {
        if entries.is_empty() {
            return Err(ApiKeysError("must define at least one key".into()));
        }
        let mut by_secret = HashMap::new();
        for (domain, secret, methods) in entries {
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
            by_secret.insert(
                secret,
                KeyIdentity {
                    label,
                    domain,
                    methods,
                },
            );
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

/// 用 serde visitor 按原文收集新格式条目（value 为对象 `{"secret","methods"}`；
/// Map 直接反序列化会静默去重重复 domain，fail-closed 要求显式拒绝）。
/// Collects new-format entries via a serde visitor (each value is an object
/// `{"secret","methods"}`; deserializing into a Map silently dedups duplicate
/// domains, so fail-closed requires explicit rejection).
fn parse_key_entries(
    raw: &str,
) -> Result<Vec<(String, String, BTreeSet<String>)>, serde_json::Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct KeyValue {
        secret: String,
        #[serde(default)]
        methods: Vec<String>,
    }
    struct EntriesVisitor;
    impl<'de> serde::de::Visitor<'de> for EntriesVisitor {
        type Value = Vec<(String, String, BTreeSet<String>)>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "a JSON object of domain to a {{secret, methods}} object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut access: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(domain) = access.next_key::<String>()? {
                if out.iter().any(|(d, _, _)| d == &domain) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate domain {domain:?}"
                    )));
                }
                let value: KeyValue = access.next_value()?;
                let raw_methods = value.methods.clone();
                let methods: BTreeSet<String> = value.methods.into_iter().collect();
                if methods.is_empty() {
                    return Err(serde::de::Error::custom(format!(
                        "methods for domain {domain:?} must be non-empty"
                    )));
                }
                for m in &methods {
                    if !PERMITTED_METHODS.contains(&m.as_str()) {
                        return Err(serde::de::Error::custom(format!(
                            "unknown method {m:?} for domain {domain:?} \
                             (allowed: {PERMITTED_METHODS:?})"
                        )));
                    }
                }
                if methods.len() != raw_methods.len() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate method for domain {domain:?}"
                    )));
                }
                out.push((domain, value.secret, methods));
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

/// 用 serde visitor 按原文收集旧格式条目（value 为字符串 secret）。
/// Collects legacy-format entries via a serde visitor (each value is a string
/// secret).
fn parse_legacy_entries(raw: &str) -> Result<Vec<(String, String)>, serde_json::Error> {
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
        let keys = ApiKeys::parse_legacy(r#"{"milk-tea":"s3cret-milk-tea"}"#).unwrap();
        let identity = keys.lookup("s3cret-milk-tea").unwrap();
        assert_eq!(identity.domain, "milk-tea");
        assert_eq!(identity.label.len(), KEY_LABEL_HEX_CHARS);
        assert!(!identity.label.contains("s3cret"));
        assert_eq!(identity.methods, BTreeSet::from(["feedback".to_string()]));
        assert!(keys.lookup("wrong").is_none());
    }

    // Step7 D2/A4：新格式方法级授权解析与 fail-closed。
    // Step7 D2/A4: new-format method-level authorization parsing and
    // fail-closed rules.
    #[test]
    fn parses_method_level_keys_and_authorizes() {
        let keys = ApiKeys::parse(
            r#"{"milk-tea":{"secret":"s1","methods":["search","compile"]},"tea2":{"secret":"s2","methods":["review"]}}"#,
        )
        .unwrap();
        let identity = keys.lookup("s1").unwrap();
        assert_eq!(identity.domain, "milk-tea");
        assert!(identity.authorize("search"));
        assert!(identity.authorize("compile"));
        assert!(!identity.authorize("review"), "review not granted");
        assert!(!identity.authorize("unknown_method"));
        let review = keys.lookup("s2").unwrap();
        assert!(review.authorize("review"));
        assert!(!review.authorize("search"));
    }

    // Step7 D2/A4：新格式 fail-closed——未知/重复/空 methods、非法形状。
    // Step7 D2/A4: new-format fail-closed — unknown/duplicate/empty methods and
    // illegal shapes.
    #[test]
    fn rejects_illegal_method_level_configs() {
        assert!(ApiKeys::parse("").is_err());
        assert!(ApiKeys::parse("[]").is_err());
        assert!(ApiKeys::parse(r#"{"milk-tea":{"secret":"s","methods":[]}}"#).is_err());
        assert!(ApiKeys::parse(r#"{"milk-tea":{"secret":"s","methods":["nope"]}}"#).is_err());
        assert!(
            ApiKeys::parse(r#"{"milk-tea":{"secret":"s","methods":["search","search"]}}"#).is_err(),
            "duplicate methods rejected"
        );
        assert!(ApiKeys::parse(r#"{"milk-tea":{"secret":""}}"#).is_err());
        assert!(ApiKeys::parse(r#"{"milk-tea":42}"#).is_err());
        assert!(
            ApiKeys::parse(r#"{"milk-tea":{"secret":"a"},"milk-tea":{"secret":"b"}}"#).is_err()
        );
        assert!(ApiKeys::parse(r#"{"a":{"secret":"same","methods":["search"]},"b":{"secret":"same","methods":["search"]}}"#).is_err());
    }

    // Step7 D2/A4：旧格式只获得 feedback（STEP7-004）；空/非法/重复 domain/
    // 重复 secret 全部启动失败。
    // Step7 D2/A4: a legacy key only gets feedback (STEP7-004); empty / illegal
    // / duplicate domain / duplicate secret all fail startup.
    #[test]
    fn rejects_empty_illegal_and_duplicate_legacy_configs() {
        assert!(ApiKeys::parse_legacy("").is_err());
        assert!(ApiKeys::parse_legacy("   ").is_err());
        assert!(ApiKeys::parse_legacy("[]").is_err());
        assert!(ApiKeys::parse_legacy(r#"{"milk-tea":42}"#).is_err());
        assert!(ApiKeys::parse_legacy(r#"{"milk-tea":""}"#).is_err());
        assert!(ApiKeys::parse_legacy(r#"{"":"x"}"#).is_err());
        // 同一 domain 出现两次：visitor 必须显式拒绝而非静默去重。
        // The same domain twice: the visitor must reject explicitly instead of
        // silently deduping.
        assert!(ApiKeys::parse_legacy(r#"{"milk-tea":"a","milk-tea":"b"}"#).is_err());
        // 同一 secret 绑定两个 domain：租户映射二义，拒绝。
        // One secret bound to two domains: ambiguous tenant mapping, rejected.
        assert!(ApiKeys::parse_legacy(r#"{"a":"same","b":"same"}"#).is_err());
        // secret 含控制符：拒绝。
        // Secrets with control characters: rejected.
        assert!(ApiKeys::parse_legacy("{\"a\":\"x\\u0000y\"}").is_err());
        assert!(ApiKeys::parse_legacy("{\"a\":\"x\\ny\"}").is_err());
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
