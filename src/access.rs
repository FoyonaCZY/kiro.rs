//! 接入 Key 和调度分组。
//!
//! 客户端拿接入 Key 调用 /v1。Key 绑定一个分组，分组决定能用哪些凭据。
//! 默认分组不能删。没有加入任何分组的凭据，视为默认分组的成员。
//! 配置里的初始 apiKey 在库为空时导入为第一把 Key，名字是「默认」。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use tokio::task_local;

use crate::common::auth;

struct RequestScope {
    group_id: String,
    allowed: HashSet<u64>,
    key_id: Option<i64>,
}

task_local! {
    static REQUEST_SCOPE: RequestScope;
}

const DEFAULT_GROUP_ID: &str = "default";
const DEFAULT_GROUP_NAME: &str = "默认";
const DEFAULT_KEY_NAME: &str = "默认";

pub fn current_group() -> Option<String> {
    REQUEST_SCOPE
        .try_with(|scope| scope.group_id.clone())
        .ok()
}

pub fn credential_allowed(id: u64) -> bool {
    REQUEST_SCOPE
        .try_with(|scope| scope.allowed.contains(&id))
        .unwrap_or(true)
}

/// 当前请求使用的接入 Key，模拟缓存按它隔离。
pub fn current_key_id() -> Option<i64> {
    REQUEST_SCOPE.try_with(|scope| scope.key_id).ok().flatten()
}

/// 不带接入 Key 的旧入口，只供测试构造分组作用域。
#[cfg(test)]
pub async fn run_with_credentials<F>(group_id: impl Into<String>, allowed: HashSet<u64>, future: F) -> F::Output
where
    F: std::future::Future,
{
    run_with_access_key(None, group_id, allowed, future).await
}

pub async fn run_with_access_key<F>(
    key_id: Option<i64>,
    group_id: impl Into<String>,
    allowed: HashSet<u64>,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    REQUEST_SCOPE
        .scope(
            RequestScope {
                group_id: group_id.into(),
                allowed,
                key_id,
            },
            future,
        )
        .await
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessKey {
    pub id: i64,
    pub name: String,
    pub secret: String,
    pub prefix: String,
    pub group_id: String,
    pub group_name: String,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessGroup {
    pub id: String,
    pub name: String,
    pub key_count: i64,
    pub members: Vec<u64>,
    pub is_default: bool,
}

pub struct AccessStore {
    conn: Mutex<Connection>,
}

impl AccessStore {
    pub fn open(path: Option<PathBuf>) -> Arc<Self> {
        let conn = match &path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                Connection::open(path).unwrap_or_else(|err| {
                    tracing::error!("打开接入库失败，改用内存库: {}", err);
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
            tracing::error!("初始化接入库失败: {}", err);
        }
        Arc::new(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn import_initial_key(&self, secret: &str) {
        let secret = secret.trim();
        if secret.is_empty() {
            return;
        }
        let conn = self.conn.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM access_keys", [], |row| row.get(0))
            .unwrap_or(0);
        if count > 0 {
            return;
        }
        if let Err(err) = conn.execute(
            "INSERT INTO access_keys(name, secret, group_id, disabled, created_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![
                DEFAULT_KEY_NAME,
                secret,
                DEFAULT_GROUP_ID,
                chrono::Utc::now().to_rfc3339()
            ],
        ) {
            tracing::error!("导入初始接入 Key 失败: {}", err);
        }
    }

    pub fn authenticate(&self, secret: &str) -> Option<AccessKey> {
        let keys = self.list_keys();
        let mut found = None;
        for key in keys {
            if auth::constant_time_eq(&key.secret, secret) && !key.disabled && found.is_none() {
                found = Some(key);
            }
        }
        found
    }

    pub fn allowed_credentials(&self, group_id: &str, credential_ids: &[u64]) -> HashSet<u64> {
        let conn = self.conn.lock();
        let membership = load_membership(&conn);
        let in_any = membership
            .values()
            .flat_map(|ids| ids.iter().copied())
            .collect::<HashSet<_>>();
        let explicit = membership.get(group_id).cloned().unwrap_or_default();
        credential_ids
            .iter()
            .copied()
            .filter(|id| {
                explicit.contains(id) || (group_id == DEFAULT_GROUP_ID && !in_any.contains(id))
            })
            .collect()
    }

    pub fn list_keys(&self) -> Vec<AccessKey> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT k.id, k.name, k.secret, k.group_id, g.name, k.disabled
             FROM access_keys k
             LEFT JOIN access_groups g ON g.id = k.group_id
             ORDER BY k.id",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                tracing::warn!("读取接入 Key 失败: {}", err);
                return Vec::new();
            }
        };
        let rows = stmt.query_map([], |row| {
            let secret: String = row.get(2)?;
            Ok(AccessKey {
                id: row.get(0)?,
                name: row.get(1)?,
                prefix: prefix(&secret),
                secret,
                group_id: row.get(3)?,
                group_name: row.get::<_, Option<String>>(4)?.unwrap_or_else(|| "未知分组".into()),
                disabled: row.get::<_, i64>(5)? != 0,
            })
        });
        rows.into_iter()
            .flatten()
            .flatten()
            .collect()
    }

    pub fn create_key(&self, name: &str, secret: &str, group_id: &str) -> Result<AccessKey, String> {
        let name = clean_name(name);
        if name.is_empty() {
            return Err("名称不能为空".into());
        }
        let secret = if secret.trim().is_empty() {
            format!("sk-{}", uuid::Uuid::new_v4().simple())
        } else {
            secret.trim().to_string()
        };
        let group_id = normalize_group(group_id);
        let conn = self.conn.lock();
        ensure_group_exists(&conn, &group_id)?;
        conn.execute(
            "INSERT INTO access_keys(name, secret, group_id, disabled, created_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![name, secret, group_id, chrono::Utc::now().to_rfc3339()],
        )
        .map_err(|err| err.to_string())?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.list_keys()
            .into_iter()
            .find(|key| key.id == id)
            .ok_or_else(|| "写入后读取失败".into())
    }

    pub fn update_key(
        &self,
        id: i64,
        name: Option<&str>,
        group_id: Option<&str>,
        disabled: Option<bool>,
    ) -> Result<(), String> {
        let conn = self.conn.lock();
        let exists: Option<i64> = conn
            .query_row(
                "SELECT id FROM access_keys WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| err.to_string())?;
        if exists.is_none() {
            return Err("接入 Key 不存在".into());
        }
        if let Some(name) = name {
            let name = clean_name(name);
            if name.is_empty() {
                return Err("名称不能为空".into());
            }
            conn.execute(
                "UPDATE access_keys SET name = ?1 WHERE id = ?2",
                params![name, id],
            )
            .map_err(|err| err.to_string())?;
        }
        if let Some(group_id) = group_id {
            let group_id = normalize_group(group_id);
            ensure_group_exists(&conn, &group_id)?;
            conn.execute(
                "UPDATE access_keys SET group_id = ?1 WHERE id = ?2",
                params![group_id, id],
            )
            .map_err(|err| err.to_string())?;
        }
        if let Some(disabled) = disabled {
            conn.execute(
                "UPDATE access_keys SET disabled = ?1 WHERE id = ?2",
                params![disabled as i64, id],
            )
            .map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    pub fn delete_key(&self, id: i64) -> Result<(), String> {
        let conn = self.conn.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM access_keys", [], |row| row.get(0))
            .map_err(|err| err.to_string())?;
        if count <= 1 {
            return Err("不能删除最后一把接入 Key".into());
        }
        let changed = conn
            .execute("DELETE FROM access_keys WHERE id = ?1", params![id])
            .map_err(|err| err.to_string())?;
        if changed == 0 {
            return Err("接入 Key 不存在".into());
        }
        Ok(())
    }

    pub fn assigned_credentials(&self) -> HashSet<u64> {
        let conn = self.conn.lock();
        load_membership(&conn)
            .into_values()
            .flatten()
            .collect()
    }

    pub fn list_groups(&self) -> Vec<AccessGroup> {
        let conn = self.conn.lock();
        let membership = load_membership(&conn);
        let mut stmt = match conn.prepare(
            "SELECT g.id, g.name, COUNT(k.id)
             FROM access_groups g
             LEFT JOIN access_keys k ON k.group_id = g.id
             GROUP BY g.id
             ORDER BY CASE WHEN g.id = 'default' THEN 0 ELSE 1 END, g.id",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            Ok(AccessGroup {
                members: membership
                    .get(&id)
                    .map(|ids| {
                        let mut ids: Vec<_> = ids.iter().copied().collect();
                        ids.sort_unstable();
                        ids
                    })
                    .unwrap_or_default(),
                is_default: id == DEFAULT_GROUP_ID,
                id,
                name: row.get(1)?,
                key_count: row.get(2)?,
            })
        });
        rows.into_iter().flatten().flatten().collect()
    }

    pub fn create_group(&self, name: &str) -> Result<AccessGroup, String> {
        let name = clean_name(name);
        if name.is_empty() {
            return Err("分组名不能为空".into());
        }
        let id = format!("g_{}", uuid::Uuid::new_v4().simple());
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO access_groups(id, name, created_at) VALUES (?1, ?2, ?3)",
            params![id, name, chrono::Utc::now().to_rfc3339()],
        )
        .map_err(|err| err.to_string())?;
        Ok(AccessGroup {
            id,
            name,
            key_count: 0,
            members: Vec::new(),
            is_default: false,
        })
    }

    pub fn rename_group(&self, id: &str, name: &str) -> Result<(), String> {
        let name = clean_name(name);
        if name.is_empty() {
            return Err("分组名不能为空".into());
        }
        let conn = self.conn.lock();
        let changed = conn
            .execute(
                "UPDATE access_groups SET name = ?1 WHERE id = ?2",
                params![name, id],
            )
            .map_err(|err| err.to_string())?;
        if changed == 0 {
            return Err("分组不存在".into());
        }
        Ok(())
    }

    pub fn delete_group(&self, id: &str) -> Result<(), String> {
        if id == DEFAULT_GROUP_ID {
            return Err("不能删除默认分组".into());
        }
        let conn = self.conn.lock();
        let keys: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM access_keys WHERE group_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|err| err.to_string())?;
        if keys > 0 {
            return Err("仍有接入 Key 绑定该分组".into());
        }
        conn.execute(
            "DELETE FROM group_members WHERE group_id = ?1",
            params![id],
        )
        .map_err(|err| err.to_string())?;
        let changed = conn
            .execute("DELETE FROM access_groups WHERE id = ?1", params![id])
            .map_err(|err| err.to_string())?;
        if changed == 0 {
            return Err("分组不存在".into());
        }
        Ok(())
    }

    pub fn set_members(
        &self,
        group_id: &str,
        members: &[u64],
        known_credentials: &[u64],
    ) -> Result<(), String> {
        let group_id = normalize_group(group_id);
        let conn = self.conn.lock();
        ensure_group_exists(&conn, &group_id)?;
        if group_id == DEFAULT_GROUP_ID {
            let membership = load_membership(&conn);
            let wanted: HashSet<u64> = members.iter().copied().collect();
            for id in known_credentials {
                if !wanted.contains(id) && !membership_elsewhere(&membership, DEFAULT_GROUP_ID, *id) {
                    return Err("凭据未加入其他分组，不能移出默认组".into());
                }
            }
        }
        let tx = conn.unchecked_transaction().map_err(|err| err.to_string())?;
        tx.execute(
            "DELETE FROM group_members WHERE group_id = ?1",
            params![group_id],
        )
        .map_err(|err| err.to_string())?;
        for id in members {
            tx.execute(
                "INSERT INTO group_members(group_id, credential_id) VALUES (?1, ?2)",
                params![group_id, *id as i64],
            )
            .map_err(|err| err.to_string())?;
        }
        tx.commit().map_err(|err| err.to_string())?;
        Ok(())
    }
}

fn membership_elsewhere(membership: &HashMap<String, HashSet<u64>>, except: &str, id: u64) -> bool {
    membership
        .iter()
        .any(|(group, ids)| group != except && ids.contains(&id))
}

fn load_membership(conn: &Connection) -> HashMap<String, HashSet<u64>> {
    let mut out: HashMap<String, HashSet<u64>> = HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT group_id, credential_id FROM group_members") else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
    }) else {
        return out;
    };
    for row in rows.flatten() {
        out.entry(row.0).or_default().insert(row.1);
    }
    out
}

fn ensure_group_exists(conn: &Connection, id: &str) -> Result<(), String> {
    let found: Option<String> = conn
        .query_row(
            "SELECT id FROM access_groups WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| err.to_string())?;
    if found.is_none() {
        return Err("分组不存在".into());
    }
    Ok(())
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS access_groups (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS access_keys (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            secret TEXT NOT NULL,
            group_id TEXT NOT NULL,
            disabled INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS group_members (
            group_id TEXT NOT NULL,
            credential_id INTEGER NOT NULL,
            PRIMARY KEY (group_id, credential_id)
        );",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO access_groups(id, name, created_at) VALUES (?1, ?2, ?3)",
        params![
            DEFAULT_GROUP_ID,
            DEFAULT_GROUP_NAME,
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn normalize_group(id: &str) -> String {
    let id = id.trim();
    if id.is_empty() {
        DEFAULT_GROUP_ID.to_string()
    } else {
        id.to_string()
    }
}

fn clean_name(name: &str) -> String {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    name.chars().take(64).collect()
}

fn prefix(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 10 {
        return "••••".into();
    }
    format!(
        "{}…{}",
        chars.iter().take(4).collect::<String>(),
        chars.iter().skip(chars.len() - 4).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_key_becomes_the_default_key() {
        let store = AccessStore::open(None);
        store.import_initial_key("initial-secret");
        store.import_initial_key("later-secret");
        let keys = store.list_keys();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "默认");
        assert_eq!(keys[0].secret, "initial-secret");
        assert_eq!(keys[0].group_id, "default");
        assert!(store.authenticate("initial-secret").is_some());
        assert!(store.authenticate("later-secret").is_none());
    }

    #[test]
    fn default_group_includes_unassigned_credentials() {
        let store = AccessStore::open(None);
        let group = store.create_group("备用").unwrap();
        store.set_members(&group.id, &[2], &[]).unwrap();
        let allowed = store.allowed_credentials("default", &[1, 2, 3]);
        assert!(allowed.contains(&1));
        assert!(!allowed.contains(&2));
        assert!(allowed.contains(&3));
        let spare = store.allowed_credentials(&group.id, &[1, 2, 3]);
        assert_eq!(spare, HashSet::from([2]));
    }

    #[test]
    fn default_group_cannot_be_deleted_or_emptied() {
        let store = AccessStore::open(None);
        assert_eq!(store.delete_group("default").unwrap_err(), "不能删除默认分组");
        store.set_members("default", &[1], &[1]).unwrap();
        assert!(store.set_members("default", &[], &[1]).is_err());
    }
}
