//! 请求记录和模型单价。
//!
//! 明细和单价放在 SQLite。每条请求只插入一行，并累加模型汇总。
//! 明细超过保留上限后删掉最旧的，汇总不删，所以请求变多也不会重写整份历史。
//! token 数沿用代理里已有的估算，不是 Kiro 账单。价格按每百万 token 计算。
//! 开启模拟缓存后，input_tokens 只算未命中的输入，缓存读写单独成列；
//! 缓存单价没填时按官方倍率：读 0.1x、5 分钟写 1.25x、1 小时写 2x 输入价。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

const RETAIN_REQUESTS: i64 = 20_000;
const PRUNE_EVERY: u64 = 100;
const MAX_BLOB: usize = 256 * 1024;
const LENGTH_ERROR_BLOB: usize = 4 * 1024 * 1024;
const CACHE_READ_MULTIPLIER: f64 = 0.1;
const CACHE_WRITE_5M_MULTIPLIER: f64 = 1.25;
const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPrice {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub input_per_m: f64,
    pub output_per_m: f64,
    /// 缓存读单价，空表示按输入价 0.1x
    #[serde(default)]
    pub cache_read_per_m: Option<f64>,
    /// 5 分钟缓存写单价，空表示按输入价 1.25x
    #[serde(default)]
    pub cache_write_5m_per_m: Option<f64>,
    /// 1 小时缓存写单价，空表示按输入价 2x
    #[serde(default)]
    pub cache_write_1h_per_m: Option<f64>,
}

/// 一次请求或一组汇总的计费 token
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
}

/// 一个凭据的人民币成本和累计消耗。
///
/// 消耗每次读取时按当前单价重算，所以改单价后历史消耗和倍率立即跟着变。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCost {
    pub credential_id: u64,
    pub label: String,
    /// 凭据已从配置里删除，只剩历史用量
    pub removed: bool,
    /// 手填的人民币成本。空表示未填，不参与倍率；0 表示免费账号
    pub cost_cny: Option<f64>,
    /// 按当前单价折算的累计消耗，美元
    pub usage_usd: f64,
    /// 成本 ¥ ÷ 消耗 $。成本或消耗为 0 时为空
    pub cost_ratio: Option<f64>,
    pub requests: u64,
    pub errors: u64,
    /// 模型没有单价、没算进消耗的成功请求数
    pub unpriced_requests: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCostReport {
    pub accounts: Vec<AccountCost>,
    /// 已填成本的账号数
    pub costed_accounts: u64,
    /// 已填成本之和，人民币
    pub cost_cny: f64,
    /// 已填成本账号的累计消耗，美元。合计倍率用它做分母
    pub costed_usage_usd: f64,
    /// 全部账号的累计消耗，美元
    pub usage_usd: f64,
    /// 合计成本 ¥ ÷ 已填成本账号的消耗 $
    pub cost_ratio: Option<f64>,
}

/// 成本人民币 ÷ 消耗美元。不做汇率换算，只用来比较账号和定售价。
fn cost_ratio(cost_cny: f64, usage_usd: f64) -> Option<f64> {
    (cost_cny > 0.0 && usage_usd > 0.0).then(|| cost_cny / usage_usd)
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
    pub account: String,
    pub endpoint: String,
    pub request_bytes: u64,
    pub stop_reason: String,
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_5m_tokens: i64,
    #[serde(default)]
    pub cache_write_1h_tokens: i64,
}

impl RequestRecord {
    /// 计费用的 token。失败请求（含断流）不计费，一律按 0。
    fn tokens(&self) -> Tokens {
        if self.status >= 400 {
            return Tokens::default();
        }
        Tokens {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_read: self.cache_read_tokens,
            cache_write_5m: self.cache_write_5m_tokens,
            cache_write_1h: self.cache_write_1h_tokens,
        }
    }
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
    pub cache_read_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub cost_usd: f64,
    pub unpriced_requests: u64,
    pub by_model: Vec<ModelUsage>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LegacyFile {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    prices: Vec<ModelPrice>,
    #[serde(default)]
    requests: Vec<RequestRecord>,
}

pub struct NewRequest {
    /// 实际用到的凭据；冷却、鉴权等没打到上游的请求为空
    pub credential_id: Option<u64>,
    pub model: String,
    pub stream: bool,
    pub status: u16,
    pub duration_ms: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub error: Option<String>,
    pub account: String,
    pub endpoint: String,
    pub request_bytes: u64,
    pub stop_reason: String,
    pub cache_read_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub inbound_headers: String,
    pub inbound_body: String,
    pub outbound_headers: String,
    pub outbound_body: String,
    pub response_body: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestDetail {
    #[serde(flatten)]
    pub view: RequestView,
    pub inbound_headers: String,
    pub inbound_body: String,
    pub outbound_headers: String,
    pub outbound_body: String,
    pub response_body: String,
    pub payload_profile: String,
}

pub struct UsageLog {
    conn: Mutex<Connection>,
    retain_requests: i64,
    since_prune: AtomicU64,
}

impl UsageLog {
    pub fn open(path: Option<PathBuf>) -> Arc<Self> {
        Self::open_with_retain(path, RETAIN_REQUESTS)
    }

    fn open_with_retain(path: Option<PathBuf>, retain_requests: i64) -> Arc<Self> {
        let conn = match &path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                Connection::open(path).unwrap_or_else(|err| {
                    tracing::error!("打开用量数据库失败，改用内存库: {}", err);
                    Connection::open_in_memory().expect("内存 SQLite 不可用")
                })
            }
            None => Connection::open_in_memory().expect("内存 SQLite 不可用"),
        };
        if path.is_some() {
            let _ = conn.pragma_update(None, "journal_mode", "WAL");
            let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        }
        let _ = conn.busy_timeout(Duration::from_secs(2));
        if let Err(err) = init_schema(&conn) {
            tracing::error!("初始化用量数据库失败: {}", err);
        }
        if let Some(path) = &path {
            if let Err(err) = import_legacy_json(&conn, path) {
                tracing::warn!("导入旧的 usage.json 失败: {}", err);
            }
        }
        if let Err(err) = exclude_failed_tokens_from_totals(&conn) {
            tracing::warn!("扣回失败请求 token 失败: {}", err);
        }
        let _ = prune(&conn, retain_requests);
        Arc::new(Self {
            conn: Mutex::new(conn),
            retain_requests,
            since_prune: AtomicU64::new(0),
        })
    }

    pub fn record(&self, request: NewRequest) {
        let conn = self.conn.lock();
        let model = request.model.chars().take(200).collect::<String>();
        let total_key = normalize_alias(&model);
        let input_tokens = request.input_tokens.max(0);
        let output_tokens = request.output_tokens.max(0);
        let cache_read = request.cache_read_tokens.max(0);
        let cache_write_5m = request.cache_write_5m_tokens.max(0);
        let cache_write_1h = request.cache_write_1h_tokens.max(0);
        let error = request.error.filter(|err| !err.is_empty());
        let account = clip(&request.account, 200);
        let endpoint = clip(&request.endpoint, 80);
        let stop_reason = clip(&request.stop_reason, 80);
        let errors: i64 = if request.status >= 400 { 1 } else { 0 };
        let length_error = error.as_deref().is_some_and(|err| {
            err.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") || err.contains("Input is too long")
        });
        let profile = payload_profile(&request.outbound_body);
        if length_error {
            tracing::warn!(profile = %profile, "上游拒绝：输入过长，字段长度");
        }
        let outbound_limit = if length_error { LENGTH_ERROR_BLOB } else { MAX_BLOB };
        let tx = match conn.unchecked_transaction() {
            Ok(tx) => tx,
            Err(err) => {
                tracing::warn!("记录请求失败: {}", err);
                return;
            }
        };
        let inserted = tx.execute(
            "INSERT INTO requests(time, model, stream, status, duration_ms, input_tokens, output_tokens, error, account, endpoint, request_bytes, stop_reason, inbound_headers, inbound_body, outbound_headers, outbound_body, response_body, payload_profile, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, credential_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
            params![
                chrono::Utc::now().to_rfc3339(),
                model,
                request.stream as i64,
                request.status as i64,
                request.duration_ms as i64,
                input_tokens,
                output_tokens,
                error,
                account,
                endpoint,
                request.request_bytes as i64,
                stop_reason,
                clip(&request.inbound_headers, MAX_BLOB),
                clip(&request.inbound_body, MAX_BLOB),
                clip(&request.outbound_headers, MAX_BLOB),
                clip(&request.outbound_body, outbound_limit),
                clip(&request.response_body, MAX_BLOB),
                clip(&profile, 16 * 1024),
                cache_read,
                cache_write_5m,
                cache_write_1h,
                request.credential_id.map(|id| id as i64),
            ],
        );
        if let Err(err) = inserted {
            tracing::warn!("记录请求失败: {}", err);
            return;
        }
        if let Err(err) = tx.execute(
            "INSERT INTO model_totals(model, requests, errors, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(model) DO UPDATE SET
               requests = requests + 1,
               errors = errors + excluded.errors,
               input_tokens = input_tokens + excluded.input_tokens,
               output_tokens = output_tokens + excluded.output_tokens,
               cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
               cache_write_5m_tokens = cache_write_5m_tokens + excluded.cache_write_5m_tokens,
               cache_write_1h_tokens = cache_write_1h_tokens + excluded.cache_write_1h_tokens",
            params![
                total_key,
                errors,
                // 失败请求只计次数，token 不进汇总，也就不计费
                if errors == 0 { input_tokens } else { 0 },
                if errors == 0 { output_tokens } else { 0 },
                if errors == 0 { cache_read } else { 0 },
                if errors == 0 { cache_write_5m } else { 0 },
                if errors == 0 { cache_write_1h } else { 0 }
            ],
        ) {
            tracing::warn!("累计用量失败: {}", err);
            return;
        }
        if let Some(credential_id) = request.credential_id {
            // 账号消耗只算成功请求的 token，失败请求只记次数
            let ok = errors == 0;
            let billable = |value: i64| if ok { value } else { 0 };
            if let Err(err) = add_account_totals(
                &tx,
                credential_id,
                &total_key,
                1,
                errors,
                Tokens {
                    input: billable(input_tokens),
                    output: billable(output_tokens),
                    cache_read: billable(cache_read),
                    cache_write_5m: billable(cache_write_5m),
                    cache_write_1h: billable(cache_write_1h),
                },
            ) {
                tracing::warn!("累计账号用量失败: {}", err);
                return;
            }
        }
        if let Err(err) = tx.commit() {
            tracing::warn!("提交用量失败: {}", err);
            return;
        }
        let tick = self.since_prune.fetch_add(1, Ordering::Relaxed);
        if self.retain_requests <= 100 || tick % PRUNE_EVERY == PRUNE_EVERY - 1 {
            if let Err(err) = prune(&conn, self.retain_requests) {
                tracing::warn!("清理旧请求失败: {}", err);
            }
        }
    }

    pub fn list(&self, limit: usize) -> Vec<RequestView> {
        let conn = self.conn.lock();
        let limit = limit.clamp(1, 500) as i64;
        let prices = match load_prices(&conn) {
            Ok(prices) => prices,
            Err(err) => {
                tracing::warn!("读取定价失败: {}", err);
                Vec::new()
            }
        };
        let mut stmt = match conn.prepare(
            &format!("SELECT {RECORD_COLUMNS} FROM requests ORDER BY id DESC LIMIT ?1"),
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                tracing::warn!("读取请求失败: {}", err);
                return Vec::new();
            }
        };
        let rows = stmt.query_map(params![limit], read_record);
        let Ok(rows) = rows else {
            return Vec::new();
        };
        rows.filter_map(|row| row.ok())
            .map(|record| RequestView {
                cost_usd: charge(&prices, &record.model, record.tokens()),
                record,
            })
            .collect()
    }

    pub fn get(&self, id: u64) -> Option<RequestDetail> {
        let conn = self.conn.lock();
        let prices = load_prices(&conn).unwrap_or_default();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {RECORD_COLUMNS}, inbound_headers, inbound_body, outbound_headers, outbound_body, response_body, payload_profile
                     FROM requests WHERE id = ?1"
                ),
                params![id as i64],
                |row| {
                    let record = read_record(row)?;
                    let text = |index: usize| -> rusqlite::Result<String> {
                        Ok(row.get::<_, Option<String>>(RECORD_COLUMN_COUNT + index)?.unwrap_or_default())
                    };
                    Ok(RequestDetail {
                        inbound_headers: text(0)?,
                        inbound_body: text(1)?,
                        outbound_headers: text(2)?,
                        outbound_body: text(3)?,
                        response_body: text(4)?,
                        payload_profile: text(5)?,
                        view: RequestView {
                            cost_usd: charge(&prices, &record.model, record.tokens()),
                            record,
                        },
                    })
                },
            )
            .optional()
            .unwrap_or(None);
        row
    }

    pub fn summary(&self) -> UsageSummary {
        let conn = self.conn.lock();
        let prices = load_prices(&conn).unwrap_or_default();
        let mut stmt = match conn.prepare(
            "SELECT model, requests, errors, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens FROM model_totals",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                tracing::warn!("读取用量汇总失败: {}", err);
                return empty_summary();
            }
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                Tokens {
                    input: row.get(3)?,
                    output: row.get(4)?,
                    cache_read: row.get(5)?,
                    cache_write_5m: row.get(6)?,
                    cache_write_1h: row.get(7)?,
                },
            ))
        });
        let Ok(rows) = rows else {
            return empty_summary();
        };
        let mut by_model = Vec::new();
        let mut unpriced_requests = 0;
        for row in rows.flatten() {
            let (model, requests, errors, tokens) = row;
            let cost = charge(&prices, &model, tokens);
            if cost.is_none() {
                unpriced_requests += requests.max(0) as u64;
            }
            by_model.push(ModelUsage {
                model,
                requests: requests.max(0) as u64,
                errors: errors.max(0) as u64,
                input_tokens: tokens.input,
                output_tokens: tokens.output,
                cache_read_tokens: tokens.cache_read,
                cache_write_5m_tokens: tokens.cache_write_5m,
                cache_write_1h_tokens: tokens.cache_write_1h,
                cost_usd: cost.unwrap_or(0.0),
            });
        }
        by_model.sort_by(|a, b| b.cost_usd.partial_cmp(&a.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
        UsageSummary {
            requests: by_model.iter().map(|row| row.requests).sum(),
            errors: by_model.iter().map(|row| row.errors).sum(),
            input_tokens: by_model.iter().map(|row| row.input_tokens).sum(),
            output_tokens: by_model.iter().map(|row| row.output_tokens).sum(),
            cache_read_tokens: by_model.iter().map(|row| row.cache_read_tokens).sum(),
            cache_write_5m_tokens: by_model.iter().map(|row| row.cache_write_5m_tokens).sum(),
            cache_write_1h_tokens: by_model.iter().map(|row| row.cache_write_1h_tokens).sum(),
            cost_usd: by_model.iter().map(|row| row.cost_usd).sum(),
            unpriced_requests,
            by_model,
        }
    }

    pub fn prices(&self) -> Vec<ModelPrice> {
        load_prices(&self.conn.lock()).unwrap_or_default()
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
        if [price.cache_read_per_m, price.cache_write_5m_per_m, price.cache_write_1h_per_m]
            .iter()
            .flatten()
            .any(|value| *value < 0.0 || !value.is_finite())
        {
            return Err("缓存单价不能为负数".into());
        }
        let conn = self.conn.lock();
        if price.id.is_empty() {
            price.id = uuid::Uuid::new_v4().to_string();
        }
        let existing = load_prices(&conn).map_err(|err| err.to_string())?;
        if let Some(other) = existing.iter().find(|item| {
            item.id != price.id && item.aliases.iter().any(|alias| price.aliases.contains(alias))
        }) {
            return Err(format!("别名已被「{}」占用", other.name));
        }
        let aliases = serde_json::to_string(&price.aliases).map_err(|err| err.to_string())?;
        conn.execute(
            "INSERT INTO prices(id, name, aliases, input_per_m, output_per_m, cache_read_per_m, cache_write_5m_per_m, cache_write_1h_per_m)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name,
               aliases = excluded.aliases,
               input_per_m = excluded.input_per_m,
               output_per_m = excluded.output_per_m,
               cache_read_per_m = excluded.cache_read_per_m,
               cache_write_5m_per_m = excluded.cache_write_5m_per_m,
               cache_write_1h_per_m = excluded.cache_write_1h_per_m",
            params![
                price.id,
                price.name,
                aliases,
                price.input_per_m,
                price.output_per_m,
                price.cache_read_per_m,
                price.cache_write_5m_per_m,
                price.cache_write_1h_per_m
            ],
        )
        .map_err(|err| err.to_string())?;
        Ok(price)
    }

    /// 设置账号人民币成本。None 表示清空（未知），0 表示免费账号，两者含义不同。
    pub fn set_account_cost(&self, credential_id: u64, cost_cny: Option<f64>) -> Result<(), String> {
        let conn = self.conn.lock();
        match cost_cny {
            None => {
                conn.execute(
                    "DELETE FROM account_costs WHERE credential_id = ?1",
                    params![credential_id as i64],
                )
                .map_err(|err| err.to_string())?;
            }
            Some(cost) => {
                if !cost.is_finite() || cost < 0.0 {
                    return Err("成本不能为负数".into());
                }
                conn.execute(
                    "INSERT INTO account_costs(credential_id, cost_cny, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(credential_id) DO UPDATE SET cost_cny = excluded.cost_cny, updated_at = excluded.updated_at",
                    params![credential_id as i64, cost, chrono::Utc::now().to_rfc3339()],
                )
                .map_err(|err| err.to_string())?;
            }
        }
        Ok(())
    }

    /// 账号成本和累计消耗。`labels` 是当前配置里的凭据（id, 显示名）。
    ///
    /// 消耗每次都按当前单价从 token 重算，不存结果，所以改单价后历史消耗和倍率立即更新。
    /// 已从配置删除但还有历史用量或成本的凭据也会列出，标为 removed。
    pub fn account_report(&self, labels: &[(u64, String)]) -> AccountCostReport {
        let conn = self.conn.lock();
        let prices = load_prices(&conn).unwrap_or_default();
        let mut costs: HashMap<u64, f64> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare("SELECT credential_id, cost_cny FROM account_costs") {
            if let Ok(rows) =
                stmt.query_map([], |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, f64>(1)?)))
            {
                costs.extend(rows.flatten());
            }
        }
        let mut totals: BTreeMap<u64, Vec<(String, i64, i64, Tokens)>> = BTreeMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT credential_id, model, requests, errors, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens FROM account_totals",
        ) {
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    Tokens {
                        input: row.get(4)?,
                        output: row.get(5)?,
                        cache_read: row.get(6)?,
                        cache_write_5m: row.get(7)?,
                        cache_write_1h: row.get(8)?,
                    },
                ))
            });
            if let Ok(rows) = rows {
                for (id, model, requests, errors, tokens) in rows.flatten() {
                    totals.entry(id).or_default().push((model, requests, errors, tokens));
                }
            }
        }

        let mut order: Vec<(u64, String, bool)> =
            labels.iter().map(|(id, label)| (*id, label.clone(), false)).collect();
        let known: std::collections::HashSet<u64> = labels.iter().map(|(id, _)| *id).collect();
        let mut orphans: Vec<u64> = totals
            .keys()
            .chain(costs.keys())
            .copied()
            .filter(|id| !known.contains(id))
            .collect();
        orphans.sort_unstable();
        orphans.dedup();
        order.extend(orphans.into_iter().map(|id| (id, format!("#{id}"), true)));

        let mut accounts = Vec::with_capacity(order.len());
        for (credential_id, label, removed) in order {
            let mut row = AccountCost {
                credential_id,
                label,
                removed,
                cost_cny: costs.get(&credential_id).copied(),
                usage_usd: 0.0,
                cost_ratio: None,
                requests: 0,
                errors: 0,
                unpriced_requests: 0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_5m_tokens: 0,
                cache_write_1h_tokens: 0,
            };
            for (model, requests, errors, tokens) in totals.get(&credential_id).into_iter().flatten() {
                row.requests += (*requests).max(0) as u64;
                row.errors += (*errors).max(0) as u64;
                row.input_tokens += tokens.input;
                row.output_tokens += tokens.output;
                row.cache_read_tokens += tokens.cache_read;
                row.cache_write_5m_tokens += tokens.cache_write_5m;
                row.cache_write_1h_tokens += tokens.cache_write_1h;
                match charge(&prices, model, *tokens) {
                    Some(cost) => row.usage_usd += cost,
                    None => row.unpriced_requests += (*requests - *errors).max(0) as u64,
                }
            }
            row.cost_ratio = row.cost_cny.and_then(|cost| cost_ratio(cost, row.usage_usd));
            accounts.push(row);
        }

        let costed: Vec<&AccountCost> = accounts.iter().filter(|row| row.cost_cny.is_some()).collect();
        let cost_cny: f64 = costed.iter().filter_map(|row| row.cost_cny).sum();
        let costed_usage_usd: f64 = costed.iter().map(|row| row.usage_usd).sum();
        AccountCostReport {
            costed_accounts: costed.len() as u64,
            cost_cny,
            costed_usage_usd,
            usage_usd: accounts.iter().map(|row| row.usage_usd).sum(),
            cost_ratio: cost_ratio(cost_cny, costed_usage_usd),
            accounts,
        }
    }

    /// 升级前的请求没有 credential_id。按 account 标签（邮箱或 `#id`）找回凭据，
    /// 回填 credential_id 并补进 account_totals。已回填的行不会再计入，可以反复调用。
    pub fn backfill_accounts(&self, email_to_id: &HashMap<String, u64>) -> usize {
        let conn = self.conn.lock();
        let resolve = |label: &str| -> Option<u64> {
            let label = label.trim();
            if let Some(id) = label.strip_prefix('#').and_then(|id| id.parse().ok()) {
                return Some(id);
            }
            email_to_id.get(label).copied()
        };
        let rows: Vec<(String, String, i64, Tokens)> = match conn.prepare(
            "SELECT account, model, status, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens
             FROM requests WHERE credential_id IS NULL AND account != ''",
        ) {
            Ok(mut stmt) => stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        Tokens {
                            input: row.get(3)?,
                            output: row.get(4)?,
                            cache_read: row.get(5)?,
                            cache_write_5m: row.get(6)?,
                            cache_write_1h: row.get(7)?,
                        },
                    ))
                })
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default(),
            Err(err) => {
                tracing::warn!("读取待回填请求失败: {}", err);
                return 0;
            }
        };
        let mut grouped: HashMap<(u64, String), (i64, i64, Tokens)> = HashMap::new();
        let mut labels: HashMap<String, u64> = HashMap::new();
        for (account, model, status, tokens) in rows {
            let Some(id) = resolve(&account) else { continue };
            labels.insert(account, id);
            let entry = grouped.entry((id, normalize_alias(&model))).or_default();
            entry.0 += 1;
            if status >= 400 {
                entry.1 += 1;
            } else {
                entry.2.input += tokens.input.max(0);
                entry.2.output += tokens.output.max(0);
                entry.2.cache_read += tokens.cache_read.max(0);
                entry.2.cache_write_5m += tokens.cache_write_5m.max(0);
                entry.2.cache_write_1h += tokens.cache_write_1h.max(0);
            }
        }
        if grouped.is_empty() {
            return 0;
        }
        let result = (|| -> rusqlite::Result<usize> {
            let tx = conn.unchecked_transaction()?;
            for ((id, model), (requests, errors, tokens)) in &grouped {
                add_account_totals(&tx, *id, model, *requests, *errors, *tokens)?;
            }
            let mut updated = 0;
            for (account, id) in &labels {
                updated += tx.execute(
                    "UPDATE requests SET credential_id = ?1 WHERE credential_id IS NULL AND account = ?2",
                    params![*id as i64, account],
                )?;
            }
            tx.commit()?;
            Ok(updated)
        })();
        match result {
            Ok(updated) => {
                tracing::info!(requests = updated, "已按凭据回填历史用量");
                updated
            }
            Err(err) => {
                tracing::warn!("回填账号用量失败: {}", err);
                0
            }
        }
    }

    pub fn delete_price(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock();
        let changed = conn
            .execute("DELETE FROM prices WHERE id = ?1", params![id])
            .map_err(|err| err.to_string())?;
        if changed == 0 {
            return Err("定价不存在".into());
        }
        Ok(())
    }
}

fn empty_summary() -> UsageSummary {
    UsageSummary {
        requests: 0,
        errors: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_5m_tokens: 0,
        cache_write_1h_tokens: 0,
        cost_usd: 0.0,
        unpriced_requests: 0,
        by_model: Vec::new(),
    }
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS requests (
            id INTEGER PRIMARY KEY,
            time TEXT NOT NULL,
            model TEXT NOT NULL,
            stream INTEGER NOT NULL,
            status INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL,
            input_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            error TEXT
        );
        CREATE TABLE IF NOT EXISTS model_totals (
            model TEXT PRIMARY KEY,
            requests INTEGER NOT NULL,
            errors INTEGER NOT NULL,
            input_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS prices (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            aliases TEXT NOT NULL,
            input_per_m REAL NOT NULL,
            output_per_m REAL NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS account_costs (
            credential_id INTEGER PRIMARY KEY,
            cost_cny REAL NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS account_totals (
            credential_id INTEGER NOT NULL,
            model TEXT NOT NULL,
            requests INTEGER NOT NULL DEFAULT 0,
            errors INTEGER NOT NULL DEFAULT 0,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0,
            cache_write_5m_tokens INTEGER NOT NULL DEFAULT 0,
            cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (credential_id, model)
        );",
    )?;
    ensure_column(conn, "requests", "account", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "requests", "endpoint", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "requests", "request_bytes", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "requests", "stop_reason", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "requests", "inbound_headers", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "requests", "inbound_body", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "requests", "outbound_headers", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "requests", "outbound_body", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "requests", "response_body", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "requests", "payload_profile", "TEXT NOT NULL DEFAULT ''")?;
    for table in ["requests", "model_totals"] {
        for column in ["cache_read_tokens", "cache_write_5m_tokens", "cache_write_1h_tokens"] {
            ensure_column(conn, table, column, "INTEGER NOT NULL DEFAULT 0")?;
        }
    }
    for column in ["cache_read_per_m", "cache_write_5m_per_m", "cache_write_1h_per_m"] {
        ensure_column(conn, "prices", column, "REAL")?;
    }
    ensure_column(conn, "requests", "credential_id", "INTEGER")?;
    Ok(())
}

/// 早期版本把失败请求的 token 也累加进了 model_totals，汇总里会被计费。
/// 用明细里的失败行一次性扣回。只执行一次，靠 meta 标记。
/// 明细超过保留上限被清理的失败行扣不回来，这部分会继续留在汇总里。
fn exclude_failed_tokens_from_totals(conn: &Connection) -> rusqlite::Result<()> {
    const KEY: &str = "failed_tokens_excluded";
    let done: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![KEY], |row| row.get(0))
        .optional()?;
    if done.is_some() {
        return Ok(());
    }
    let mut failed: HashMap<String, Tokens> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT model, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens
             FROM requests WHERE status >= 400",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Tokens {
                    input: row.get(1)?,
                    output: row.get(2)?,
                    cache_read: row.get(3)?,
                    cache_write_5m: row.get(4)?,
                    cache_write_1h: row.get(5)?,
                },
            ))
        })?;
        for (model, tokens) in rows.flatten() {
            let sum = failed.entry(normalize_alias(&model)).or_default();
            sum.input += tokens.input.max(0);
            sum.output += tokens.output.max(0);
            sum.cache_read += tokens.cache_read.max(0);
            sum.cache_write_5m += tokens.cache_write_5m.max(0);
            sum.cache_write_1h += tokens.cache_write_1h.max(0);
        }
    }
    let tx = conn.unchecked_transaction()?;
    for (model, tokens) in &failed {
        tx.execute(
            "UPDATE model_totals SET
               input_tokens = max(input_tokens - ?2, 0),
               output_tokens = max(output_tokens - ?3, 0),
               cache_read_tokens = max(cache_read_tokens - ?4, 0),
               cache_write_5m_tokens = max(cache_write_5m_tokens - ?5, 0),
               cache_write_1h_tokens = max(cache_write_1h_tokens - ?6, 0)
             WHERE model = ?1",
            params![
                model,
                tokens.input,
                tokens.output,
                tokens.cache_read,
                tokens.cache_write_5m,
                tokens.cache_write_1h
            ],
        )?;
    }
    tx.execute("INSERT INTO meta(key, value) VALUES(?1, '1')", params![KEY])?;
    tx.commit()?;
    if !failed.is_empty() {
        tracing::info!(models = failed.len(), "已从模型汇总扣回失败请求的 token");
    }
    Ok(())
}

/// 按凭据×模型累计。模型名已归一化，与 model_totals 同一口径。
fn add_account_totals(
    conn: &Connection,
    credential_id: u64,
    model: &str,
    requests: i64,
    errors: i64,
    tokens: Tokens,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO account_totals(credential_id, model, requests, errors, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(credential_id, model) DO UPDATE SET
           requests = requests + excluded.requests,
           errors = errors + excluded.errors,
           input_tokens = input_tokens + excluded.input_tokens,
           output_tokens = output_tokens + excluded.output_tokens,
           cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
           cache_write_5m_tokens = cache_write_5m_tokens + excluded.cache_write_5m_tokens,
           cache_write_1h_tokens = cache_write_1h_tokens + excluded.cache_write_1h_tokens",
        params![
            credential_id as i64,
            model,
            requests,
            errors,
            tokens.input.max(0),
            tokens.output.max(0),
            tokens.cache_read.max(0),
            tokens.cache_write_5m.max(0),
            tokens.cache_write_1h.max(0)
        ],
    )?;
    Ok(())
}

const RECORD_COLUMNS: &str = "id, time, model, stream, status, duration_ms, input_tokens, output_tokens, error, account, endpoint, request_bytes, stop_reason, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens";
const RECORD_COLUMN_COUNT: usize = 16;

fn read_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRecord> {
    Ok(RequestRecord {
        id: row.get::<_, i64>(0)? as u64,
        time: row.get(1)?,
        model: row.get(2)?,
        stream: row.get::<_, i64>(3)? != 0,
        status: row.get::<_, i64>(4)? as u16,
        duration_ms: row.get::<_, i64>(5)? as u64,
        input_tokens: row.get(6)?,
        output_tokens: row.get(7)?,
        error: row.get(8)?,
        account: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        endpoint: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
        request_bytes: row.get::<_, Option<i64>>(11)?.unwrap_or(0).max(0) as u64,
        stop_reason: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
        cache_read_tokens: row.get::<_, Option<i64>>(13)?.unwrap_or(0),
        cache_write_5m_tokens: row.get::<_, Option<i64>>(14)?.unwrap_or(0),
        cache_write_1h_tokens: row.get::<_, Option<i64>>(15)?.unwrap_or(0),
    })
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )?;
    Ok(())
}

fn prune(conn: &Connection, retain: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM requests WHERE id < (
            SELECT MIN(id) FROM (
                SELECT id FROM requests ORDER BY id DESC LIMIT ?1
            )
        )",
        params![retain.max(1)],
    )?;
    Ok(())
}

fn load_prices(conn: &Connection) -> rusqlite::Result<Vec<ModelPrice>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, aliases, input_per_m, output_per_m, cache_read_per_m, cache_write_5m_per_m, cache_write_1h_per_m FROM prices",
    )?;
    let rows = stmt.query_map([], |row| {
        let aliases: String = row.get(2)?;
        Ok(ModelPrice {
            id: row.get(0)?,
            name: row.get(1)?,
            aliases: serde_json::from_str(&aliases).unwrap_or_default(),
            input_per_m: row.get(3)?,
            output_per_m: row.get(4)?,
            cache_read_per_m: row.get(5)?,
            cache_write_5m_per_m: row.get(6)?,
            cache_write_1h_per_m: row.get(7)?,
        })
    })?;
    rows.collect()
}

fn import_legacy_json(conn: &Connection, sqlite_path: &Path) -> rusqlite::Result<()> {
    let imported: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'legacy_json'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if imported.is_some() {
        return Ok(());
    }
    let json_path = sqlite_path.with_file_name("usage.json");
    if let Ok(text) = std::fs::read_to_string(&json_path) {
        if let Ok(legacy) = serde_json::from_str::<LegacyFile>(&text) {
            let tx = conn.unchecked_transaction()?;
            for price in legacy.prices {
                let aliases = serde_json::to_string(&price.aliases).unwrap_or_else(|_| "[]".into());
                tx.execute(
                    "INSERT OR IGNORE INTO prices(id, name, aliases, input_per_m, output_per_m)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![price.id, price.name, aliases, price.input_per_m, price.output_per_m],
                )?;
            }
            for record in &legacy.requests {
                tx.execute(
                    "INSERT OR IGNORE INTO requests(id, time, model, stream, status, duration_ms, input_tokens, output_tokens, error)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        record.id,
                        record.time,
                        record.model,
                        record.stream as i64,
                        record.status as i64,
                        record.duration_ms as i64,
                        record.input_tokens,
                        record.output_tokens,
                        record.error,
                    ],
                )?;
                let errors: i64 = if record.status >= 400 { 1 } else { 0 };
                tx.execute(
                    "INSERT INTO model_totals(model, requests, errors, input_tokens, output_tokens)
                     VALUES (?1, 1, ?2, ?3, ?4)
                     ON CONFLICT(model) DO UPDATE SET
                       requests = requests + 1,
                       errors = errors + excluded.errors,
                       input_tokens = input_tokens + excluded.input_tokens,
                       output_tokens = output_tokens + excluded.output_tokens",
                    params![
                        normalize_alias(&record.model),
                        errors,
                        record.input_tokens.max(0),
                        record.output_tokens.max(0)
                    ],
                )?;
            }
            let _ = legacy.next_id;
            tx.execute(
                "INSERT INTO meta(key, value) VALUES('legacy_json', '1')",
                [],
            )?;
            tx.commit()?;
            return Ok(());
        }
    }
    conn.execute(
        "INSERT INTO meta(key, value) VALUES('legacy_json', '1')",
        [],
    )?;
    Ok(())
}

pub fn charge(prices: &[ModelPrice], model: &str, tokens: Tokens) -> Option<f64> {
    let model = normalize_alias(model);
    let price = prices.iter().find(|price| price.aliases.iter().any(|alias| alias == &model))?;
    let million = 1_000_000.0;
    let per = |count: i64, rate: f64| count.max(0) as f64 / million * rate;
    let read = price.cache_read_per_m.unwrap_or(price.input_per_m * CACHE_READ_MULTIPLIER);
    let write_5m = price
        .cache_write_5m_per_m
        .unwrap_or(price.input_per_m * CACHE_WRITE_5M_MULTIPLIER);
    let write_1h = price
        .cache_write_1h_per_m
        .unwrap_or(price.input_per_m * CACHE_WRITE_1H_MULTIPLIER);
    Some(
        per(tokens.input, price.input_per_m)
            + per(tokens.output, price.output_per_m)
            + per(tokens.cache_read, read)
            + per(tokens.cache_write_5m, write_5m)
            + per(tokens.cache_write_1h, write_1h),
    )
}

pub fn encode_headers<I, K, V>(headers: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        let name = name.as_ref();
        let value = if header_sensitive(name) {
            "[redacted]"
        } else {
            value.as_ref()
        };
        map.insert(name.to_string(), serde_json::Value::String(value.to_string()));
    }
    clip(&serde_json::to_string(&map).unwrap_or_default(), MAX_BLOB)
}

fn header_sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key"
    ) || name.contains("api-key")
        || name.contains("secret")
        || (name.contains("token") && name != "tokentype")
}

/// 出站请求的结构摘要。只记录路径、长度和图片尺寸，方便对照「Input is too long」落在哪一段。
pub fn payload_profile(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return format!("bytes={} json=invalid", body.len());
    };
    let state = value.get("conversationState").unwrap_or(&value);
    let mut images = 0usize;
    let mut image_bytes = 0usize;
    let mut tools = 0usize;
    let mut tool_uses = 0usize;
    let mut tool_results = 0usize;
    let mut notable = Vec::new();
    if let Some(history) = state.get("history").and_then(|item| item.as_array()) {
        for (index, message) in history.iter().enumerate() {
            notable.push(message_profile(index, message, &mut images, &mut image_bytes, &mut tools, &mut tool_uses, &mut tool_results));
        }
    }
    if let Some(current) = state.get("currentMessage") {
        notable.push(message_profile(usize::MAX, current, &mut images, &mut image_bytes, &mut tools, &mut tool_uses, &mut tool_results));
    }
    let history_len = state.get("history").and_then(|item| item.as_array()).map(|items| items.len()).unwrap_or(0);
    let mut hits = Vec::new();
    collect_large_strings(&value, "", &mut hits);
    hits.sort_by(|left, right| right.1.cmp(&left.1));
    hits.truncate(8);
    notable.retain(|(_, score, _)| *score >= 2_000);
    notable.sort_by(|left, right| right.1.cmp(&left.1));
    notable.truncate(24);
    notable.sort_by_key(|(index, _, _)| *index);
    let mut lines = vec![format!(
        "bytes={body_len} history={history_len} images={images} image_bytes={image_bytes} tools={tools} tool_uses={tool_uses} tool_results={tool_results}",
        body_len = body.len(),
    )];
    for (path, len, note) in hits {
        lines.push(format!("largest {path} {len}{note}"));
    }
    for (_, _, line) in notable {
        lines.push(line);
    }
    lines.join("\n")
}

fn message_profile(
    index: usize,
    message: &serde_json::Value,
    images: &mut usize,
    image_bytes: &mut usize,
    tools: &mut usize,
    tool_uses: &mut usize,
    tool_results: &mut usize,
) -> (usize, usize, String) {
    let label = if index == usize::MAX { "current".to_string() } else { format!("m{index}") };
    let sort = if index == usize::MAX { usize::MAX } else { index };
    if let Some(user) = message.get("userInputMessage") {
        let content = string_len(user, "content");
        let (count, bytes, dims) = image_stats(user.get("images"));
        *images += count;
        *image_bytes += bytes;
        let context = user.get("userInputMessageContext");
        let tool_count = array_len(context.and_then(|item| item.get("tools")));
        let (result_count, result_bytes) = text_stats(context.and_then(|item| item.get("toolResults")), &["content", "text"]);
        *tools += tool_count;
        *tool_results += result_count;
        let score = content + bytes + result_bytes;
        return (
            sort,
            score,
            format!("{label} user content={content} images={count}/{bytes}{dims} tools={tool_count} tool_results={result_count}/{result_bytes}"),
        );
    }
    if let Some(assistant) = message.get("assistantResponseMessage") {
        let content = string_len(assistant, "content");
        let uses = assistant.get("toolUses").and_then(|item| item.as_array());
        let use_count = uses.map(|items| items.len()).unwrap_or(0);
        let use_bytes = uses
            .map(|items| items.iter().map(|item| serde_json::to_string(item.get("input").unwrap_or(&serde_json::Value::Null)).map(|text| text.len()).unwrap_or(0)).sum())
            .unwrap_or(0);
        *tool_uses += use_count;
        return (
            sort,
            content + use_bytes,
            format!("{label} assistant content={content} tool_uses={use_count}/{use_bytes}"),
        );
    }
    (sort, 0, format!("{label} other"))
}

fn string_len(value: &serde_json::Value, key: &str) -> usize {
    value.get(key).and_then(|item| item.as_str()).map(|text| text.len()).unwrap_or(0)
}

fn array_len(value: Option<&serde_json::Value>) -> usize {
    value.and_then(|item| item.as_array()).map(|items| items.len()).unwrap_or(0)
}

fn image_stats(value: Option<&serde_json::Value>) -> (usize, usize, String) {
    let Some(items) = value.and_then(|item| item.as_array()) else {
        return (0, 0, String::new());
    };
    let mut bytes = 0usize;
    let mut dims = Vec::new();
    for image in items {
        let data = image
            .pointer("/source/bytes")
            .or_else(|| image.get("bytes"))
            .and_then(|item| item.as_str())
            .unwrap_or("");
        bytes += data.len();
        if let Some((kind, width, height)) = image_meta(data) {
            dims.push(format!("{kind} {width}x{height}"));
        }
    }
    let note = if dims.is_empty() { String::new() } else { format!(" {}", dims.join(",")) };
    (items.len(), bytes, note)
}

fn text_stats(value: Option<&serde_json::Value>, keys: &[&str]) -> (usize, usize) {
    let Some(items) = value.and_then(|item| item.as_array()) else {
        return (0, 0);
    };
    let bytes = items
        .iter()
        .map(|item| {
            keys.iter().map(|key| string_len(item, key)).sum::<usize>().max(
                item.get("content")
                    .map(|content| serde_json::to_string(content).map(|text| text.len()).unwrap_or(0))
                    .unwrap_or(0),
            )
        })
        .sum();
    (items.len(), bytes)
}

fn collect_large_strings(value: &serde_json::Value, path: &str, hits: &mut Vec<(String, usize, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let next = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                collect_large_strings(child, &next, hits);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_large_strings(child, &format!("{path}[{index}]"), hits);
            }
        }
        serde_json::Value::String(text) if text.len() >= 256 => {
            let note = if path.ends_with("bytes") {
                image_meta(text).map(|(kind, width, height)| format!(" {kind} {width}x{height}")).unwrap_or_default()
            } else {
                String::new()
            };
            hits.push((path.to_string(), text.len(), note));
        }
        _ => {}
    }
}

fn image_meta(data: &str) -> Option<(&'static str, u32, u32)> {
    let bytes = decode_b64_prefix(data, 128)?;
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() >= 24 && &bytes[12..16] == b"IHDR" {
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some(("png", width, height));
    }
    if bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut index = 2;
        while index + 9 < bytes.len() {
            if bytes[index] != 0xFF {
                break;
            }
            let marker = bytes[index + 1];
            if marker == 0xC0 || marker == 0xC1 || marker == 0xC2 {
                let height = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;
                let width = u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]) as u32;
                return Some(("jpeg", width, height));
            }
            let len = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
            if len < 2 {
                break;
            }
            index += 2 + len;
        }
    }
    None
}

fn decode_b64_prefix(data: &str, max_bytes: usize) -> Option<Vec<u8>> {
    fn val(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let chars: Vec<u8> = data.bytes().filter(|byte| val(*byte).is_some()).take(max_bytes.div_ceil(3) * 4).collect();
    if chars.len() < 4 {
        return None;
    }
    let mut out = Vec::new();
    let mut index = 0;
    while index + 4 <= chars.len() && out.len() < max_bytes {
        let a = val(chars[index])?;
        let b = val(chars[index + 1])?;
        let c = val(chars[index + 2])?;
        let d = val(chars[index + 3])?;
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
        out.push((c << 6) | d);
        index += 4;
    }
    out.truncate(max_bytes);
    Some(out)
}

fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated {} bytes]", &text[..end], text.len() - end)
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
            cache_read_per_m: None,
            cache_write_5m_per_m: None,
            cache_write_1h_per_m: None,
        }
    }

    fn sample(model: &str, status: u16) -> NewRequest {
        NewRequest {
            credential_id: None,
            model: model.into(),
            stream: true,
            status,
            duration_ms: 10,
            input_tokens: 1000,
            output_tokens: 500,
            error: None,
            account: "preview@example.com".into(),
            endpoint: "ide".into(),
            request_bytes: 128,
            stop_reason: "end_turn".into(),
            cache_read_tokens: 0,
            cache_write_5m_tokens: 0,
            cache_write_1h_tokens: 0,
            inbound_headers: String::new(),
            inbound_body: String::new(),
            outbound_headers: String::new(),
            outbound_body: String::new(),
            response_body: String::new(),
        }
    }

    #[test]
    fn charge_uses_alias_without_cache() {
        let plain = |input, output| Tokens {
            input,
            output,
            ..Tokens::default()
        };
        let cost = charge(&[price()], "Claude-Opus-5-5", plain(1_000_000, 1_000_000)).unwrap();
        assert!((cost - 90.0).abs() < 0.0001);
        assert!(charge(&[price()], "claude-haiku-4-5", plain(10, 10)).is_none());
    }

    #[test]
    fn cache_tokens_use_default_multipliers_or_explicit_prices() {
        let tokens = Tokens {
            input: 0,
            output: 0,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
        };
        // 输入 15：读 1.5 + 5m 写 18.75 + 1h 写 30
        let cost = charge(&[price()], "claude-opus-5-5", tokens).unwrap();
        assert!((cost - 50.25).abs() < 0.0001, "{cost}");
        let mut explicit = price();
        explicit.cache_read_per_m = Some(1.0);
        explicit.cache_write_5m_per_m = Some(2.0);
        explicit.cache_write_1h_per_m = Some(3.0);
        let cost = charge(&[explicit], "claude-opus-5-5", tokens).unwrap();
        assert!((cost - 6.0).abs() < 0.0001, "{cost}");
    }

    #[test]
    fn cache_tokens_are_stored_and_summed() {
        let log = UsageLog::open(None);
        let mut request = sample("claude-opus-5-5", 200);
        request.input_tokens = 100;
        request.cache_read_tokens = 9000;
        request.cache_write_5m_tokens = 400;
        request.cache_write_1h_tokens = 50;
        log.record(request);
        log.upsert_price(price()).unwrap();
        let listed = &log.list(10)[0];
        assert_eq!(listed.record.cache_read_tokens, 9000);
        assert_eq!(listed.record.cache_write_5m_tokens, 400);
        assert_eq!(listed.record.cache_write_1h_tokens, 50);
        let detail = log.get(listed.record.id).unwrap();
        assert_eq!(detail.view.record.cache_read_tokens, 9000);
        let summary = log.summary();
        assert_eq!(summary.cache_read_tokens, 9000);
        assert_eq!(summary.by_model[0].cache_write_1h_tokens, 50);
        // 100*15 + 500*75 + 9000*1.5 + 400*18.75 + 50*30，除以 1e6
        let expected = (1500.0 + 37500.0 + 13500.0 + 7500.0 + 1500.0) / 1_000_000.0;
        assert!((summary.cost_usd - expected).abs() < 1e-9, "{}", summary.cost_usd);
        let mut negative = price();
        negative.cache_read_per_m = Some(-1.0);
        assert!(log.upsert_price(negative).is_err());
    }

    #[test]
    fn summary_counts_unpriced_requests() {
        let log = UsageLog::open(None);
        log.record(sample("claude-opus-5-5", 200));
        let summary = log.summary();
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.unpriced_requests, 1);
        assert_eq!(summary.cost_usd, 0.0);
        log.upsert_price(price()).unwrap();
        let summary = log.summary();
        assert_eq!(summary.unpriced_requests, 0);
        assert!(summary.cost_usd > 0.0);
    }

    fn on_credential(id: u64, model: &str, status: u16) -> NewRequest {
        let mut request = sample(model, status);
        request.credential_id = Some(id);
        request.account = format!("#{id}");
        request
    }

    fn account<'a>(report: &'a AccountCostReport, id: u64) -> &'a AccountCost {
        report.accounts.iter().find(|row| row.credential_id == id).unwrap()
    }

    #[test]
    fn account_usage_counts_only_successful_tokens_and_reprices_with_current_price() {
        let log = UsageLog::open(None);
        log.record(on_credential(1, "claude-opus-5-5", 200));
        log.record(on_credential(1, "claude-opus-5-5", 502));
        log.record(on_credential(2, "claude-opus-5-5", 200));
        let labels = vec![(1, "a@example.com".to_string()), (2, "#2".to_string())];

        // 还没定价：请求和 token 已记下，消耗为 0，未定价 1 条
        let before = log.account_report(&labels);
        let one = account(&before, 1);
        assert_eq!((one.requests, one.errors, one.unpriced_requests), (2, 1, 1));
        assert_eq!(one.input_tokens, 1000, "failed request tokens are not billed");
        assert_eq!(one.usage_usd, 0.0);

        // 定价后历史立即按新单价算：1000*15 + 500*75 = 52500 / 1e6
        log.upsert_price(price()).unwrap();
        let priced = log.account_report(&labels);
        assert!((account(&priced, 1).usage_usd - 0.0525).abs() < 1e-9);
        assert_eq!(account(&priced, 1).unpriced_requests, 0);

        // 改单价（含缓存价）后再读，同一批历史按新单价重算
        let mut cheaper = price();
        cheaper.input_per_m = 3.0;
        cheaper.output_per_m = 15.0;
        cheaper.cache_read_per_m = Some(0.3);
        log.upsert_price(cheaper).unwrap();
        let repriced = log.account_report(&labels);
        assert!((account(&repriced, 1).usage_usd - 0.0105).abs() < 1e-9, "{}", account(&repriced, 1).usage_usd);
    }

    #[test]
    fn cost_ratio_is_cny_over_usd_and_ignores_unset_accounts() {
        let log = UsageLog::open(None);
        log.upsert_price(price()).unwrap();
        for _ in 0..20 {
            log.record(on_credential(1, "claude-opus-5-5", 200));
        }
        for _ in 0..20 {
            log.record(on_credential(2, "claude-opus-5-5", 200));
        }
        log.record(on_credential(3, "claude-opus-5-5", 200));
        let labels = vec![(1, "#1".to_string()), (2, "#2".to_string()), (3, "#3".to_string())];
        // 每个账号 20 条 → 20 * 0.0525 = 1.05 美元
        log.set_account_cost(1, Some(2.1)).unwrap();
        log.set_account_cost(2, Some(0.0)).unwrap();
        let report = log.account_report(&labels);
        assert!((account(&report, 1).cost_ratio.unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(account(&report, 2).cost_ratio, None, "free account has no ratio");
        assert_eq!(account(&report, 3).cost_cny, None);
        assert_eq!(report.costed_accounts, 2);
        assert!((report.cost_cny - 2.1).abs() < 1e-9);
        // 合计：2.1 元 ÷（1.05+1.05）美元；未填成本的 #3 不进分母
        assert!((report.costed_usage_usd - 2.1).abs() < 1e-9);
        assert!((report.cost_ratio.unwrap() - 1.0).abs() < 1e-9);
        assert!(report.usage_usd > report.costed_usage_usd);

        assert!(log.set_account_cost(1, Some(-1.0)).is_err());
        log.set_account_cost(1, None).unwrap();
        assert_eq!(account(&log.account_report(&labels), 1).cost_cny, None);
    }

    #[test]
    fn removed_credentials_keep_their_history() {
        let log = UsageLog::open(None);
        log.record(on_credential(9, "claude-opus-5-5", 200));
        log.set_account_cost(9, Some(10.0)).unwrap();
        let report = log.account_report(&[(1, "#1".to_string())]);
        let gone = account(&report, 9);
        assert!(gone.removed);
        assert_eq!(gone.label, "#9");
        assert_eq!(gone.cost_cny, Some(10.0));
        assert!(!account(&report, 1).removed);
    }

    #[test]
    fn backfill_maps_old_rows_by_label_once() {
        let log = UsageLog::open(None);
        // 升级前的记录：没有 credential_id，只有 account 标签
        let mut by_email = sample("Claude-Opus-5-5", 200);
        by_email.account = "a@example.com".into();
        log.record(by_email);
        let mut by_index = sample("claude-opus-5-5", 200);
        by_index.account = "#2".into();
        log.record(by_index);
        let mut failed = sample("claude-opus-5-5", 502);
        failed.account = "#2".into();
        log.record(failed);
        let mut unknown = sample("claude-opus-5-5", 200);
        unknown.account = "gone@example.com".into();
        log.record(unknown);

        let emails = HashMap::from([("a@example.com".to_string(), 1u64)]);
        assert_eq!(log.backfill_accounts(&emails), 3);
        assert_eq!(log.backfill_accounts(&emails), 0, "second run adds nothing");

        let labels = vec![(1, "a@example.com".to_string()), (2, "#2".to_string())];
        let report = log.account_report(&labels);
        assert_eq!(account(&report, 1).requests, 1);
        assert_eq!(account(&report, 1).input_tokens, 1000, "model alias normalized");
        assert_eq!((account(&report, 2).requests, account(&report, 2).errors), (2, 1));
        assert_eq!(account(&report, 2).input_tokens, 1000);
        assert_eq!(report.accounts.len(), 2, "unmapped label is left alone");
    }

    #[test]
    fn totals_survive_after_request_rows_are_pruned() {
        let log = UsageLog::open_with_retain(None, 2);
        log.record(sample("claude-opus-5-5", 200));
        log.record(sample("claude-opus-5-5", 200));
        log.record(sample("claude-opus-5-5", 500));
        assert_eq!(log.list(10).len(), 2);
        let summary = log.summary();
        assert_eq!(summary.requests, 3);
        assert_eq!(summary.errors, 1);
        // 失败的那条只计次数，token 不进汇总
        assert_eq!(summary.input_tokens, 2000);
    }

    #[test]
    fn failed_requests_are_never_billed() {
        let log = UsageLog::open(None);
        log.upsert_price(price()).unwrap();
        log.record(sample("claude-opus-5-5", 200));
        let mut broken = sample("claude-opus-5-5", 502);
        broken.error = Some("上游流中断: error decoding response body".into());
        log.record(broken);

        let rows = log.list(10);
        let failed = rows.iter().find(|row| row.record.status == 502).unwrap();
        assert_eq!(failed.cost_usd, Some(0.0));
        assert_eq!(failed.record.input_tokens, 1000, "tokens are kept for diagnosis");
        let ok = rows.iter().find(|row| row.record.status == 200).unwrap();
        assert!((ok.cost_usd.unwrap() - 0.0525).abs() < 1e-9);
        assert_eq!(log.get(failed.record.id).unwrap().view.cost_usd, Some(0.0));

        let summary = log.summary();
        assert_eq!((summary.requests, summary.errors), (2, 1));
        assert!((summary.cost_usd - 0.0525).abs() < 1e-9, "{}", summary.cost_usd);
    }

    #[test]
    fn old_failed_tokens_are_removed_from_totals_once() {
        let log = UsageLog::open(None);
        log.record(sample("claude-opus-5-5", 200));
        log.record(sample("Claude-Opus-5-5", 502));
        {
            // 模拟升级前：失败 token 也进过汇总，且还没做过扣回
            let conn = log.conn.lock();
            conn.execute(
                "UPDATE model_totals SET input_tokens = input_tokens + 1000, output_tokens = output_tokens + 500",
                [],
            )
            .unwrap();
            conn.execute("DELETE FROM meta WHERE key = 'failed_tokens_excluded'", []).unwrap();
        }
        assert_eq!(log.summary().input_tokens, 2000);
        exclude_failed_tokens_from_totals(&log.conn.lock()).unwrap();
        assert_eq!(log.summary().input_tokens, 1000);
        assert_eq!(log.summary().output_tokens, 500);
        exclude_failed_tokens_from_totals(&log.conn.lock()).unwrap();
        assert_eq!(log.summary().input_tokens, 1000, "runs only once");
    }

    #[test]
    fn detail_keeps_headers_and_redacts_secrets() {
        let headers = encode_headers([
            ("user-agent", "cursor"),
            ("x-api-key", "secret-value"),
            ("Authorization", "Bearer secret"),
        ]);
        assert!(headers.contains("cursor"));
        assert!(!headers.contains("secret-value"));
        assert!(!headers.contains("Bearer secret"));
        assert!(headers.contains("[redacted]"));

        let log = UsageLog::open(None);
        let mut request = sample("claude-opus-5-5", 200);
        request.inbound_headers = headers;
        request.inbound_body = "{\"messages\":[]}".into();
        request.outbound_headers = "{\"host\":\"q.example\"}".into();
        request.outbound_body = "{\"conversationState\":{}}".into();
        request.response_body = "{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}".into();
        log.record(request);
        let listed = &log.list(10)[0];
        assert_eq!(listed.record.account, "preview@example.com");
        assert_eq!(listed.record.request_bytes, 128);
        let detail = log.get(listed.record.id).unwrap();
        assert!(detail.inbound_body.contains("messages"));
        assert!(detail.outbound_body.contains("conversationState"));
        assert!(detail.response_body.contains("hello"));
        assert!(!detail.inbound_headers.contains("secret"));
        assert!(detail.payload_profile.contains("history=0"));
    }

    #[test]
    fn payload_profile_names_the_largest_field_without_its_text() {
        let long = "a".repeat(4000);
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let body = format!(
            r#"{{"conversationState":{{"history":[{{"userInputMessage":{{"content":"{long}","images":[{{"format":"png","source":{{"bytes":"{png}"}}}}]}}}}],"currentMessage":{{"userInputMessage":{{"content":"hi"}}}}}}}}"#
        );
        let profile = payload_profile(&body);
        assert!(profile.contains("largest conversationState.history[0].userInputMessage.content 4000"), "{profile}");
        assert!(profile.contains("png 1x1"), "{profile}");
        assert!(!profile.contains(&long), "{profile}");
        assert!(profile.contains("m0 user"));
    }
}
