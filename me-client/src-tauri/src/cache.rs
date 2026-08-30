use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct CacheDatabase {
    path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheSaveRequest {
    pub edb_id: String,
    pub start_order: u64,
    pub event_count: u64,
    pub expected_event_count: u64,
    pub expected_mutation_revision: Option<u64>,
    pub mutation_revision: u64,
    pub last_event_hash: Option<String>,
    pub events: Vec<Value>,
    #[serde(default)]
    pub reset: bool,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub gateway_label: String,
    #[serde(default)]
    pub workspace_label: String,
    #[serde(default)]
    pub session_label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheMetadata {
    pub key: String,
    pub edb_id: String,
    pub scope: String,
    pub agent_id: String,
    pub mutation_revision: u64,
    pub last_event_hash: Option<String>,
    pub event_count: u64,
    pub byte_size: u64,
    pub updated_at: u64,
    pub gateway_label: String,
    pub workspace_label: String,
    pub session_label: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheChunk {
    pub edb_id: String,
    pub start_order: u64,
    pub next_order: u64,
    pub total_count: u64,
    pub mutation_revision: u64,
    pub last_event_hash: Option<String>,
    pub events: Vec<Value>,
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RememberedDevice {
    pub endpoint: String,
    pub password: String,
    pub updated_at: u64,
}

#[derive(Debug)]
struct ExistingState {
    mutation_revision: u64,
    event_count: u64,
    byte_size: u64,
}

impl CacheDatabase {
    pub fn new(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("无法创建客户端数据目录：{error}"))?;
        }
        let database = Self { path };
        database.initialize()?;
        Ok(database)
    }

    fn connect(&self) -> Result<Connection, String> {
        let connection = Connection::open(&self.path)
            .map_err(|error| format!("无法打开客户端数据库：{error}"))?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
            )
            .map_err(|error| format!("无法配置客户端数据库：{error}"))?;
        Ok(connection)
    }

    fn initialize(&self) -> Result<(), String> {
        let connection = self.connect()?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS client_settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS remembered_devices (
                    endpoint TEXT PRIMARY KEY,
                    password TEXT NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS edb_sessions (
                    edb_id TEXT PRIMARY KEY,
                    scope TEXT NOT NULL,
                    agent_id TEXT NOT NULL,
                    mutation_revision INTEGER NOT NULL,
                    last_event_hash TEXT,
                    event_count INTEGER NOT NULL,
                    byte_size INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    gateway_label TEXT NOT NULL,
                    workspace_label TEXT NOT NULL,
                    session_label TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS edb_events (
                    edb_id TEXT NOT NULL,
                    event_order INTEGER NOT NULL,
                    event_json TEXT NOT NULL,
                    byte_size INTEGER NOT NULL,
                    PRIMARY KEY (edb_id, event_order),
                    FOREIGN KEY (edb_id) REFERENCES edb_sessions(edb_id) ON DELETE CASCADE
                );",
            )
            .map_err(|error| format!("无法初始化客户端数据库：{error}"))
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>, String> {
        self.connect()?
            .query_row(
                "SELECT value FROM client_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("无法读取客户端设置：{error}"))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.connect()?
            .execute(
                "INSERT INTO client_settings(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
            .map_err(|error| format!("无法保存客户端设置：{error}"))
    }

    pub fn remembered_devices(&self) -> Result<Vec<RememberedDevice>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT endpoint, password, updated_at
                 FROM remembered_devices ORDER BY updated_at DESC, endpoint ASC",
            )
            .map_err(|error| format!("无法读取已记住的设备：{error}"))?;
        statement
            .query_map([], |row| {
                Ok(RememberedDevice {
                    endpoint: row.get(0)?,
                    password: row.get(1)?,
                    updated_at: from_sql_u64(row.get::<_, i64>(2)?)?,
                })
            })
            .map_err(|error| format!("无法读取已记住的设备：{error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法读取已记住的设备：{error}"))
    }

    pub fn remember_device(
        &self,
        endpoint: &str,
        password: &str,
    ) -> Result<RememberedDevice, String> {
        let updated_at = now_ms();
        self.connect()?
            .execute(
                "INSERT INTO remembered_devices(endpoint, password, updated_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(endpoint) DO UPDATE SET
                    password = excluded.password, updated_at = excluded.updated_at",
                params![endpoint, password, to_i64(updated_at, "timestamp")?],
            )
            .map_err(|error| format!("无法记住设备：{error}"))?;
        Ok(RememberedDevice {
            endpoint: endpoint.to_owned(),
            password: password.to_owned(),
            updated_at,
        })
    }

    pub fn forget_device(&self, endpoint: &str) -> Result<(), String> {
        self.connect()?
            .execute(
                "DELETE FROM remembered_devices WHERE endpoint = ?1",
                params![endpoint],
            )
            .map(|_| ())
            .map_err(|error| format!("无法忘记设备：{error}"))
    }

    pub fn load_metadata(&self, edb_ids: &[String]) -> Result<Vec<CacheMetadata>, String> {
        let connection = self.connect()?;
        let mut entries = Vec::new();
        for edb_id in edb_ids {
            if !valid_edb_id(edb_id) {
                continue;
            }
            if let Some(metadata) = query_metadata(&connection, edb_id)? {
                entries.push(metadata);
            }
        }
        Ok(entries)
    }

    pub fn list(&self) -> Result<Vec<CacheMetadata>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT edb_id, scope, agent_id, mutation_revision, last_event_hash,
                        event_count, byte_size, updated_at, gateway_label, workspace_label,
                        session_label
                 FROM edb_sessions ORDER BY updated_at DESC",
            )
            .map_err(|error| format!("无法读取缓存列表：{error}"))?;
        statement
            .query_map([], metadata_from_row)
            .map_err(|error| format!("无法读取缓存列表：{error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法读取缓存列表：{error}"))
    }

    pub fn load_chunk(
        &self,
        edb_id: &str,
        start_order: u64,
        byte_limit: u64,
    ) -> Result<CacheChunk, String> {
        if !valid_edb_id(edb_id) {
            return Err("EDB_ID 必须是 64 位小写十六进制值".into());
        }
        if byte_limit == 0 {
            return Err("缓存读取预算必须大于零".into());
        }
        let connection = self.connect()?;
        let metadata =
            query_metadata(&connection, edb_id)?.ok_or_else(|| "缓存不存在".to_owned())?;
        if start_order > metadata.event_count {
            return Err("缓存读取位置超出 Event 范围".into());
        }
        if start_order == metadata.event_count {
            return Ok(CacheChunk {
                edb_id: edb_id.to_owned(),
                start_order,
                next_order: start_order,
                total_count: metadata.event_count,
                mutation_revision: metadata.mutation_revision,
                last_event_hash: metadata.last_event_hash,
                events: Vec::new(),
                done: true,
            });
        }

        let budget = byte_limit.min(MAX_CHUNK_BYTES);
        let read = (|| -> Result<(Vec<Value>, u64), String> {
            let mut statement = connection
                .prepare(
                    "SELECT event_order, event_json, byte_size FROM edb_events
                     WHERE edb_id = ?1 AND event_order >= ?2 ORDER BY event_order ASC",
                )
                .map_err(|error| format!("无法读取 EDB Event：{error}"))?;
            let mut rows = statement
                .query(params![edb_id, to_i64(start_order, "event order")?])
                .map_err(|error| format!("无法读取 EDB Event：{error}"))?;
            let mut events = Vec::new();
            let mut next_order = start_order;
            let mut used_bytes = 0_u64;
            while let Some(row) = rows
                .next()
                .map_err(|error| format!("无法读取 EDB Event：{error}"))?
            {
                let order = from_i64(
                    row.get::<_, i64>(0)
                        .map_err(|error| format!("无法读取 EDB Event：{error}"))?,
                    "event order",
                )?;
                let json = row
                    .get::<_, String>(1)
                    .map_err(|error| format!("无法读取 EDB Event：{error}"))?;
                let bytes = from_i64(
                    row.get::<_, i64>(2)
                        .map_err(|error| format!("无法读取 EDB Event：{error}"))?,
                    "event byte size",
                )?;
                if order != next_order {
                    return Err("缓存 EventOrder 不连续".into());
                }
                if !events.is_empty() && used_bytes.saturating_add(bytes) > budget {
                    break;
                }
                let event = serde_json::from_str(&json)
                    .map_err(|_| "缓存包含无效的 EDB Event".to_owned())?;
                events.push(event);
                used_bytes = used_bytes.saturating_add(bytes);
                next_order = next_order.saturating_add(1);
                if next_order >= metadata.event_count {
                    break;
                }
            }
            if events.is_empty() || next_order > metadata.event_count {
                return Err("缓存 Event 数量与元数据不一致".into());
            }
            if start_order == 0 && !identity_matches(&events, edb_id) {
                return Err("缓存首事件与 EDB_ID 不匹配".into());
            }
            Ok((events, next_order))
        })();

        let (events, next_order) = match read {
            Ok(read) => read,
            Err(error) => {
                delete_entry(&connection, edb_id)?;
                return Err(error);
            }
        };
        Ok(CacheChunk {
            edb_id: edb_id.to_owned(),
            start_order,
            next_order,
            total_count: metadata.event_count,
            mutation_revision: metadata.mutation_revision,
            last_event_hash: metadata.last_event_hash,
            events,
            done: next_order == metadata.event_count,
        })
    }

    pub fn remove(&self, edb_id: &str) -> Result<(), String> {
        if !valid_edb_id(edb_id) {
            return Ok(());
        }
        delete_entry(&self.connect()?, edb_id)
    }

    pub fn save(&self, request: CacheSaveRequest) -> Result<(), String> {
        validate_save_request(&request)?;
        let end_order = request
            .start_order
            .checked_add(request.events.len() as u64)
            .ok_or_else(|| "EventOrder 超出支持范围".to_owned())?;
        let serialized = request
            .events
            .iter()
            .map(|event| {
                serde_json::to_string(event)
                    .map(|json| {
                        let bytes = json.len() as u64;
                        (json, bytes)
                    })
                    .map_err(|error| format!("无法序列化 EDB Event：{error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let chunk_bytes = serialized
            .iter()
            .try_fold(0_u64, |total, (_, bytes)| total.checked_add(*bytes))
            .ok_or_else(|| "缓存大小超出支持范围".to_owned())?;

        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| format!("无法开始缓存事务：{error}"))?;
        let existing = transaction
            .query_row(
                "SELECT mutation_revision, event_count, byte_size
                 FROM edb_sessions WHERE edb_id = ?1",
                params![request.edb_id],
                |row| {
                    Ok(ExistingState {
                        mutation_revision: from_sql_u64(row.get(0)?)?,
                        event_count: from_sql_u64(row.get(1)?)?,
                        byte_size: from_sql_u64(row.get(2)?)?,
                    })
                },
            )
            .optional()
            .map_err(|error| format!("无法读取现有缓存：{error}"))?;

        let expected_matches = match &existing {
            Some(state) => {
                state.event_count == request.expected_event_count
                    && request
                        .expected_mutation_revision
                        .is_none_or(|revision| revision == state.mutation_revision)
            }
            None => request.expected_event_count == 0,
        };
        if !expected_matches {
            if existing.as_ref().is_some_and(|state| {
                state.mutation_revision > request.mutation_revision
                    || (state.mutation_revision == request.mutation_revision
                        && state.event_count >= end_order)
            }) {
                return Ok(());
            }
            return Err("缓存状态已变化，拒绝写入过期或不连续的批次".into());
        }
        if request.reset {
            if request.start_order != 0 {
                return Err("重置批次必须从 EventOrder 0 开始".into());
            }
            transaction
                .execute(
                    "DELETE FROM edb_events WHERE edb_id = ?1",
                    params![request.edb_id],
                )
                .map_err(|error| format!("无法替换旧缓存：{error}"))?;
        } else {
            if request.start_order != request.expected_event_count {
                return Err("增量批次起点与预期缓存长度不一致".into());
            }
            if existing
                .as_ref()
                .is_some_and(|state| state.mutation_revision != request.mutation_revision)
            {
                return Err("mutation revision 变化时必须重置缓存".into());
            }
        }

        let previous_bytes = if request.reset {
            0
        } else {
            existing.as_ref().map_or(0, |state| state.byte_size)
        };
        let byte_size = previous_bytes
            .checked_add(chunk_bytes)
            .ok_or_else(|| "缓存大小超出支持范围".to_owned())?;
        transaction
            .execute(
                "INSERT INTO edb_sessions(
                    edb_id, scope, agent_id, mutation_revision, last_event_hash,
                    event_count, byte_size, updated_at, gateway_label, workspace_label, session_label
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(edb_id) DO UPDATE SET
                    scope = excluded.scope,
                    agent_id = excluded.agent_id,
                    mutation_revision = excluded.mutation_revision,
                    last_event_hash = excluded.last_event_hash,
                    event_count = excluded.event_count,
                    byte_size = excluded.byte_size,
                    updated_at = excluded.updated_at,
                    gateway_label = excluded.gateway_label,
                    workspace_label = excluded.workspace_label,
                    session_label = excluded.session_label",
                params![
                    request.edb_id,
                    request.scope,
                    request.agent_id,
                    to_i64(request.mutation_revision, "mutation revision")?,
                    request.last_event_hash,
                    to_i64(end_order, "event count")?,
                    to_i64(byte_size, "cache byte size")?,
                    to_i64(now_ms(), "timestamp")?,
                    request.gateway_label,
                    request.workspace_label,
                    request.session_label,
                ],
            )
            .map_err(|error| format!("无法更新缓存元数据：{error}"))?;
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO edb_events(edb_id, event_order, event_json, byte_size)
                     VALUES(?1, ?2, ?3, ?4)",
                )
                .map_err(|error| format!("无法准备缓存写入：{error}"))?;
            for (index, (json, bytes)) in serialized.iter().enumerate() {
                let order = request.start_order + index as u64;
                insert
                    .execute(params![
                        request.edb_id,
                        to_i64(order, "event order")?,
                        json,
                        to_i64(*bytes, "event byte size")?,
                    ])
                    .map_err(|error| format!("无法写入 EDB Event：{error}"))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| format!("无法提交缓存事务：{error}"))
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn query_metadata(connection: &Connection, edb_id: &str) -> Result<Option<CacheMetadata>, String> {
    connection
        .query_row(
            "SELECT edb_id, scope, agent_id, mutation_revision, last_event_hash,
                    event_count, byte_size, updated_at, gateway_label, workspace_label,
                    session_label
             FROM edb_sessions WHERE edb_id = ?1",
            params![edb_id],
            metadata_from_row,
        )
        .optional()
        .map_err(|error| format!("无法读取缓存元数据：{error}"))
}

fn metadata_from_row(row: &Row<'_>) -> rusqlite::Result<CacheMetadata> {
    let edb_id = row.get::<_, String>(0)?;
    Ok(CacheMetadata {
        key: edb_id.clone(),
        edb_id,
        scope: row.get(1)?,
        agent_id: row.get(2)?,
        mutation_revision: from_sql_u64(row.get(3)?)?,
        last_event_hash: row.get(4)?,
        event_count: from_sql_u64(row.get(5)?)?,
        byte_size: from_sql_u64(row.get(6)?)?,
        updated_at: from_sql_u64(row.get(7)?)?,
        gateway_label: row.get(8)?,
        workspace_label: row.get(9)?,
        session_label: row.get(10)?,
    })
}

fn delete_entry(connection: &Connection, edb_id: &str) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM edb_sessions WHERE edb_id = ?1",
            params![edb_id],
        )
        .map(|_| ())
        .map_err(|error| format!("无法清除会话缓存：{error}"))
}

fn validate_save_request(request: &CacheSaveRequest) -> Result<(), String> {
    if !valid_edb_id(&request.edb_id) {
        return Err("EDB_ID 必须是 64 位小写十六进制值".into());
    }
    let end_order = request
        .start_order
        .checked_add(request.events.len() as u64)
        .ok_or_else(|| "EventOrder 超出支持范围".to_owned())?;
    if end_order > request.event_count {
        return Err("缓存批次超出权威 Event 总数".into());
    }
    if request.reset && request.start_order != 0 {
        return Err("重置批次必须从 EventOrder 0 开始".into());
    }
    if !request.reset && request.start_order != request.expected_event_count {
        return Err("增量批次起点与预期缓存长度不一致".into());
    }
    if end_order == 0 {
        if request.last_event_hash.is_some() {
            return Err("空缓存不能包含 Event Hash".into());
        }
    } else if request.last_event_hash.as_deref().is_none_or(str::is_empty) {
        return Err("非空缓存必须包含最后 Event Hash".into());
    }
    if request.start_order == 0
        && !request.events.is_empty()
        && !identity_matches(&request.events, &request.edb_id)
    {
        return Err("缓存首事件与 EDB_ID 不匹配".into());
    }
    Ok(())
}

fn identity_matches(events: &[Value], edb_id: &str) -> bool {
    events
        .first()
        .and_then(|event| event.get("EdbIdGeneration"))
        .and_then(|identity| identity.get("edb_id"))
        .and_then(Value::as_str)
        == Some(edb_id)
}

fn valid_edb_id(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn to_i64(value: u64, label: &str) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| format!("{label} 超出 SQLite 支持范围"))
}

fn from_i64(value: i64, label: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("{label} 包含无效的负值"))
}

fn from_sql_u64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_database(name: &str) -> CacheDatabase {
        let directory = std::env::var_os("ME_CLIENT_TEST_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/cache-tests")
            });
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!(
            "{name}-{}-{}.sqlite3",
            std::process::id(),
            now_ms()
        ));
        CacheDatabase::new(path).unwrap()
    }

    fn event(edb_id: &str, id: u64, content: &str) -> Value {
        if id == 0 {
            serde_json::json!({"EdbIdGeneration": {"id": 0, "timestamp_ms": 1, "edb_id": edb_id}})
        } else {
            serde_json::json!({"UserPrompt": {"id": id, "timestamp_ms": id + 1, "content": content}})
        }
    }

    fn request(
        edb_id: &str,
        start_order: u64,
        total_count: u64,
        expected_event_count: u64,
        expected_mutation_revision: Option<u64>,
        mutation_revision: u64,
        events: Vec<Value>,
        reset: bool,
    ) -> CacheSaveRequest {
        let end_order = start_order + events.len() as u64;
        CacheSaveRequest {
            edb_id: edb_id.into(),
            start_order,
            event_count: total_count,
            expected_event_count,
            expected_mutation_revision,
            mutation_revision,
            last_event_hash: (end_order > 0).then(|| format!("hash-{end_order}")),
            events,
            reset,
            scope: "/old/workspace".into(),
            agent_id: "main".into(),
            gateway_label: "old gateway".into(),
            workspace_label: "old workspace".into(),
            session_label: "session".into(),
        }
    }

    fn cleanup(database: CacheDatabase) {
        let path = database.path().to_owned();
        drop(database);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[test]
    fn remembered_devices_update_order_and_forget_without_touching_edb_cache() {
        let database = test_database("remembered-devices");
        database
            .remember_device("https://first.example", "first password")
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        database
            .remember_device("https://second.example", "second password")
            .unwrap();
        let devices = database.remembered_devices().unwrap();
        assert_eq!(
            devices
                .iter()
                .map(|device| device.endpoint.as_str())
                .collect::<Vec<_>>(),
            ["https://second.example", "https://first.example"]
        );
        assert_eq!(devices[0].password, "second password");

        std::thread::sleep(std::time::Duration::from_millis(2));
        database
            .remember_device("https://first.example", "updated password")
            .unwrap();
        let devices = database.remembered_devices().unwrap();
        assert_eq!(devices[0].endpoint, "https://first.example");
        assert_eq!(devices[0].password, "updated password");
        database.forget_device("https://second.example").unwrap();
        assert_eq!(database.remembered_devices().unwrap().len(), 1);
        assert!(database.list().unwrap().is_empty());
        cleanup(database);
    }

    #[test]
    fn cache_identity_is_only_edb_id_and_batches_are_incremental() {
        let database = test_database("identity");
        let edb_id = "1".repeat(64);
        database
            .save(request(
                &edb_id,
                0,
                2,
                0,
                None,
                0,
                vec![event(&edb_id, 0, "identity")],
                false,
            ))
            .unwrap();
        let mut moved = request(
            &edb_id,
            1,
            2,
            1,
            Some(0),
            0,
            vec![event(&edb_id, 1, "hello")],
            false,
        );
        moved.scope = "/new/workspace".into();
        moved.gateway_label = "new gateway".into();
        moved.workspace_label = "new workspace".into();
        moved.agent_id = "renamed-agent".into();
        database.save(moved).unwrap();

        let entries = database
            .load_metadata(std::slice::from_ref(&edb_id))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, edb_id);
        assert_eq!(entries[0].event_count, 2);
        assert_eq!(entries[0].scope, "/new/workspace");
        assert_eq!(entries[0].agent_id, "renamed-agent");
        let chunk = database.load_chunk(&edb_id, 0, 1024 * 1024).unwrap();
        assert_eq!(chunk.events.len(), 2);
        assert!(chunk.done);
        cleanup(database);
    }

    #[test]
    fn reset_is_chunked_and_delayed_batches_cannot_truncate_newer_cache() {
        let database = test_database("reset");
        let edb_id = "a".repeat(64);
        database
            .save(request(
                &edb_id,
                0,
                2,
                0,
                None,
                0,
                vec![event(&edb_id, 0, "old"), event(&edb_id, 1, "old")],
                false,
            ))
            .unwrap();
        let first_reset = request(
            &edb_id,
            0,
            3,
            2,
            Some(0),
            1,
            vec![event(&edb_id, 0, "new")],
            true,
        );
        database.save(first_reset.clone()).unwrap();
        database
            .save(request(
                &edb_id,
                1,
                3,
                1,
                Some(1),
                1,
                vec![event(&edb_id, 1, "new"), event(&edb_id, 2, "new")],
                false,
            ))
            .unwrap();
        database.save(first_reset).unwrap();
        let metadata = database.list().unwrap().remove(0);
        assert_eq!(metadata.event_count, 3);
        assert_eq!(metadata.mutation_revision, 1);
        assert_eq!(metadata.last_event_hash.as_deref(), Some("hash-3"));
        cleanup(database);
    }

    #[test]
    fn gaps_and_partial_overlaps_are_rejected() {
        let database = test_database("boundaries");
        let edb_id = "b".repeat(64);
        database
            .save(request(
                &edb_id,
                0,
                4,
                0,
                None,
                0,
                vec![event(&edb_id, 0, "0"), event(&edb_id, 1, "1")],
                false,
            ))
            .unwrap();
        assert!(
            database
                .save(request(
                    &edb_id,
                    3,
                    4,
                    3,
                    Some(0),
                    0,
                    vec![event(&edb_id, 3, "gap")],
                    false,
                ))
                .is_err()
        );
        assert!(
            database
                .save(request(
                    &edb_id,
                    1,
                    4,
                    1,
                    Some(0),
                    0,
                    vec![event(&edb_id, 1, "overlap"), event(&edb_id, 2, "overlap")],
                    false,
                ))
                .is_err()
        );
        assert_eq!(database.list().unwrap()[0].event_count, 2);
        cleanup(database);
    }

    #[test]
    fn chunk_reads_obey_byte_budget_and_list_is_metadata_only() {
        let database = test_database("chunks");
        let edb_id = "c".repeat(64);
        let events = vec![
            event(&edb_id, 0, "identity"),
            event(&edb_id, 1, &"x".repeat(256)),
            event(&edb_id, 2, &"y".repeat(256)),
        ];
        database
            .save(request(&edb_id, 0, 3, 0, None, 0, events, false))
            .unwrap();
        let metadata = database.list().unwrap();
        assert_eq!(metadata.len(), 1);
        assert!(metadata[0].byte_size > 512);
        let first = database.load_chunk(&edb_id, 0, 1).unwrap();
        assert_eq!(first.events.len(), 1);
        assert!(!first.done);
        let second = database.load_chunk(&edb_id, first.next_order, 1).unwrap();
        assert_eq!(second.events.len(), 1);
        assert!(!second.done);
        let third = database.load_chunk(&edb_id, second.next_order, 1).unwrap();
        assert_eq!(third.events.len(), 1);
        assert!(third.done);
        cleanup(database);
    }

    #[test]
    fn large_incremental_history_stays_linear_and_chunked() {
        const TOTAL_EVENTS: u64 = 20_001;
        const BATCH_EVENTS: u64 = 100;
        const READ_BUDGET: u64 = 1024 * 1024;
        let database = test_database("large-linear");
        let edb_id = "e".repeat(64);
        let content = "x".repeat(512);
        let mut start = 0_u64;
        let mut expected_bytes = 0_u64;
        let mut batches = 0_u64;
        while start < TOTAL_EVENTS {
            let end = (start + BATCH_EVENTS).min(TOTAL_EVENTS);
            let events = (start..end)
                .map(|id| event(&edb_id, id, &content))
                .collect::<Vec<_>>();
            expected_bytes += events
                .iter()
                .map(|event| serde_json::to_string(event).unwrap().len() as u64)
                .sum::<u64>();
            database
                .save(request(
                    &edb_id,
                    start,
                    TOTAL_EVENTS,
                    start,
                    (start > 0).then_some(0),
                    0,
                    events,
                    false,
                ))
                .unwrap();
            start = end;
            batches += 1;
        }
        let metadata = database.list().unwrap().remove(0);
        assert_eq!(batches, 201);
        assert_eq!(metadata.event_count, TOTAL_EVENTS);
        assert_eq!(metadata.byte_size, expected_bytes);
        assert!(metadata.byte_size > 10 * 1024 * 1024);

        let mut order = 0_u64;
        let mut chunks = 0_u64;
        while order < TOTAL_EVENTS {
            let chunk = database.load_chunk(&edb_id, order, READ_BUDGET).unwrap();
            assert_eq!(chunk.start_order, order);
            assert!(chunk.next_order > order);
            assert!(chunk.next_order <= TOTAL_EVENTS);
            order = chunk.next_order;
            chunks += 1;
        }
        assert!(chunks > 10);
        cleanup(database);
    }

    #[test]
    fn invalid_identity_is_rejected() {
        let database = test_database("invalid");
        let edb_id = "d".repeat(64);
        let invalid = request(
            &edb_id,
            0,
            1,
            0,
            None,
            0,
            vec![event(&"e".repeat(64), 0, "wrong")],
            false,
        );
        assert!(database.save(invalid).is_err());
        assert!(database.list().unwrap().is_empty());
        cleanup(database);
    }
}
