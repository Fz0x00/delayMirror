use chrono::{DateTime, Utc, Duration};
use serde_json::Value;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackageMetadata {
    pub package_name: String,
    pub metadata: Value,
    pub last_updated: DateTime<Utc>,
}

pub trait MetadataCache {
    fn get(&self, package_name: &str) -> Result<Option<PackageMetadata>, Box<dyn std::error::Error>>;
    fn set(&self, package_name: &str, metadata: &Value) -> Result<(), Box<dyn std::error::Error>>;
    fn is_valid(&self, package_name: &str, max_age_hours: i64) -> Result<bool, Box<dyn std::error::Error>>;
    fn cleanup(&self, max_age_hours: i64) -> Result<usize, Box<dyn std::error::Error>>;
}

#[cfg(feature = "server")]
pub mod server {
    use super::*;
    use rusqlite::{Connection, params};

    pub struct SqliteMetadataCache {
        conn: Connection,
    }

    impl SqliteMetadataCache {
        pub fn new(db_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
            let conn = Connection::open(db_path)?;
            conn.execute(
                r#"
                CREATE TABLE IF NOT EXISTS package_metadata (
                    package_name TEXT PRIMARY KEY,
                    metadata TEXT NOT NULL,
                    last_updated TIMESTAMP NOT NULL
                )
                "#,
                [],
            )?;
            Ok(Self { conn })
        }
    }

    impl MetadataCache for SqliteMetadataCache {
        fn get(&self, package_name: &str) -> Result<Option<PackageMetadata>, Box<dyn std::error::Error>> {
            let mut stmt = self.conn.prepare(
                "SELECT metadata, last_updated FROM package_metadata WHERE package_name = ?"
            )?;
            let mut rows = stmt.query([package_name])?;
            if let Some(row) = rows.next()? {
                let metadata_str: String = row.get(0)?;
                let last_updated: DateTime<Utc> = row.get(1)?;
                let metadata: Value = serde_json::from_str(&metadata_str)?;
                Ok(Some(PackageMetadata {
                    package_name: package_name.to_string(),
                    metadata,
                    last_updated,
                }))
            } else {
                Ok(None)
            }
        }

        fn set(&self, package_name: &str, metadata: &Value) -> Result<(), Box<dyn std::error::Error>> {
            let metadata_str = serde_json::to_string(metadata)?;
            let now = Utc::now();
            self.conn.execute(
                r#"
                INSERT OR REPLACE INTO package_metadata (package_name, metadata, last_updated)
                VALUES (?, ?, ?)
                "#,
                params![package_name, metadata_str, now],
            )?;
            Ok(())
        }

        fn is_valid(&self, package_name: &str, max_age_hours: i64) -> Result<bool, Box<dyn std::error::Error>> {
            if let Some(metadata) = self.get(package_name)? {
                let max_age = Duration::hours(max_age_hours);
                let now = Utc::now();
                Ok(now.signed_duration_since(metadata.last_updated) <= max_age)
            } else {
                Ok(false)
            }
        }

        fn cleanup(&self, max_age_hours: i64) -> Result<usize, Box<dyn std::error::Error>> {
            let cutoff_time = Utc::now() - Duration::hours(max_age_hours);
            Ok(self.conn.execute(
                "DELETE FROM package_metadata WHERE last_updated < ?",
                [cutoff_time],
            )?)
        }
    }
}

#[cfg(feature = "workers")]
pub mod workers {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use worker::{*, kv::KvStore};
    use futures::executor::block_on;

    pub struct InMemoryMetadataCache {
        cache: Mutex<HashMap<String, PackageMetadata>>,
    }

    impl InMemoryMetadataCache {
        pub fn new() -> Self {
            Self {
                cache: Mutex::new(HashMap::new()),
            }
        }
    }

    impl MetadataCache for InMemoryMetadataCache {
        fn get(&self, package_name: &str) -> std::result::Result<Option<PackageMetadata>, Box<dyn std::error::Error>> {
            let cache = self.cache.lock().unwrap();
            Ok(cache.get(package_name).cloned())
        }

        fn set(&self, package_name: &str, metadata: &Value) -> std::result::Result<(), Box<dyn std::error::Error>> {
            let mut cache = self.cache.lock().unwrap();
            cache.insert(
                package_name.to_string(),
                PackageMetadata {
                    package_name: package_name.to_string(),
                    metadata: metadata.clone(),
                    last_updated: Utc::now(),
                },
            );
            Ok(())
        }

        fn is_valid(&self, package_name: &str, max_age_hours: i64) -> std::result::Result<bool, Box<dyn std::error::Error>> {
            if let Some(metadata) = self.get(package_name)? {
                let max_age = Duration::hours(max_age_hours);
                let now = Utc::now();
                Ok(now.signed_duration_since(metadata.last_updated) <= max_age)
            } else {
                Ok(false)
            }
        }

        fn cleanup(&self, max_age_hours: i64) -> std::result::Result<usize, Box<dyn std::error::Error>> {
            let mut cache = self.cache.lock().unwrap();
            let cutoff_time = Utc::now() - Duration::hours(max_age_hours);
            let initial_len = cache.len();
            cache.retain(|_, v| v.last_updated >= cutoff_time);
            Ok(initial_len - cache.len())
        }
    }

    pub struct KVMetadataCache {
        kv: KvStore,
        namespace: String,
    }

    impl KVMetadataCache {
        pub fn new(kv: KvStore, namespace: &str) -> Self {
            Self {
                kv,
                namespace: namespace.to_string(),
            }
        }
    }

    impl MetadataCache for KVMetadataCache {
        fn get(&self, package_name: &str) -> std::result::Result<Option<PackageMetadata>, Box<dyn std::error::Error>> {
            let key = format!("{}/{}", self.namespace, package_name);
            block_on(async move {
                match self.kv.get(&key).text().await {
                    Ok(Some(value)) => {
                        let metadata: PackageMetadata = serde_json::from_str(&value)?;
                        Ok(Some(metadata))
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(Box::new(e) as Box<dyn std::error::Error>),
                }
            })
        }

        fn set(&self, package_name: &str, metadata: &Value) -> std::result::Result<(), Box<dyn std::error::Error>> {
            let key = format!("{}/{}", self.namespace, package_name);
            let package_metadata = PackageMetadata {
                package_name: package_name.to_string(),
                metadata: metadata.clone(),
                last_updated: Utc::now(),
            };
            let json_str = serde_json::to_string(&package_metadata)?;
            block_on(async move {
                match self.kv.put(&key, json_str) {
                    Ok(builder) => builder.execute().await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>),
                    Err(e) => Err(Box::new(e) as Box<dyn std::error::Error>),
                }
            })
        }

        fn is_valid(&self, package_name: &str, max_age_hours: i64) -> std::result::Result<bool, Box<dyn std::error::Error>> {
            if let Some(metadata) = self.get(package_name)? {
                let max_age = Duration::hours(max_age_hours);
                let now = Utc::now();
                Ok(now.signed_duration_since(metadata.last_updated) <= max_age)
            } else {
                Ok(false)
            }
        }

        fn cleanup(&self, _max_age_hours: i64) -> std::result::Result<usize, Box<dyn std::error::Error>> {
            // KV 存储没有直接的批量清理方法，这里返回 0 表示不支持清理
            Ok(0)
        }
    }
}

#[cfg(feature = "server")]
pub use server::SqliteMetadataCache;

#[cfg(feature = "workers")]
pub use workers::InMemoryMetadataCache;

#[cfg(feature = "workers")]
pub use workers::KVMetadataCache;