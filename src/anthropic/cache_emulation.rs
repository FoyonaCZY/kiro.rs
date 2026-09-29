//! 模拟 Anthropic prompt cache 的计费口径。
//!
//! Kiro 上游没有 prompt cache，发给上游的请求一字不改。这里只按官方缓存规则估算
//! 「这次请求在官方 API 上会命中多少」，写进返回给客户端的 usage 和用量库，
//! 用于向下游按缓存价计费。思路移植自 sub2api 的 kiro_cache_emulation.go：
//!
//! - 请求按 tools → system → messages 拆成内容块，逐块做链式 SHA-256，
//!   每个位置的指纹代表「从头到这里」的整段前缀。
//! - 断点优先用客户端的 cache_control（支持 `ttl: "1h"`）；出现过断点后，
//!   之后每条消息末尾继续补断点。客户端完全没发时，每条消息末尾补 5 分钟断点。
//! - 累计 token 低于模型最低门槛的断点不算。
//! - 从最后一个断点往前最多查 10 个，第一个未过期的指纹就是命中位置。
//! - 上游返回 2xx 之后才调用 [`observe`]，失败请求不会污染后续计算。
//! - 按接入 Key 隔离：多个下游共用同一个 Kiro 凭据时，不会吃到别人的缓存。
//!
//! 状态只在进程内存里，重启后全部冷启动。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::model::config::CacheEmulationConfig;
use crate::token::count_tokens;

const TTL_5M: Duration = Duration::from_secs(5 * 60);
const TTL_1H: Duration = Duration::from_secs(60 * 60);
/// 往回查的断点数上限，与 sub2api 一致。
const LOOKBACK: usize = 10;
/// 单个接入 Key 最多保留的前缀指纹，超出时先淘汰最早过期的。
const MAX_ENTRIES_PER_NAMESPACE: usize = 4096;
/// 图片和文档按固定值计，不按 base64 长度估算。只影响各断点之间的比例。
const MEDIA_TOKENS: u64 = 1600;

static CONFIG: OnceLock<CacheEmulationConfig> = OnceLock::new();
static TRACKER: OnceLock<CacheTracker> = OnceLock::new();

/// 启动时调用一次。未调用或 `enabled=false` 时整个模块不生效，usage 输出与原来一致。
pub fn init(config: CacheEmulationConfig) {
    let _ = CONFIG.set(config);
}

fn config() -> Option<CacheEmulationConfig> {
    #[cfg(test)]
    if let Some(config) = TEST_OVERRIDE.with(|cell| cell.get()) {
        return config.enabled.then_some(config);
    }
    CONFIG.get().copied().filter(|config| config.enabled)
}

#[cfg(test)]
thread_local! {
    static TEST_OVERRIDE: std::cell::Cell<Option<CacheEmulationConfig>> =
        const { std::cell::Cell::new(None) };
}

/// 仅测试：在当前线程临时开启模拟缓存。每个测试跑在独立线程上，互不影响。
#[cfg(test)]
pub(crate) fn enable_for_test(read_ratio: f64, creation_ratio: f64) {
    TEST_OVERRIDE.with(|cell| {
        cell.set(Some(CacheEmulationConfig {
            enabled: true,
            read_ratio,
            creation_ratio,
        }))
    });
}

fn tracker() -> &'static CacheTracker {
    TRACKER.get_or_init(CacheTracker::default)
}

/// 从客户端原始请求体构建前缀画像。未开启或请求不够长时返回 None。
pub fn profile(body: &str) -> Option<CacheProfile> {
    config()?;
    CacheProfile::from_body(body)
}

/// 上游返回 2xx 后调用：按当前接入 Key 查命中，并把这次的前缀记下来。
pub fn observe(profile: &CacheProfile) -> CacheSplit {
    let namespace = match crate::access::current_key_id() {
        Some(id) => format!("key:{id}"),
        None => "default".to_string(),
    };
    tracker().observe(&namespace, profile, Instant::now())
}

/// 客户端看到的 usage。没有命中数据或未开启时，只有 input_tokens/output_tokens。
pub fn usage_json(split: Option<&CacheSplit>, input_tokens: i32, output_tokens: i32) -> Value {
    match (split, config()) {
        (Some(split), Some(config)) => split
            .apply_with(input_tokens, config.read_ratio, config.creation_ratio)
            .to_json(output_tokens),
        _ => json!({ "input_tokens": input_tokens, "output_tokens": output_tokens }),
    }
}

/// 用量库记录的拆分，与 [`usage_json`] 使用同一套计算。
pub fn usage_for_log(split: Option<&CacheSplit>, input_tokens: i32) -> CacheUsage {
    match (split, config()) {
        (Some(split), Some(config)) => {
            split.apply_with(input_tokens, config.read_ratio, config.creation_ratio)
        }
        _ => CacheUsage {
            input_tokens: input_tokens.max(0),
            ..CacheUsage::default()
        },
    }
}

/// 命中结果，按块 token 的占比表示。
///
/// 块 token 是逐块估算的，和最终 input_tokens 的口径不同，所以只保存比例，
/// 等拿到最终 input_tokens（contextUsageEvent 或本地估算）再换算成具体数值。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheSplit {
    pub read: f64,
    pub creation: f64,
    pub one_hour: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheUsage {
    /// 未命中也未写缓存的输入，对应官方 usage.input_tokens。
    pub input_tokens: i32,
    pub cache_read: i32,
    pub cache_write_5m: i32,
    pub cache_write_1h: i32,
}

impl CacheUsage {
    pub fn cache_creation(&self) -> i32 {
        self.cache_write_5m + self.cache_write_1h
    }

    pub fn to_json(&self, output_tokens: i32) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "cache_creation_input_tokens": self.cache_creation(),
            "cache_read_input_tokens": self.cache_read,
            "cache_creation": {
                "ephemeral_5m_input_tokens": self.cache_write_5m,
                "ephemeral_1h_input_tokens": self.cache_write_1h,
            },
            "output_tokens": output_tokens,
        })
    }
}

impl CacheSplit {
    /// 按比例换算成具体 token。ratio 小于 1 时，少报的部分回到 input_tokens，总数不变。
    pub fn apply_with(&self, total: i32, read_ratio: f64, creation_ratio: f64) -> CacheUsage {
        let total = total.max(0);
        let scale = |fraction: f64, ratio: f64| {
            ((total as f64) * fraction.clamp(0.0, 1.0) * ratio.clamp(0.0, 1.0)).round() as i32
        };
        let read = scale(self.read, read_ratio).min(total);
        let creation = scale(self.creation, creation_ratio).min(total - read);
        let (write_5m, write_1h) = if self.one_hour {
            (0, creation)
        } else {
            (creation, 0)
        };
        CacheUsage {
            input_tokens: total - read - creation,
            cache_read: read,
            cache_write_5m: write_5m,
            cache_write_1h: write_1h,
        }
    }
}

/// 一次请求的前缀画像。
#[derive(Debug, Clone)]
pub struct CacheProfile {
    blocks: Vec<Block>,
    /// 只包含达到最低门槛的断点，按位置递增。
    breakpoints: Vec<Breakpoint>,
    total_tokens: u64,
}

#[derive(Debug, Clone)]
struct Block {
    fingerprint: [u8; 32],
    cumulative: u64,
}

#[derive(Debug, Clone, Copy)]
struct Breakpoint {
    index: usize,
    ttl: Duration,
}

struct Pending<'a> {
    value: &'a Value,
    tag: &'static str,
    tokens: u64,
    ttl: Option<Duration>,
    message_end: bool,
}

impl CacheProfile {
    pub fn from_body(body: &str) -> Option<Self> {
        let payload: Value = serde_json::from_str(body).ok()?;
        let model = payload.get("model").and_then(Value::as_str).unwrap_or_default();
        let pending = flatten(&payload);
        if pending.is_empty() {
            return None;
        }
        let client_marked = pending.iter().any(|block| block.ttl.is_some());
        let prelude = json!({ "model": model, "tool_choice": payload.get("tool_choice") });
        let mut state: [u8; 32] = Sha256::digest(canonical(&prelude)).into();

        let last_index = pending.len() - 1;
        let mut blocks = Vec::with_capacity(pending.len());
        let mut marks = Vec::new();
        let mut cumulative = 0u64;
        let mut active: Option<Duration> = None;
        for (index, block) in pending.iter().enumerate() {
            cumulative += block.tokens;
            let mut digest = Sha256::new();
            digest.update(block.tag.as_bytes());
            digest.update([0u8]);
            digest.update(canonical(block.value));
            let block_hash = digest.finalize();
            let mut chain = Sha256::new();
            chain.update(state);
            chain.update(block_hash);
            state = chain.finalize().into();
            blocks.push(Block {
                fingerprint: state,
                cumulative,
            });

            let ttl = if !client_marked {
                (block.message_end || index == last_index).then_some(TTL_5M)
            } else if let Some(ttl) = block.ttl {
                active = Some(ttl);
                Some(ttl)
            } else if block.message_end {
                active
            } else {
                None
            };
            if let Some(ttl) = ttl {
                marks.push(Breakpoint {
                    index,
                    ttl: ttl.min(TTL_1H),
                });
            }
        }

        let minimum = min_cacheable(model);
        let breakpoints: Vec<_> = marks
            .into_iter()
            .filter(|mark| blocks[mark.index].cumulative >= minimum)
            .collect();
        if breakpoints.is_empty() {
            return None;
        }
        Some(Self {
            blocks,
            breakpoints,
            total_tokens: cumulative,
        })
    }
}

/// 官方最低可缓存长度：Opus 4.5 起和 Haiku 4.5 是 4096，其余按 1024。
fn min_cacheable(model: &str) -> u64 {
    let model = model.to_ascii_lowercase();
    let haiku_45 = model.contains("haiku") && (model.contains("4-5") || model.contains("4.5"));
    if model.contains("opus") || haiku_45 { 4096 } else { 1024 }
}

fn flatten(payload: &Value) -> Vec<Pending<'_>> {
    let mut out = Vec::new();
    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        for tool in tools {
            out.push(Pending {
                value: tool,
                tag: "tool",
                tokens: count_tokens(&canonical_string(tool)),
                ttl: ttl_of(tool),
                message_end: false,
            });
        }
    }
    match payload.get("system") {
        Some(Value::Array(items)) => {
            for item in items {
                out.push(Pending {
                    value: item,
                    tag: "system",
                    tokens: block_tokens(item),
                    ttl: ttl_of(item),
                    message_end: false,
                });
            }
        }
        Some(text @ Value::String(value)) if !value.is_empty() => out.push(Pending {
            value: text,
            tag: "system",
            tokens: count_tokens(value),
            ttl: None,
            message_end: false,
        }),
        _ => {}
    }
    let messages = payload.get("messages").and_then(Value::as_array);
    for message in messages.into_iter().flatten() {
        let tag = if message.get("role").and_then(Value::as_str) == Some("assistant") {
            "assistant"
        } else {
            "user"
        };
        match message.get("content") {
            Some(text @ Value::String(value)) => out.push(Pending {
                value: text,
                tag,
                tokens: count_tokens(value),
                ttl: None,
                message_end: true,
            }),
            Some(Value::Array(items)) => {
                let last = items.len().saturating_sub(1);
                for (index, item) in items.iter().enumerate() {
                    out.push(Pending {
                        value: item,
                        tag,
                        tokens: block_tokens(item),
                        ttl: ttl_of(item),
                        message_end: index == last,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn block_tokens(block: &Value) -> u64 {
    let field = |key: &str| block.get(key).and_then(Value::as_str).map(count_tokens);
    match block.get("type").and_then(Value::as_str) {
        Some("text") => field("text").unwrap_or(0),
        Some("thinking") => field("thinking").unwrap_or(0),
        Some("image") | Some("document") => MEDIA_TOKENS,
        Some("tool_result") => match block.get("content") {
            Some(Value::String(text)) => count_tokens(text),
            Some(Value::Array(items)) => items.iter().map(block_tokens).sum(),
            _ => 0,
        },
        Some("tool_use") => {
            let input = block.get("input").unwrap_or(&Value::Null);
            count_tokens(&canonical_string(input)) + field("name").unwrap_or(0)
        }
        _ => count_tokens(&canonical_string(block)),
    }
}

fn ttl_of(block: &Value) -> Option<Duration> {
    let control = block.get("cache_control")?.as_object()?;
    Some(if control.get("ttl").and_then(Value::as_str) == Some("1h") {
        TTL_1H
    } else {
        TTL_5M
    })
}

/// 键排序、去掉 cache_control 后的 JSON，保证同一内容总是得到同一指纹。
fn canonical(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

fn canonical_string(value: &Value) -> String {
    String::from_utf8(canonical(value)).unwrap_or_default()
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> =
                map.keys().filter(|key| key.as_str() != "cache_control").collect();
            keys.sort();
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(key).unwrap_or_default());
                out.push(b':');
                write_canonical(&map[key.as_str()], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        other => out.extend(serde_json::to_vec(other).unwrap_or_default()),
    }
}

#[derive(Default)]
struct CacheTracker {
    entries: Mutex<HashMap<String, HashMap<[u8; 32], Entry>>>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    ttl: Duration,
    expires_at: Instant,
}

impl CacheTracker {
    /// 查命中并记录本次前缀。两步在同一把锁里完成，并发请求不会互相覆盖。
    fn observe(&self, namespace: &str, profile: &CacheProfile, now: Instant) -> CacheSplit {
        let mut all = self.entries.lock();
        all.retain(|_, entries| {
            entries.retain(|_, entry| entry.expires_at > now);
            !entries.is_empty()
        });
        let entries = all.entry(namespace.to_string()).or_default();

        let mut matched = 0u64;
        for breakpoint in profile.breakpoints.iter().rev().take(LOOKBACK) {
            let block = &profile.blocks[breakpoint.index];
            if let Some(entry) = entries.get_mut(&block.fingerprint) {
                entry.expires_at = now + entry.ttl;
                matched = block.cumulative;
                break;
            }
        }

        let Some(last) = profile.breakpoints.last().copied() else {
            return CacheSplit::default();
        };
        let cached_to = profile.blocks[last.index].cumulative;
        for breakpoint in &profile.breakpoints {
            let fingerprint = profile.blocks[breakpoint.index].fingerprint;
            let entry = entries.entry(fingerprint).or_insert(Entry {
                ttl: breakpoint.ttl,
                expires_at: now,
            });
            entry.ttl = entry.ttl.max(breakpoint.ttl);
            entry.expires_at = entry.expires_at.max(now + breakpoint.ttl);
        }

        if entries.len() > MAX_ENTRIES_PER_NAMESPACE {
            let excess = entries.len() - MAX_ENTRIES_PER_NAMESPACE;
            let mut by_expiry: Vec<_> = entries
                .iter()
                .map(|(fingerprint, entry)| (entry.expires_at, *fingerprint))
                .collect();
            by_expiry.sort();
            for (_, fingerprint) in by_expiry.into_iter().take(excess) {
                entries.remove(&fingerprint);
            }
        }

        let total = profile.total_tokens.max(1) as f64;
        CacheSplit {
            read: matched as f64 / total,
            creation: cached_to.saturating_sub(matched) as f64 / total,
            one_hour: last.ttl >= TTL_1H,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_system() -> String {
        "cache ".repeat(3000)
    }

    /// 第一轮：system + 一条 user
    fn turn_one(system: Value) -> String {
        json!({
            "model": "claude-opus-5-5",
            "system": system,
            "messages": [{ "role": "user", "content": "hi" }],
        })
        .to_string()
    }

    /// 第二轮：在第一轮基础上追加 assistant + user
    fn turn_two(system: Value) -> String {
        json!({
            "model": "claude-opus-5-5",
            "system": system,
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "hello" },
                { "role": "user", "content": [{ "type": "text", "text": "next question" }] },
            ],
        })
        .to_string()
    }

    fn plain_system() -> Value {
        json!(long_system())
    }

    #[test]
    fn first_request_is_all_creation_second_reads_prefix() {
        let tracker = CacheTracker::default();
        let now = Instant::now();

        let first = CacheProfile::from_body(&turn_one(plain_system())).unwrap();
        let split = tracker.observe("k", &first, now);
        assert_eq!(split.read, 0.0);
        assert!(split.creation > 0.99, "{split:?}");
        assert!(!split.one_hour);

        let second = CacheProfile::from_body(&turn_two(plain_system())).unwrap();
        let split = tracker.observe("k", &second, now + Duration::from_secs(30));
        assert!(split.read > 0.95, "earlier prefix should hit: {split:?}");
        assert!(split.creation > 0.0 && split.creation < 0.05, "{split:?}");
        assert!((split.read + split.creation - 1.0).abs() < 1e-9);
    }

    #[test]
    fn identical_request_again_is_a_full_read() {
        let tracker = CacheTracker::default();
        let now = Instant::now();
        let profile = CacheProfile::from_body(&turn_one(plain_system())).unwrap();
        tracker.observe("k", &profile, now);
        let split = tracker.observe("k", &profile, now + Duration::from_secs(10));
        assert!((split.read - 1.0).abs() < 1e-9, "{split:?}");
        assert_eq!(split.creation, 0.0);
    }

    #[test]
    fn five_minute_entries_expire() {
        let tracker = CacheTracker::default();
        let now = Instant::now();
        let profile = CacheProfile::from_body(&turn_one(plain_system())).unwrap();
        tracker.observe("k", &profile, now);
        let split = tracker.observe("k", &profile, now + Duration::from_secs(6 * 60));
        assert_eq!(split.read, 0.0, "expired after 5 minutes");
    }

    #[test]
    fn hit_refreshes_ttl() {
        let tracker = CacheTracker::default();
        let now = Instant::now();
        let profile = CacheProfile::from_body(&turn_one(plain_system())).unwrap();
        tracker.observe("k", &profile, now);
        tracker.observe("k", &profile, now + Duration::from_secs(4 * 60));
        let split = tracker.observe("k", &profile, now + Duration::from_secs(8 * 60));
        assert!(split.read > 0.99, "renewed at 4 min, still alive at 8 min: {split:?}");
    }

    #[test]
    fn below_model_minimum_is_not_cacheable() {
        let body = json!({
            "model": "claude-opus-5-5",
            "system": "short",
            "messages": [{ "role": "user", "content": "hi" }],
        })
        .to_string();
        assert!(CacheProfile::from_body(&body).is_none());
        // sonnet 门槛 1024，约 1500 token 可以缓存；opus 门槛 4096 不行
        let medium = "cache ".repeat(1000);
        let sonnet = json!({
            "model": "claude-sonnet-4-6",
            "system": medium,
            "messages": [{ "role": "user", "content": "hi" }],
        });
        assert!(CacheProfile::from_body(&sonnet.to_string()).is_some());
        let mut opus = sonnet;
        opus["model"] = json!("claude-opus-5-5");
        assert!(CacheProfile::from_body(&opus.to_string()).is_none());
    }

    #[test]
    fn explicit_one_hour_breakpoint_propagates_to_message_ends() {
        let system = json!([{
            "type": "text",
            "text": long_system(),
            "cache_control": { "type": "ephemeral", "ttl": "1h" },
        }]);
        let tracker = CacheTracker::default();
        let now = Instant::now();
        let first = CacheProfile::from_body(&turn_one(system.clone())).unwrap();
        let split = tracker.observe("k", &first, now);
        assert!(split.one_hour);
        // 1h 断点在 30 分钟后仍然有效，第二轮能读到
        let second = CacheProfile::from_body(&turn_two(system)).unwrap();
        let split = tracker.observe("k", &second, now + Duration::from_secs(30 * 60));
        assert!(split.read > 0.95, "{split:?}");
    }

    #[test]
    fn cache_control_marks_do_not_change_the_fingerprint() {
        let marked = json!([{
            "type": "text",
            "text": long_system(),
            "cache_control": { "type": "ephemeral" },
        }]);
        let plain = json!([{ "type": "text", "text": long_system() }]);
        let tracker = CacheTracker::default();
        let now = Instant::now();
        tracker.observe("k", &CacheProfile::from_body(&turn_one(marked)).unwrap(), now);
        let split = tracker.observe(
            "k",
            &CacheProfile::from_body(&turn_one(plain)).unwrap(),
            now + Duration::from_secs(5),
        );
        assert!(split.read > 0.99, "{split:?}");
    }

    #[test]
    fn key_order_does_not_change_the_fingerprint() {
        let a = r#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":[{"type":"text","text":"PAD"}]}]}"#
            .replace("PAD", &long_system());
        let b = r#"{"messages":[{"content":[{"text":"PAD","type":"text"}],"role":"user"}],"model":"claude-opus-5-5"}"#
            .replace("PAD", &long_system());
        let tracker = CacheTracker::default();
        let now = Instant::now();
        tracker.observe("k", &CacheProfile::from_body(&a).unwrap(), now);
        let split = tracker.observe("k", &CacheProfile::from_body(&b).unwrap(), now);
        assert!(split.read > 0.99, "{split:?}");
    }

    #[test]
    fn namespaces_are_isolated() {
        let tracker = CacheTracker::default();
        let now = Instant::now();
        let profile = CacheProfile::from_body(&turn_one(plain_system())).unwrap();
        tracker.observe("key:1", &profile, now);
        let split = tracker.observe("key:2", &profile, now);
        assert_eq!(split.read, 0.0, "another access key must not read key:1's cache");
    }

    #[test]
    fn different_model_does_not_share_prefix() {
        let tracker = CacheTracker::default();
        let now = Instant::now();
        let opus = turn_one(plain_system());
        let sonnet = opus.replace("claude-opus-5-5", "claude-sonnet-4-6");
        tracker.observe("k", &CacheProfile::from_body(&opus).unwrap(), now);
        let split = tracker.observe("k", &CacheProfile::from_body(&sonnet).unwrap(), now);
        assert_eq!(split.read, 0.0);
    }

    #[test]
    fn apply_splits_total_and_ratios_move_tokens_back_to_input() {
        let split = CacheSplit {
            read: 0.8,
            creation: 0.15,
            one_hour: false,
        };
        let usage = split.apply_with(10_000, 1.0, 1.0);
        assert_eq!(
            usage,
            CacheUsage {
                input_tokens: 500,
                cache_read: 8000,
                cache_write_5m: 1500,
                cache_write_1h: 0,
            }
        );
        let halved = split.apply_with(10_000, 0.5, 0.0);
        assert_eq!(halved.cache_read, 4000);
        assert_eq!(halved.cache_creation(), 0);
        assert_eq!(halved.input_tokens, 6000);

        let hour = CacheSplit { one_hour: true, ..split }.apply_with(10_000, 1.0, 1.0);
        assert_eq!((hour.cache_write_5m, hour.cache_write_1h), (0, 1500));

        let json = usage.to_json(42);
        assert_eq!(json["cache_read_input_tokens"], 8000);
        assert_eq!(json["cache_creation_input_tokens"], 1500);
        assert_eq!(json["cache_creation"]["ephemeral_5m_input_tokens"], 1500);
        assert_eq!(json["output_tokens"], 42);
    }

    #[test]
    fn disabled_usage_json_keeps_original_shape() {
        // 测试进程里没有调用 init，等同于关闭
        let split = CacheSplit {
            read: 0.5,
            creation: 0.5,
            one_hour: false,
        };
        let json = usage_json(Some(&split), 100, 7);
        assert_eq!(json, json!({ "input_tokens": 100, "output_tokens": 7 }));
        assert!(profile(&turn_one(plain_system())).is_none());
    }

    #[test]
    fn tool_use_and_images_are_counted_without_panicking() {
        let body = json!({
            "model": "claude-sonnet-4-6",
            "tools": [{ "name": "read", "description": long_system(), "input_schema": { "type": "object" } }],
            "messages": [
                { "role": "user", "content": [
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
                    { "type": "text", "text": "look" },
                ]},
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "hmm", "signature": "s" },
                    { "type": "tool_use", "id": "t1", "name": "read", "input": { "path": "a" } },
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": [{ "type": "text", "text": "ok" }] },
                ]},
            ],
        })
        .to_string();
        let profile = CacheProfile::from_body(&body).unwrap();
        assert_eq!(profile.blocks.len(), 6);
        assert!(profile.breakpoints.len() >= 2);
    }
}
