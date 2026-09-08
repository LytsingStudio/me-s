use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct ClientDatabase {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RememberedDevice {
    pub endpoint: String,
    pub password: String,
    pub updated_at: u64,
}

impl ClientDatabase {
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
            .busy_timeout(DATABASE_BUSY_TIMEOUT)
            .map_err(|error| format!("无法配置客户端数据库锁等待：{error}"))?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
            )
            .map_err(|error| format!("无法配置客户端数据库：{error}"))?;
        Ok(connection)
    }

    fn initialize(&self) -> Result<(), String> {
        let mut connection = self.connect()?;
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
                );",
            )
            .map_err(|error| format!("无法初始化客户端数据库：{error}"))?;
        if let Err(error) = remove_legacy_cache(&mut connection) {
            // A busy old client must not prevent this client from using its preferences.
            log::warn!("Unable to remove legacy client cache; will retry on next startup: {error}");
        }
        Ok(())
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
}

fn remove_legacy_cache(connection: &mut Connection) -> rusqlite::Result<()> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let legacy: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name IN ('edb_events', 'edb_sessions'))
         OR EXISTS(SELECT 1 FROM client_settings WHERE key = 'me-raw-edb-decoding')",
        [], |row| row.get(0),
    )?;
    if version >= 1 && !legacy {
        return Ok(());
    }
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "DROP TABLE IF EXISTS edb_events;
         DROP TABLE IF EXISTS edb_sessions;
         DELETE FROM client_settings WHERE key = 'me-raw-edb-decoding';",
    )?;
    transaction.commit()?;
    // Mark only after reclaiming the old event pages, so an interrupted cleanup is retried.
    connection
        .execute_batch("VACUUM; PRAGMA user_version = 1; PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

fn to_i64(value: u64, label: &str) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| format!("{label} 超出 SQLite 支持范围"))
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

    fn test_database(name: &str) -> ClientDatabase {
        let directory = std::env::var_os("ME_CLIENT_TEST_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/storage-tests")
            });
        fs::create_dir_all(&directory).unwrap();
        ClientDatabase::new(directory.join(format!(
            "{name}-{}-{}.sqlite3",
            std::process::id(),
            now_ms()
        )))
        .unwrap()
    }

    fn cleanup(database: ClientDatabase) {
        let _ = fs::remove_file(&database.path);
        let _ = fs::remove_file(database.path.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(database.path.with_extension("sqlite3-shm"));
    }

    #[test]
    fn preferences_and_remembered_devices_survive_reopening() {
        let database = test_database("preferences");
        database.set_setting("me-theme", "ocean").unwrap();
        database
            .remember_device("https://first.example", "first password")
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));
        database
            .remember_device("https://second.example", "second password")
            .unwrap();
        let devices = database.remembered_devices().unwrap();
        assert_eq!(devices[0].endpoint, "https://second.example");
        std::thread::sleep(Duration::from_millis(2));
        database
            .remember_device("https://first.example", "updated password")
            .unwrap();
        let reopened = ClientDatabase::new(database.path.clone()).unwrap();
        let devices = reopened.remembered_devices().unwrap();
        assert_eq!(devices[0].endpoint, "https://first.example");
        assert_eq!(devices[0].password, "updated password");
        reopened.forget_device("https://second.example").unwrap();
        assert_eq!(database.remembered_devices().unwrap().len(), 1);
        assert_eq!(
            reopened.setting("me-theme").unwrap().as_deref(),
            Some("ocean")
        );
        cleanup(database);
    }

    #[test]
    fn legacy_cleanup_removes_only_cache_and_reclaims_space() {
        let database = test_database("legacy");
        database
            .set_setting("gateway.endpoint", "https://saved.example")
            .unwrap();
        database.set_setting("me-theme", "obsidian").unwrap();
        database.set_setting("me-raw-edb-decoding", "true").unwrap();
        database
            .remember_device("https://saved.example", "saved password")
            .unwrap();
        let connection = database.connect().unwrap();
        connection
            .execute_batch(
                "PRAGMA user_version = 0;
             CREATE TABLE edb_sessions(edb_id TEXT PRIMARY KEY);
             CREATE TABLE edb_events(edb_id TEXT REFERENCES edb_sessions(edb_id), event_json TEXT);
             INSERT INTO edb_sessions VALUES('legacy');
             INSERT INTO edb_events VALUES('legacy', zeroblob(4194304));
             CREATE TABLE unrelated(value TEXT);
             INSERT INTO unrelated VALUES('keep');
             PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .unwrap();
        drop(connection);
        let before = fs::metadata(&database.path).unwrap().len();
        let reopened = ClientDatabase::new(database.path.clone()).unwrap();
        assert_eq!(reopened.setting("me-raw-edb-decoding").unwrap(), None);
        assert_eq!(
            reopened.setting("me-theme").unwrap().as_deref(),
            Some("obsidian")
        );
        assert_eq!(
            reopened.setting("gateway.endpoint").unwrap().as_deref(),
            Some("https://saved.example")
        );
        assert_eq!(
            reopened.remembered_devices().unwrap()[0].password,
            "saved password"
        );
        let connection = reopened.connect().unwrap();
        let count: u32 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('edb_events','edb_sessions')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            connection
                .query_row("SELECT value FROM unrelated", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        drop(connection);
        assert!(fs::metadata(&database.path).unwrap().len() < before / 2);
        ClientDatabase::new(database.path.clone()).unwrap();
        cleanup(database);
    }

    #[test]
    fn locked_legacy_cache_is_preserved_and_cleanup_retries_after_release() {
        let database = test_database("locked-legacy");
        database.set_setting("me-theme", "ocean").unwrap();
        let mut connection = database.connect().unwrap();
        connection
            .execute_batch(
                "PRAGMA user_version = 0; CREATE TABLE edb_sessions(edb_id TEXT PRIMARY KEY);
             CREATE TABLE edb_events(event_json TEXT); INSERT INTO edb_events VALUES('legacy');",
            )
            .unwrap();
        let mut other = database.connect().unwrap();
        other.busy_timeout(Duration::from_millis(20)).unwrap();
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        assert!(remove_legacy_cache(&mut other).is_err());
        assert_eq!(
            other
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            other
                .query_row("SELECT event_json FROM edb_events", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "legacy"
        );
        assert_eq!(
            database.setting("me-theme").unwrap().as_deref(),
            Some("ocean")
        );
        transaction.commit().unwrap();
        drop(other);
        drop(connection);
        let reopened = ClientDatabase::new(database.path.clone()).unwrap();
        assert!(
            !reopened
                .connect()
                .unwrap()
                .table_exists(None, "edb_events")
                .unwrap()
        );
        assert_eq!(
            reopened.setting("me-theme").unwrap().as_deref(),
            Some("ocean")
        );
        cleanup(database);
    }

    #[test]
    fn cleanup_detects_legacy_data_recreated_after_an_earlier_migration() {
        let database = test_database("recreated-legacy");
        let connection = database.connect().unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            1
        );
        connection
            .execute_batch("CREATE TABLE edb_events(event_json TEXT);")
            .unwrap();
        drop(connection);
        database.set_setting("me-raw-edb-decoding", "true").unwrap();
        let reopened = ClientDatabase::new(database.path.clone()).unwrap();
        assert!(
            !reopened
                .connect()
                .unwrap()
                .table_exists(None, "edb_events")
                .unwrap()
        );
        assert_eq!(reopened.setting("me-raw-edb-decoding").unwrap(), None);
        cleanup(database);
    }

    #[test]
    fn concurrent_writers_wait_for_the_shared_database_lock() {
        let database = test_database("writers");
        let mut connection = database.connect().unwrap();
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let other = database.clone();
        let writer = std::thread::spawn(move || other.set_setting("me-theme", "ocean"));
        std::thread::sleep(Duration::from_millis(50));
        transaction.commit().unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(
            database.setting("me-theme").unwrap().as_deref(),
            Some("ocean")
        );
        drop(connection);
        cleanup(database);
    }
}
