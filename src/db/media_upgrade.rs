//! 制作组洗版记录：备选或列表外版本下载后，等待更靠前的制作组发布。

use chrono::Utc;
use rusqlite::{OptionalExtension, Row, params};

use super::{Database, join_error, open_connection, sql_error};
use crate::error::AppError;
use crate::media::models::MediaGroupUpgradeRecord;

const UPGRADE_SELECT: &str =
    "SELECT id, subscription_id, target_key, season, episode, absolute_episode,
        download_id, current_group, current_rank, status, upgrade_download_id, upgrade_group,
        upgrade_rank, replaced_count, last_checked_at, last_error, created_at, updated_at
     FROM media_group_upgrades";

#[derive(Debug, Clone)]
pub struct NewMediaGroupUpgrade {
    pub subscription_id: i64,
    pub target_key: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub absolute_episode: Option<u32>,
    pub download_id: i64,
    pub current_group: Option<String>,
    pub current_rank: u32,
}

impl Database {
    /// 记录（或重新开始）某集的洗版观察。同一订阅同一集只保留一条。
    pub async fn watch_media_group_upgrade(
        &self,
        request: NewMediaGroupUpgrade,
    ) -> Result<(), AppError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_connection(&path)?;
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO media_group_upgrades
                 (subscription_id, target_key, season, episode, absolute_episode, download_id,
                  current_group, current_rank, status, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'watching', ?, ?)
                 ON CONFLICT(subscription_id, target_key) DO UPDATE SET
                    season = excluded.season, episode = excluded.episode,
                    absolute_episode = excluded.absolute_episode,
                    download_id = excluded.download_id, current_group = excluded.current_group,
                    current_rank = excluded.current_rank, status = 'watching',
                    upgrade_download_id = NULL, upgrade_group = NULL, upgrade_rank = NULL,
                    last_checked_at = NULL, last_error = NULL, updated_at = excluded.updated_at",
                params![
                    request.subscription_id,
                    request.target_key,
                    request.season,
                    request.episode,
                    request.absolute_episode,
                    request.download_id,
                    request.current_group,
                    request.current_rank,
                    now,
                    now,
                ],
            )
            .map_err(sql_error)?;
            Ok(())
        })
        .await
        .map_err(join_error)?
    }

    /// 需要检查的洗版记录：观察中且距上次检查已超过间隔，或正在下载新版本。
    pub async fn list_due_media_group_upgrades(
        &self,
        checked_before: &str,
        limit: usize,
    ) -> Result<Vec<MediaGroupUpgradeRecord>, AppError> {
        let path = self.path.clone();
        let checked_before = checked_before.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = open_connection(&path)?;
            let sql = format!(
                "{UPGRADE_SELECT}
                 WHERE status = 'upgrading'
                    OR (status = 'watching' AND (last_checked_at IS NULL OR last_checked_at < ?))
                 ORDER BY CASE status WHEN 'upgrading' THEN 0 ELSE 1 END,
                          COALESCE(last_checked_at, ''), id
                 LIMIT ?"
            );
            let mut statement = conn.prepare(&sql).map_err(sql_error)?;
            let rows = statement
                .query_map(params![checked_before, limit as i64], map_upgrade)
                .map_err(sql_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
        })
        .await
        .map_err(join_error)?
    }

    pub async fn list_media_group_upgrades(
        &self,
        subscription_id: i64,
    ) -> Result<Vec<MediaGroupUpgradeRecord>, AppError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_connection(&path)?;
            let sql =
                format!("{UPGRADE_SELECT} WHERE subscription_id = ? ORDER BY target_key DESC");
            let mut statement = conn.prepare(&sql).map_err(sql_error)?;
            let rows = statement
                .query_map([subscription_id], map_upgrade)
                .map_err(sql_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
        })
        .await
        .map_err(join_error)?
    }

    pub async fn get_media_group_upgrade(
        &self,
        id: i64,
    ) -> Result<Option<MediaGroupUpgradeRecord>, AppError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_connection(&path)?;
            conn.query_row(&format!("{UPGRADE_SELECT} WHERE id = ?"), [id], map_upgrade)
                .optional()
                .map_err(sql_error)
        })
        .await
        .map_err(join_error)?
    }

    /// 保存一次检查的结果；`updated_at` 由数据库层刷新。
    pub async fn save_media_group_upgrade(
        &self,
        record: &MediaGroupUpgradeRecord,
    ) -> Result<(), AppError> {
        let path = self.path.clone();
        let record = record.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_connection(&path)?;
            conn.execute(
                "UPDATE media_group_upgrades
                 SET download_id = ?, current_group = ?, current_rank = ?, status = ?,
                     upgrade_download_id = ?, upgrade_group = ?, upgrade_rank = ?,
                     replaced_count = ?, last_checked_at = ?, last_error = ?, updated_at = ?
                 WHERE id = ?",
                params![
                    record.download_id,
                    record.current_group,
                    record.current_rank,
                    record.status,
                    record.upgrade_download_id,
                    record.upgrade_group,
                    record.upgrade_rank,
                    record.replaced_count,
                    record.last_checked_at,
                    record.last_error,
                    Utc::now().to_rfc3339(),
                    record.id,
                ],
            )
            .map_err(sql_error)?;
            Ok(())
        })
        .await
        .map_err(join_error)?
    }

    /// 关闭洗版或修改目标后，停止该订阅仍在观察的记录（下载中的新版本照常完成）。
    pub async fn cancel_watching_media_group_upgrades(
        &self,
        subscription_id: i64,
    ) -> Result<usize, AppError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_connection(&path)?;
            conn.execute(
                "UPDATE media_group_upgrades SET status = 'cancelled', updated_at = ?
                 WHERE subscription_id = ? AND status = 'watching'",
                params![Utc::now().to_rfc3339(), subscription_id],
            )
            .map_err(sql_error)
        })
        .await
        .map_err(join_error)?
    }
}

fn map_upgrade(row: &Row<'_>) -> rusqlite::Result<MediaGroupUpgradeRecord> {
    Ok(MediaGroupUpgradeRecord {
        id: row.get(0)?,
        subscription_id: row.get(1)?,
        target_key: row.get(2)?,
        season: row.get(3)?,
        episode: row.get(4)?,
        absolute_episode: row.get(5)?,
        download_id: row.get(6)?,
        current_group: row.get(7)?,
        current_rank: row.get(8)?,
        status: row.get(9)?,
        upgrade_download_id: row.get(10)?,
        upgrade_group: row.get(11)?,
        upgrade_rank: row.get(12)?,
        replaced_count: row.get(13)?,
        last_checked_at: row.get(14)?,
        last_error: row.get(15)?,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
    })
}
