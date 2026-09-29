//! 请求记录和模型单价。
//!
//! token 数沿用代理里已有的估算，不是 Kiro 账单。价格按每百万 token 计算，不含缓存。

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const MAX_REQUESTS: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPrice {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub input_per_m: f64,
    pub output_per_m: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestRecord {
    pub id: u64,
    pub time: String,
    pub model: String,
    pub stream: bool,
    pub status: u16,
    pub duration_ms: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestView {
    #[serde(flatten)]
    pub record: RequestRecord,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub model: String,
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub unpriced_requests: u64,
    pub by_model: Vec<ModelUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UsageFile {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    prices: Vec<ModelPrice>,
    #[serde(default)]
    requests: Vec<RequestRecord>,
}

pub struct NewRequest {
    pub model: String,
    pub stream: bool,
    pub status: u16,
    pub duration_ms: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub error: Option<String>,
}

pub struct UsageLog {
    path: Option<PathBuf>,
    inner: Mutex<UsageFile>,
}

impl UsageLog {
    pub fn open(path: Option<PathBuf>) -> Arc<Self> {
        let inner = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Arc::new(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    pub fn record(&self, request: NewRequest) {
        let mut inner = self.inner.lock();
        inner.next_id = inner.next_id.saturating_add(1);
        let id = inner.next_id;
        inner.requests.push(RequestRecord {
            id,
            time: chrono::Utc::now().to_rfc3339(),
            model: request.model,
            stream: request.stream,
            status: request.status,
            duration_ms: request.duration_ms,
            input_tokens: request.input_tokens.max(0),
            output_tokens: request.output_tokens.max(0),
            error: request.error.filter(|err| !err.is_empty()),
        });
        if inner.requests.len() > MAX_REQUESTS {
            let extra = inner.requests.len() - MAX_REQUESTS;
            inner.requests.drain(0..extra);
        }
        self.persist(&inner);
    }

    pub fn list(&self, limit: usize) -> Vec<RequestView> {
        let inner = self.inner.lock();
        let limit = limit.clamp(1, 500);
        inner
            .requests
            .iter()
            .rev()
            .take(limit)
            .map(|record| RequestView {
                cost_usd: charge(&inner.prices, &record.model, record.input_tokens, record.output_tokens),
                record: record.clone(),
            })
            .collect()
    }

    pub fn summary(&self) -> UsageSummary {
        let inner = self.inner.lock();
        let mut by_model: Vec<ModelUsage> = Vec::new();
        let mut unpriced_requests = 0;
        for record in &inner.requests {
            let cost = charge(&inner.prices, &record.model, record.input_tokens, record.output_tokens);
            if cost.is_none() {
                unpriced_requests += 1;
            }
            let row = by_model.iter_mut().find(|row| row.model == record.model);
            let row = if let Some(row) = row {
                row
            } else {
                by_model.push(ModelUsage {
                    model: record.model.clone(),
                    requests: 0,
                    errors: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost_usd: 0.0,
                });
                by_model.last_mut().unwrap()
            };
            row.requests += 1;
            if record.status >= 400 {
                row.errors += 1;
            }
            row.input_tokens += record.input_tokens;
            row.output_tokens += record.output_tokens;
            row.cost_usd += cost.unwrap_or(0.0);
        }
        by_model.sort_by(|a, b| b.cost_usd.partial_cmp(&a.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
        UsageSummary {
            requests: by_model.iter().map(|row| row.requests).sum(),
            errors: by_model.iter().map(|row| row.errors).sum(),
            input_tokens: by_model.iter().map(|row| row.input_tokens).sum(),
            output_tokens: by_model.iter().map(|row| row.output_tokens).sum(),
            cost_usd: by_model.iter().map(|row| row.cost_usd).sum(),
            unpriced_requests,
            by_model,
        }
    }

    pub fn prices(&self) -> Vec<ModelPrice> {
        self.inner.lock().prices.clone()
    }

    pub fn upsert_price(&self, mut price: ModelPrice) -> Result<ModelPrice, String> {
        price.name = price.name.trim().to_string();
        price.aliases = clean_aliases(price.aliases);
        if price.name.is_empty() {
            return Err("名称不能为空".into());
        }
        if price.aliases.is_empty() {
            return Err("至少要有一个模型别名".into());
        }
        if price.input_per_m < 0.0 || price.output_per_m < 0.0 {
            return Err("单价不能为负数".into());
        }
        let mut inner = self.inner.lock();
        if price.id.is_empty() {
            price.id = uuid::Uuid::new_v4().to_string();
        }
        if let Some(existing) = inner.prices.iter().find(|item| {
            item.id != price.id && item.aliases.iter().any(|alias| price.aliases.contains(alias))
        }) {
            return Err(format!("别名已被「{}」占用", existing.name));
        }
        if let Some(slot) = inner.prices.iter_mut().find(|item| item.id == price.id) {
            *slot = price.clone();
        } else {
            inner.prices.push(price.clone());
        }
        self.persist(&inner);
        Ok(price)
    }

    pub fn delete_price(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock();
        let before = inner.prices.len();
        inner.prices.retain(|price| price.id != id);
        if inner.prices.len() == before {
            return Err("定价不存在".into());
        }
        self.persist(&inner);
        Ok(())
    }

    fn persist(&self, inner: &UsageFile) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(inner) {
            let _ = std::fs::write(path, text);
        }
    }
}

pub fn charge(prices: &[ModelPrice], model: &str, input_tokens: i64, output_tokens: i64) -> Option<f64> {
    let model = normalize_alias(model);
    let price = prices.iter().find(|price| price.aliases.iter().any(|alias| alias == &model))?;
    let million = 1_000_000.0;
    Some(input_tokens.max(0) as f64 / million * price.input_per_m + output_tokens.max(0) as f64 / million * price.output_per_m)
}

fn normalize_alias(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn clean_aliases(raw: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for alias in raw {
        let alias = normalize_alias(&alias);
        if alias.is_empty() || out.contains(&alias) {
            continue;
        }
        out.push(alias);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> ModelPrice {
        ModelPrice {
            id: "p1".into(),
            name: "Opus".into(),
            aliases: vec!["claude-opus-5-5".into()],
            input_per_m: 15.0,
            output_per_m: 75.0,
        }
    }

    #[test]
    fn charge_uses_alias_without_cache() {
        let cost = charge(&[price()], "Claude-Opus-5-5", 1_000_000, 1_000_000).unwrap();
        assert!((cost - 90.0).abs() < 0.0001);
        assert!(charge(&[price()], "claude-haiku-4-5", 10, 10).is_none());
    }

    #[test]
    fn summary_counts_unpriced_requests() {
        let log = UsageLog::open(None);
        log.record(NewRequest {
            model: "claude-opus-5-5".into(),
            stream: true,
            status: 200,
            duration_ms: 10,
            input_tokens: 1000,
            output_tokens: 500,
            error: None,
        });
        let summary = log.summary();
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.unpriced_requests, 1);
        assert_eq!(summary.cost_usd, 0.0);
        log.upsert_price(price()).unwrap();
        let summary = log.summary();
        assert_eq!(summary.unpriced_requests, 0);
        assert!(summary.cost_usd > 0.0);
    }
}
