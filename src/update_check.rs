//! 新版本检查：定时查询 GitHub 上的最新正式版，有新版本时在界面上提醒。
//!
//! 只做检查与提醒，不会自动下载或替换程序；开关默认打开，可在设置里关闭。

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::db::Database;
use crate::net::client_factory;

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/aidever/kirara/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/aidever/kirara/releases";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const STARTUP_DELAY: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub enabled: bool,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_url: String,
    pub published_at: Option<String>,
    pub checked_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCheckSettings {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: Option<String>,
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

pub struct UpdateChecker {
    db: Database,
    /// 防止定时检查和手动检查同时请求 GitHub。
    running: Mutex<()>,
}

impl UpdateChecker {
    pub fn new(db: Database) -> Arc<Self> {
        Arc::new(Self {
            db,
            running: Mutex::new(()),
        })
    }

    pub async fn status(&self) -> Result<UpdateStatus, crate::error::AppError> {
        let record = self.db.get_update_check().await?;
        let update_available = record
            .latest_version
            .as_deref()
            .is_some_and(|latest| is_newer(latest, CURRENT_VERSION));
        Ok(UpdateStatus {
            enabled: record.enabled,
            current_version: CURRENT_VERSION.to_string(),
            latest_version: record.latest_version,
            update_available,
            release_url: record
                .release_url
                .unwrap_or_else(|| RELEASES_PAGE.to_string()),
            published_at: record.published_at,
            checked_at: record.checked_at,
            last_error: record.last_error,
        })
    }

    /// 立即检查一次并记录结果。
    pub async fn check_now(&self) -> Result<UpdateStatus, crate::error::AppError> {
        {
            let _guard = self.running.lock().await;
            let result = self.fetch_latest().await;
            if let Err(error) = &result {
                warn!(%error, "update check failed");
            }
            self.db
                .save_update_check_result(Utc::now().to_rfc3339(), result)
                .await?;
        }
        let status = self.status().await?;
        if status.update_available {
            info!(
                current = CURRENT_VERSION,
                latest = status.latest_version.as_deref().unwrap_or_default(),
                "a newer kirara release is available"
            );
        }
        Ok(status)
    }

    async fn fetch_latest(&self) -> Result<(String, String, Option<String>), String> {
        let proxy = self
            .db
            .get_settings()
            .await
            .ok()
            .and_then(|settings| settings.proxy);
        let client = client_factory::build_client(proxy.as_deref())
            .map_err(|error| format!("创建 HTTP 客户端失败：{error}"))?;
        let response = client
            .get(LATEST_RELEASE_API)
            .header(reqwest::header::USER_AGENT, format!("kirara/{CURRENT_VERSION}"))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|error| format!("请求 GitHub 失败：{error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("GitHub 返回 HTTP {}", status.as_u16()));
        }
        let release: GithubRelease = response
            .json()
            .await
            .map_err(|error| format!("解析 GitHub 响应失败：{error}"))?;
        if release.draft || release.prerelease {
            return Err("GitHub 最新版本不是正式版".to_string());
        }
        let version = release.tag_name.trim().trim_start_matches(['v', 'V']).to_string();
        if parse_version(&version).is_none() {
            return Err(format!("无法识别的版本号：{}", release.tag_name));
        }
        let url = release
            .html_url
            .filter(|url| url.starts_with("https://github.com/"))
            .unwrap_or_else(|| RELEASES_PAGE.to_string());
        Ok((version, url, release.published_at))
    }

    /// 后台循环：开关打开时每 6 小时检查一次；关闭后不再访问 GitHub。
    pub async fn start(self: Arc<Self>, shutdown: CancellationToken) {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(STARTUP_DELAY) => {}
        }
        loop {
            match self.db.get_update_check().await {
                Ok(record) if record.enabled && is_due(record.checked_at.as_deref()) => {
                    if let Err(error) = self.check_now().await {
                        warn!(%error, "failed to record update check");
                    }
                }
                Ok(_) => {}
                Err(error) => warn!(%error, "failed to load update check settings"),
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
    }
}

fn is_due(checked_at: Option<&str>) -> bool {
    let Some(checked_at) = checked_at.and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
    else {
        return true;
    };
    let elapsed = Utc::now().signed_duration_since(checked_at);
    elapsed.num_seconds() < 0 || elapsed.to_std().is_ok_and(|elapsed| elapsed >= CHECK_INTERVAL)
}

/// 取版本号的「主.次.修订」三段；忽略 `-snapshot` 之类的后缀。
fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let core = value
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

pub async fn get_status(
    State(checker): State<Arc<UpdateChecker>>,
) -> Result<Json<UpdateStatus>, crate::web::ApiError> {
    Ok(Json(checker.status().await?))
}

pub async fn update_settings(
    State(checker): State<Arc<UpdateChecker>>,
    Json(payload): Json<UpdateCheckSettings>,
) -> Result<Json<UpdateStatus>, crate::web::ApiError> {
    checker.db.set_update_check_enabled(payload.enabled).await?;
    Ok(Json(checker.status().await?))
}

pub async fn run_check(
    State(checker): State<Arc<UpdateChecker>>,
) -> Result<Json<UpdateStatus>, crate::web::ApiError> {
    Ok(Json(checker.check_now().await?))
}

pub fn router(checker: Arc<UpdateChecker>) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/update-check", get(get_status).put(update_settings))
        .route("/api/update-check/run", post(run_check))
        .with_state(checker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_release_versions_numerically() {
        assert!(is_newer("2.14.0", "2.13.0"));
        assert!(is_newer("v2.13.1", "2.13.0"));
        assert!(is_newer("2.10.0", "2.9.9"));
        assert!(!is_newer("2.13.0", "2.13.0"));
        assert!(!is_newer("2.12.5", "2.13.0"));
        assert!(!is_newer("2.13.0", "2.13.0-snapshot.5"));
        assert!(!is_newer("nonsense", "2.13.0"));
    }

    #[test]
    fn check_is_due_after_the_interval_or_without_history() {
        assert!(is_due(None));
        assert!(is_due(Some("not a date")));
        assert!(!is_due(Some(&Utc::now().to_rfc3339())));
        let old = Utc::now() - chrono::Duration::hours(7);
        assert!(is_due(Some(&old.to_rfc3339())));
    }
}
