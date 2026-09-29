//! 请求记录和模型单价。
//!
//! 明细和单价放在 SQLite。每条请求只插入一行，并累加模型汇总。
//! 明细超过保留上限后删掉最旧的，汇总不删，所以请求变多也不会重写整份历史。
//! token 数沿用代理里已有的估算，不是 Kiro 账单。价格按每百万 token 计算，不含缓存。

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
    pub account: String,
    pub endpoint: String,
    pub request_bytes: u64,
    pub stop_reason: String,
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
    pub inbound_headers: String,
    pub inbound_body: String,
    pub outbound_headers: String,
    pub outbound_body: String,
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
        let error = request.error.filter(|err| !err.is_empty());
        let account = clip(&request.account, 200);
        let endpoint = clip(&request.endpoint, 80);
        let stop_reason = clip(&request.stop_reason, 80);
        let errors: i64 = if request.status >= 400 { 1 } else { 0 };
        let tx = match conn.unchecked_transaction() {
            Ok(tx) => tx,
            Err(err) => {
                tracing::warn!("记录请求失败: {}", err);
                return;
            }
        };
        let inserted = tx.execute(
            "INSERT INTO requests(time, model, stream, status, duration_ms, input_tokens, output_tokens, error, account, endpoint, request_bytes, stop_reason, inbound_headers, inbound_body, outbound_headers, outbound_body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
                clip(&request.outbound_body, MAX_BLOB),
            ],
        );
        if let Err(err) = inserted {
            tracing::warn!("记录请求失败: {}", err);
            return;
        }
        if let Err(err) = tx.execute(
            "INSERT INTO model_totals(model, requests, errors, input_tokens, output_tokens)
             VALUES (?1, 1, ?2, ?3, ?4)
             ON CONFLICT(model) DO UPDATE SET
               requests = requests + 1,
               errors = errors + excluded.errors,
               input_tokens = input_tokens + excluded.input_tokens,
               output_tokens = output_tokens + excluded.output_tokens",
            params![total_key, errors, input_tokens, output_tokens],
        ) {
            tracing::warn!("累计用量失败: {}", err);
            return;
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
            "SELECT id, time, model, stream, status, duration_ms, input_tokens, output_tokens, error, account, endpoint, request_bytes, stop_reason
             FROM requests ORDER BY id DESC LIMIT ?1",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                tracing::warn!("读取请求失败: {}", err);
                return Vec::new();
            }
        };
        let rows = stmt.query_map(params![limit], |row| {
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
            })
        });
        let Ok(rows) = rows else {
            return Vec::new();
        };
        rows.filter_map(|row| row.ok())
            .map(|record| RequestView {
                cost_usd: charge(&prices, &record.model, record.input_tokens, record.output_tokens),
                record,
            })
            .collect()
    }

    pub fn get(&self, id: u64) -> Option<RequestDetail> {
        let conn = self.conn.lock();
        let prices = load_prices(&conn).unwrap_or_default();
        let row = conn
            .query_row(
                "SELECT id, time, model, stream, status, duration_ms, input_tokens, output_tokens, error, account, endpoint, request_bytes, stop_reason, inbound_headers, inbound_body, outbound_headers, outbound_body
                 FROM requests WHERE id = ?1",
                params![id as i64],
                |row| {
                    let record = RequestRecord {
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
                    };
                    Ok(RequestDetail {
                        inbound_headers: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
                        inbound_body: row.get::<_, Option<String>>(14)?.unwrap_or_default(),
                        outbound_headers: row.get::<_, Option<String>>(15)?.unwrap_or_default(),
                        outbound_body: row.get::<_, Option<String>>(16)?.unwrap_or_default(),
                        view: RequestView {
                            cost_usd: charge(
                                &prices,
                                &record.model,
                                record.input_tokens,
                                record.output_tokens,
                            ),
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
            "SELECT model, requests, errors, input_tokens, output_tokens FROM model_totals",
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
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        });
        let Ok(rows) = rows else {
            return empty_summary();
        };
        let mut by_model = Vec::new();
        let mut unpriced_requests = 0;
        for row in rows.flatten() {
            let (model, requests, errors, input_tokens, output_tokens) = row;
            let cost = charge(&prices, &model, input_tokens, output_tokens);
            if cost.is_none() {
                unpriced_requests += requests.max(0) as u64;
            }
            by_model.push(ModelUsage {
                model,
                requests: requests.max(0) as u64,
                errors: errors.max(0) as u64,
                input_tokens,
                output_tokens,
                cost_usd: cost.unwrap_or(0.0),
            });
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
            "INSERT INTO prices(id, name, aliases, input_per_m, output_per_m)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name,
               aliases = excluded.aliases,
               input_per_m = excluded.input_per_m,
               output_per_m = excluded.output_per_m",
            params![price.id, price.name, aliases, price.input_per_m, price.output_per_m],
        )
        .map_err(|err| err.to_string())?;
        Ok(price)
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
    Ok(())
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
    let mut stmt = conn.prepare("SELECT id, name, aliases, input_per_m, output_per_m FROM prices")?;
    let rows = stmt.query_map([], |row| {
        let aliases: String = row.get(2)?;
        Ok(ModelPrice {
            id: row.get(0)?,
            name: row.get(1)?,
            aliases: serde_json::from_str(&aliases).unwrap_or_default(),
            input_per_m: row.get(3)?,
            output_per_m: row.get(4)?,
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

pub fn charge(prices: &[ModelPrice], model: &str, input_tokens: i64, output_tokens: i64) -> Option<f64> {
    let model = normalize_alias(model);
    let price = prices.iter().find(|price| price.aliases.iter().any(|alias| alias == &model))?;
    let million = 1_000_000.0;
    Some(input_tokens.max(0) as f64 / million * price.input_per_m + output_tokens.max(0) as f64 / million * price.output_per_m)
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
        }
    }

    fn sample(model: &str, status: u16) -> NewRequest {
        NewRequest {
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
            inbound_headers: String::new(),
            inbound_body: String::new(),
            outbound_headers: String::new(),
            outbound_body: String::new(),
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
        assert_eq!(summary.input_tokens, 3000);
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
        log.record(request);
        let listed = &log.list(10)[0];
        assert_eq!(listed.record.account, "preview@example.com");
        assert_eq!(listed.record.request_bytes, 128);
        let detail = log.get(listed.record.id).unwrap();
        assert!(detail.inbound_body.contains("messages"));
        assert!(detail.outbound_body.contains("conversationState"));
        assert!(!detail.inbound_headers.contains("secret"));
    }
}
