use chrono::{DateTime, Utc, Duration};
use rusqlite::{Connection, params};
use serde_json::Value;

#[derive(Debug)]
pub struct PackageMetadata {
    pub package_name: String,
    pub metadata: Value,
    pub last_updated: DateTime<Utc>,
}

pub struct MetadataCache {
    conn: Connection,
}

impl MetadataCache {
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

    pub fn get(&self, package_name: &str) -> Result<Option<PackageMetadata>, Box<dyn std::error::Error>> {
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

    pub fn set(&self, package_name: &str, metadata: &Value) -> Result<(), Box<dyn std::error::Error>> {
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

    pub fn is_valid(&self, package_name: &str, max_age_hours: i64) -> Result<bool, Box<dyn std::error::Error>> {
        if let Some(metadata) = self.get(package_name)? {
            let max_age = Duration::hours(max_age_hours);
            let now = Utc::now();
            Ok(now.signed_duration_since(metadata.last_updated) <= max_age)
        } else {
            Ok(false)
        }
    }

    pub fn cleanup(&self, max_age_hours: i64) -> Result<usize, Box<dyn std::error::Error>> {
        let cutoff_time = Utc::now() - Duration::hours(max_age_hours);
        Ok(self.conn.execute(
            "DELETE FROM package_metadata WHERE last_updated < ?",
            [cutoff_time],
        )?)
    }
}