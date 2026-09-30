//! 凭据存储（accounts.sqlite）
//!
//! 凭据原来只存在 credentials.json，进程刷新 token 时会把内存里的内容整份写回，
//! 所以在线改文件会被覆盖，改代理只能停服务。现在凭据存进和 credentials.json 同目录的
//! accounts.sqlite：
//!
//! - 库里还没有凭据时，启动从 credentials.json 导入一次；之后以库为准。
//! - credentials.json 导入后不再读写，原样留作备份。
//! - 管理端的增删改和 token 刷新都直接写库，改代理不需要重启。
//!
//! 每条凭据整条存成 JSON，字段和 credentials.json 一致，将来加字段不用迁移表结构。

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use rusqlite::{Connection, params};

use crate::kiro::model::credentials::KiroCredentials;

pub const DB_FILE: &str = "accounts.sqlite";

pub struct CredentialStore {
    conn: Mutex<Connection>,
    #[allow(dead_code)]
    path: Option<PathBuf>,
}

impl CredentialStore {
    /// 打开 `dir/accounts.sqlite`，不存在就创建
    pub fn open_in(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(DB_FILE);
        let conn = Connection::open(&path)?;
        restrict_permissions(&path);
        Self::init(conn, Some(path))
    }

    /// 仅测试用：内存库
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::init(Connection::open_in_memory().unwrap(), None).unwrap()
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> anyhow::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS credentials (
                id INTEGER PRIMARY KEY,
                position INTEGER NOT NULL,
                data TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    #[allow(dead_code)]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.conn
            .lock()
            .query_row("SELECT COUNT(*) FROM credentials", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count == 0)
            .unwrap_or(true)
    }

    /// 按保存时的顺序读出全部凭据
    pub fn load(&self) -> anyhow::Result<Vec<KiroCredentials>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, data FROM credentials ORDER BY position, id")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, data) = row?;
            let mut cred: KiroCredentials = serde_json::from_str(&data)
                .map_err(|err| anyhow::anyhow!("凭据 #{id} 数据损坏: {err}"))?;
            cred.id = Some(id as u64);
            out.push(cred);
        }
        Ok(out)
    }

    /// 整份替换。凭据只有几十条，一个事务里删了重写最简单，也不会留下半截状态。
    pub fn save(&self, credentials: &[KiroCredentials]) -> anyhow::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM credentials", [])?;
        for (position, cred) in credentials.iter().enumerate() {
            let id = cred
                .id
                .ok_or_else(|| anyhow::anyhow!("凭据缺少 id，不能入库"))?;
            tx.execute(
                "INSERT INTO credentials(id, position, data, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    id as i64,
                    position as i64,
                    serde_json::to_string(cred)?,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// 库里有 refresh token 和代理密码，只让属主读写
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!("设置 {:?} 权限失败: {}", path, err);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的假密钥，按用途拼出来
    fn fake(kind: &str, id: u64) -> Option<String> {
        Some(format!("{kind}-{id}"))
    }

    fn cred(id: u64, email: &str) -> KiroCredentials {
        let mut cred = KiroCredentials {
            id: Some(id),
            email: Some(email.into()),
            proxy_url: Some("socks5://gate.example:1000".into()),
            proxy_username: Some("user-a".into()),
            priority: id as u32,
            ..Default::default()
        };
        cred.refresh_token = fake("rt", id);
        cred.proxy_password = fake("pass", id);
        cred
    }

    #[test]
    fn save_and_load_keep_order_and_fields() {
        let store = CredentialStore::in_memory();
        assert!(store.is_empty());
        store.save(&[cred(5, "b@x"), cred(2, "a@x")]).unwrap();
        assert!(!store.is_empty());
        let loaded = store.load().unwrap();
        assert_eq!(
            loaded.iter().map(|c| c.id.unwrap()).collect::<Vec<_>>(),
            vec![5, 2]
        );
        assert_eq!(loaded[0].email.as_deref(), Some("b@x"));
        assert_eq!(loaded[1].proxy_username.as_deref(), Some("user-a"));
        assert_eq!(loaded[1].proxy_password.as_deref(), Some("pass-2"));
        assert_eq!(loaded[1].refresh_token.as_deref(), Some("rt-2"));
    }

    #[test]
    fn save_replaces_everything() {
        let store = CredentialStore::in_memory();
        store.save(&[cred(1, "a@x"), cred(2, "b@x")]).unwrap();
        store.save(&[cred(2, "b2@x")]).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].email.as_deref(), Some("b2@x"));
    }

    #[test]
    fn credentials_without_id_are_rejected() {
        let store = CredentialStore::in_memory();
        let mut missing = cred(1, "a@x");
        missing.id = None;
        assert!(store.save(&[missing]).is_err());
        assert!(store.is_empty(), "failed save leaves the table untouched");
    }
}
