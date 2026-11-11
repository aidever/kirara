use rusqlite::{OptionalExtension, params};

use super::{Database, join_error, open_connection, sql_error};
use crate::error::AppError;

/// 新版本检查的开关与最近一次检查结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateCheckRecord {
    pub enabled: bool,
    pub latest_version: Option<String>,
    pub release_url: Option<String>,
    pub published_at: Option<String>,
    pub checked_at: Option<String>,
    pub last_error: Option<String>,
}

impl Database {
    pub(super) async fn init_update_check(&self) -> Result<(), AppError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let conn = open_connection(&path)?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS update_check (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    enabled INTEGER NOT NULL DEFAULT 1,
                    latest_version TEXT,
                    release_url TEXT,
                    published_at TEXT,
                    checked_at TEXT,
                    last_error TEXT
                );
                INSERT OR IGNORE INTO update_check (id, enabled) VALUES (1, 1);",
            )
            .map_err(sql_error)
        })
        .await
        .map_err(join_error)?
    }

    pub async fn get_update_check(&self) -> Result<UpdateCheckRecord, AppError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> Result<UpdateCheckRecord, AppError> {
            let conn = open_connection(&path)?;
            let record = conn
                .query_row(
                    "SELECT enabled, latest_version, release_url, published_at, checked_at, last_error
                     FROM update_check WHERE id = 1",
                    [],
                    |row| {
                        Ok(UpdateCheckRecord {
                            enabled: row.get::<_, i64>(0)? != 0,
                            latest_version: row.get(1)?,
                            release_url: row.get(2)?,
                            published_at: row.get(3)?,
                            checked_at: row.get(4)?,
                            last_error: row.get(5)?,
                        })
                    },
                )
                .optional()
                .map_err(sql_error)?;
            Ok(record.unwrap_or(UpdateCheckRecord {
                enabled: true,
                ..UpdateCheckRecord::default()
            }))
        })
        .await
        .map_err(join_error)?
    }

    pub async fn set_update_check_enabled(&self, enabled: bool) -> Result<(), AppError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let conn = open_connection(&path)?;
            conn.execute(
                "INSERT INTO update_check (id, enabled) VALUES (1, ?)
                 ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled",
                [enabled as i64],
            )
            .map_err(sql_error)?;
            Ok(())
        })
        .await
        .map_err(join_error)?
    }

    /// 记录一次检查结果；失败时保留上次查到的版本，只更新错误信息。
    pub async fn save_update_check_result(
        &self,
        checked_at: String,
        result: Result<(String, String, Option<String>), String>,
    ) -> Result<(), AppError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let conn = open_connection(&path)?;
            match result {
                Ok((version, url, published_at)) => conn.execute(
                    "UPDATE update_check SET latest_version = ?, release_url = ?, published_at = ?,
                         checked_at = ?, last_error = NULL WHERE id = 1",
                    params![version, url, published_at, checked_at],
                ),
                Err(error) => conn.execute(
                    "UPDATE update_check SET checked_at = ?, last_error = ? WHERE id = 1",
                    params![checked_at, error],
                ),
            }
            .map_err(sql_error)?;
            Ok(())
        })
        .await
        .map_err(join_error)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn update_check_defaults_to_enabled_and_keeps_last_version_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path()).await.unwrap();
        let record = db.get_update_check().await.unwrap();
        assert!(record.enabled);
        assert_eq!(record.latest_version, None);

        db.save_update_check_result(
            "2026-11-10T00:00:00Z".into(),
            Ok(("2.14.0".into(), "https://example.invalid/r".into(), None)),
        )
        .await
        .unwrap();
        db.save_update_check_result("2026-11-10T06:00:00Z".into(), Err("timeout".into()))
            .await
            .unwrap();
        db.set_update_check_enabled(false).await.unwrap();

        let record = db.get_update_check().await.unwrap();
        assert!(!record.enabled);
        assert_eq!(record.latest_version.as_deref(), Some("2.14.0"));
        assert_eq!(record.last_error.as_deref(), Some("timeout"));
        assert_eq!(record.checked_at.as_deref(), Some("2026-11-10T06:00:00Z"));
    }
}
