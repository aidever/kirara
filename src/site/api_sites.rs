//! Account statistics for trackers that expose a JSON API instead of NexusPHP pages.
//! Ported from the PT-depiler definitions `rousipro.ts` (Rousi / PeerGo API v1),
//! `zhuque.ts` (TNode) and `sunnypt.ts` (SunnyPT integration API).

use std::future::Future;
use std::pin::Pin;

use reqwest::header::{ACCEPT, AUTHORIZATION, COOKIE, HeaderMap, HeaderValue};
use reqwest::{Client, Method, Url};
use serde_json::Value;

use super::json_util::{
    at, ensure_success, integer, number, request_json, size_bytes, text, time_millis,
};
use super::{SiteAdapter, SiteAuth, SiteType, TorrentAttributes, UserStats, UserStatsDetails};

pub struct ApiSiteAdapter {
    site_type: SiteType,
    base_url: String,
    auth: SiteAuth,
    request_headers: HeaderMap,
    client: Client,
}

impl ApiSiteAdapter {
    pub fn new(
        site_type: SiteType,
        base_url: String,
        auth: SiteAuth,
        request_headers: HeaderMap,
        client: Client,
    ) -> Self {
        Self {
            site_type,
            base_url: base_url.trim().trim_end_matches('/').to_string(),
            auth,
            request_headers,
            client,
        }
    }

    fn label(&self) -> &'static str {
        match self.site_type {
            SiteType::Rousi => "Rousi Pro",
            SiteType::TNode => "TNode",
            SiteType::SunnyPt => "SunnyPT",
            SiteType::Piggo => "Piggo",
            _ => "站点",
        }
    }

    fn headers(&self) -> Result<HeaderMap, String> {
        let mut headers = self.request_headers.clone();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let invalid = || format!("{} 凭据包含非法字符", self.label());
        match (self.site_type, &self.auth) {
            (SiteType::Rousi | SiteType::Piggo, SiteAuth::ApiKey { api_key }) => {
                let mut value = HeaderValue::from_str(&format!("Bearer {}", api_key.trim()))
                    .map_err(|_| invalid())?;
                value.set_sensitive(true);
                headers.insert(AUTHORIZATION, value);
            }
            (SiteType::SunnyPt, SiteAuth::ApiKey { api_key }) => {
                let mut value = HeaderValue::from_str(api_key.trim()).map_err(|_| invalid())?;
                value.set_sensitive(true);
                headers.insert("x-api-key", value);
            }
            (
                SiteType::TNode,
                SiteAuth::Cookie { cookie } | SiteAuth::CookiePasskey { cookie, .. },
            ) => {
                let mut value = HeaderValue::from_str(cookie.trim()).map_err(|_| invalid())?;
                value.set_sensitive(true);
                headers.insert(COOKIE, value);
            }
            (SiteType::TNode, _) => return Err("TNode 站点需要 Cookie 认证".to_string()),
            _ => return Err(format!("{} 需要 API Key 认证", self.label())),
        }
        Ok(headers)
    }

    async fn get(&self, url: &str) -> Result<Value, String> {
        let json = request_json(
            &self.client,
            Method::GET,
            url,
            self.headers()?,
            None,
            self.label(),
        )
        .await?;
        ensure_success(&json, self.label())?;
        Ok(json)
    }

    async fn fetch_stats(&self) -> Result<UserStats, String> {
        match self.site_type {
            SiteType::Rousi => {
                let profile = self
                    .get(&format!("{}/api/v1/profile", self.base_url))
                    .await?;
                // 时魔在单独的接口里，取不到不影响主要数据。
                let reward = self
                    .get(&format!("{}/api/v1/seeding-reward", self.base_url))
                    .await
                    .ok();
                parse_rousi_profile(&profile, reward.as_ref())
            }
            SiteType::TNode => {
                let info = self
                    .get(&format!("{}/api/plugins/ptppUserInfo", self.base_url))
                    .await?;
                parse_tnode_user_info(&info)
            }
            SiteType::SunnyPt => {
                let api = sunnypt_api_base(&self.base_url)?;
                let profile = self.get(&format!("{api}/profile")).await?;
                parse_sunnypt_profile(&profile)
            }
            SiteType::Piggo => self.fetch_piggo_stats().await,
            _ => Err("不支持的站点类型".to_string()),
        }
    }

    async fn piggo_post(&self, api: &str, path: &str, body: Value) -> Result<Value, String> {
        let json = request_json(
            &self.client,
            Method::POST,
            &format!("{api}{path}"),
            self.headers()?,
            Some(&body),
            "Piggo",
        )
        .await?;
        ensure_success(&json, "Piggo")?;
        Ok(json)
    }

    async fn fetch_piggo_stats(&self) -> Result<UserStats, String> {
        let api = piggo_api_base(&self.base_url)?;
        let me = self.piggo_post(&api, "/me", serde_json::json!({})).await?;
        let mut stats = parse_piggo_me(&me)?;
        // 做种/下载数来自「我的种子」列表的 total，取不到不影响主要数据。
        for (kind, slot) in [("seeding", 0), ("leeching", 1)] {
            let total = self
                .piggo_post(
                    &api,
                    "/me/torrents",
                    serde_json::json!({"kind": kind, "page": 1, "per_page": 1}),
                )
                .await
                .ok()
                .and_then(|json| u32_of(at(&json, "data.total")));
            if slot == 0 {
                stats.seeding_count = total.or(stats.seeding_count);
            } else {
                stats.leeching_count = total.or(stats.leeching_count);
            }
        }
        Ok(stats)
    }
}

/// Piggo 的「实验室 API」在 `api.<站点域名>/v1/api` 下（站点 guest-config 的 site_url）。
pub(crate) fn piggo_api_base(base_url: &str) -> Result<String, String> {
    let url = Url::parse(base_url.trim()).map_err(|_| "Piggo 站点地址无效".to_string())?;
    let host = url.host_str().ok_or_else(|| "Piggo 站点地址无效".to_string())?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    let host = host.strip_prefix("api.").unwrap_or(host);
    Ok(format!("https://api.{host}/v1/api"))
}

pub(crate) fn parse_piggo_me(me: &Value) -> Result<UserStats, String> {
    let user = at(me, "data.user");
    let stats = &user["stats"];
    let uploaded = size_bytes(&stats["uploaded"]).ok_or("Piggo 上传量缺失")?;
    let downloaded = size_bytes(&stats["downloaded"]).ok_or("Piggo 下载量缺失")?;
    let level_name = text(&user["class"]["name"]).or_else(|| text(&user["class"]));
    let details = UserStatsDetails {
        level_name,
        level_id: integer(&user["class"]["id"]).and_then(|v| i64::try_from(v).ok()),
        invites: integer(&stats["invites"]),
        ..Default::default()
    };
    Ok(UserStats {
        uid: text(&user["id"]),
        username: text(&user["username"])
            .or_else(|| text(&user["name"]))
            .ok_or("Piggo 用户名缺失，登录令牌可能已失效")?,
        uploaded,
        downloaded,
        ratio: ratio_of(uploaded, downloaded, number(&stats["ratio"])),
        bonus: number(&stats["bonus"]),
        seeding_count: u32_of(&stats["seeding"]),
        leeching_count: u32_of(&stats["leeching"]),
        details,
    })
}

/// SunnyPT 的集成 API 在 `api.<站点域名>/api/v1/mp` 下。
pub(crate) fn sunnypt_api_base(base_url: &str) -> Result<String, String> {
    let url = Url::parse(base_url.trim()).map_err(|_| "SunnyPT 站点地址无效".to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "SunnyPT 站点地址无效".to_string())?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    let host = host.strip_prefix("api.").unwrap_or(host);
    Ok(format!("https://api.{host}/api/v1/mp"))
}

fn u32_of(value: &Value) -> Option<u32> {
    integer(value).and_then(|value| u32::try_from(value).ok())
}

fn required_username(value: &Value, label: &str) -> Result<String, String> {
    text(value).ok_or_else(|| format!("{label} 用户名缺失，凭据可能已失效"))
}

fn ratio_of(uploaded: u64, downloaded: u64, reported: Option<f64>) -> Option<f64> {
    reported
        .filter(|ratio| *ratio >= 0.0)
        .or_else(|| (downloaded > 0).then(|| uploaded as f64 / downloaded as f64))
}

pub(crate) fn parse_rousi_profile(
    profile: &Value,
    reward: Option<&Value>,
) -> Result<UserStats, String> {
    let data = &profile["data"];
    let uploaded = integer(&data["uploaded"]).ok_or("Rousi Pro 上传量缺失")?;
    let downloaded = integer(&data["downloaded"]).ok_or("Rousi Pro 下载量缺失")?;
    let details = UserStatsDetails {
        level_id: integer(&data["level"]).and_then(|v| i64::try_from(v).ok()),
        level_name: text(&data["level_text"]),
        join_time: time_millis(&data["registered_at"]),
        last_access_at: time_millis(&data["last_active_at"]),
        seeding_size: integer(at(data, "seeding_leeching_data.seeding_size")),
        bonus_per_hour: reward.and_then(|reward| number(at(reward, "data.total_reward"))),
        ..Default::default()
    };
    Ok(UserStats {
        uid: text(&data["id"]),
        username: required_username(&data["username"], "Rousi Pro")?,
        uploaded,
        downloaded,
        ratio: ratio_of(uploaded, downloaded, number(&data["ratio"])),
        bonus: number(&data["karma"]),
        seeding_count: u32_of(at(data, "seeding_leeching_data.seeding_count")),
        leeching_count: u32_of(at(data, "seeding_leeching_data.leeching_count")),
        details,
    })
}

pub(crate) fn parse_tnode_user_info(info: &Value) -> Result<UserStats, String> {
    let data = &info["data"];
    let uploaded = integer(&data["uploaded"]).ok_or("TNode 上传量缺失")?;
    let downloaded = integer(&data["downloaded"]).ok_or("TNode 下载量缺失")?;
    let details = UserStatsDetails {
        level_name: text(&data["levelName"]),
        join_time: time_millis(&data["joinTime"]),
        last_access_at: time_millis(&data["lastUpdateTime"]),
        message_count: integer(&data["messageCount"]),
        invites: integer(&data["invites"]),
        true_downloaded: integer(&data["trueDownloaded"]),
        seeding_size: integer(&data["seedingSize"]),
        seeding_bonus: number(&data["seedingPoints"]),
        bonus_per_hour: number(&data["bonusPerHour"]),
        uploads: integer(&data["uploadCount"]),
        ..Default::default()
    };
    Ok(UserStats {
        uid: text(&data["id"]),
        username: required_username(&data["name"], "TNode")?,
        uploaded,
        downloaded,
        ratio: ratio_of(uploaded, downloaded, None),
        bonus: number(&data["bonus"]),
        seeding_count: u32_of(&data["seeding"]),
        leeching_count: None,
        details,
    })
}

pub(crate) fn parse_sunnypt_profile(profile: &Value) -> Result<UserStats, String> {
    let data = &profile["data"];
    let uploaded = integer(&data["uploaded"]).ok_or("SunnyPT 上传量缺失")?;
    let downloaded = integer(&data["downloaded"]).ok_or("SunnyPT 下载量缺失")?;
    let details = UserStatsDetails {
        level_name: text(&data["level"]),
        // data.class 是 NexusPHP 原始等级常量（0 = Peasant … 9 = Nexus Master）。
        level_id: integer(&data["class"]).and_then(|v| i64::try_from(v).ok()),
        join_time: time_millis(&data["registered_at"]),
        message_count: integer(&data["unread_messages"]),
        seeding_size: integer(&data["seeding_size"]),
        ..Default::default()
    };
    Ok(UserStats {
        uid: text(&data["id"]),
        username: required_username(&data["username"], "SunnyPT")?,
        uploaded,
        downloaded,
        ratio: ratio_of(uploaded, downloaded, number(&data["ratio"])),
        bonus: number(&data["bonus"]),
        seeding_count: u32_of(&data["seeding_count"]),
        leeching_count: u32_of(&data["leeching_count"]),
        details,
    })
}

impl SiteAdapter for ApiSiteAdapter {
    fn get_user_stats(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<UserStats, String>> + Send + '_>> {
        Box::pin(self.fetch_stats())
    }

    fn get_torrent_attributes(
        &self,
        _detail_url: &str,
    ) -> Pin<Box<dyn Future<Output = Result<TorrentAttributes, String>> + Send + '_>> {
        let label = self.label();
        Box::pin(async move { Err(format!("{label} 暂不支持读取种子促销属性")) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_rousi_profile_and_reward() {
        let profile = json!({"code": 0, "message": "ok", "data": {
            "id": 42, "username": "alice", "level": 3, "level_text": "Power User",
            "registered_at": "2025-01-02T03:04:05+08:00", "uploaded": 2048, "downloaded": 1024,
            "ratio": 2.0, "karma": 123.5,
            "seeding_leeching_data": {"seeding_count": 7, "seeding_size": 99, "leeching_count": 1}
        }});
        let reward = json!({"code": 0, "data": {"total_reward": 12.5}});
        let stats = parse_rousi_profile(&profile, Some(&reward)).unwrap();
        assert_eq!(stats.uid.as_deref(), Some("42"));
        assert_eq!(stats.username, "alice");
        assert_eq!(stats.ratio, Some(2.0));
        assert_eq!(stats.bonus, Some(123.5));
        assert_eq!(stats.seeding_count, Some(7));
        assert_eq!(stats.leeching_count, Some(1));
        assert_eq!(stats.details.seeding_size, Some(99));
        assert_eq!(stats.details.bonus_per_hour, Some(12.5));
        assert_eq!(stats.details.level_name.as_deref(), Some("Power User"));
    }

    #[test]
    fn parses_tnode_user_info() {
        let info = json!({"status": 200, "data": {
            "id": "9", "name": "bob", "levelName": "结丹", "joinTime": 1_700_000_000,
            "uploaded": "300", "downloaded": "100", "trueDownloaded": 90, "bonus": "10.5",
            "seedingPoints": 3, "bonusPerHour": 1.25, "uploadCount": 2, "seeding": 5,
            "seedingSize": 1000, "messageCount": 0, "csrfToken": "secret", "invites": 1
        }});
        let stats = parse_tnode_user_info(&info).unwrap();
        assert_eq!(stats.username, "bob");
        assert_eq!(stats.ratio, Some(3.0));
        assert_eq!(stats.details.join_time, Some(1_700_000_000_000));
        assert_eq!(stats.details.true_downloaded, Some(90));
        assert_eq!(stats.seeding_count, Some(5));
        assert!(!serde_json::to_string(&stats).unwrap().contains("secret"));
    }

    #[test]
    fn parses_sunnypt_profile_and_api_base() {
        let profile = json!({"code": 0, "data": {
            "id": 1, "username": "carol", "uploaded": 10, "downloaded": 0, "bonus": 5,
            "seeding_count": 2, "leeching_count": 0, "level": "Insane User", "class": 5
        }});
        let stats = parse_sunnypt_profile(&profile).unwrap();
        assert_eq!(stats.username, "carol");
        assert_eq!(stats.ratio, None);
        assert_eq!(stats.details.level_id, Some(5));
        assert_eq!(
            sunnypt_api_base("https://sunnypt.top/").unwrap(),
            "https://api.sunnypt.top/api/v1/mp"
        );
        assert_eq!(
            sunnypt_api_base("https://api.sunnypt.top").unwrap(),
            "https://api.sunnypt.top/api/v1/mp"
        );
    }

    #[test]
    fn parses_piggo_me() {
        let me = json!({"code": 0, "msg": "ok", "data": {"user": {
            "id": 12, "username": "pig", "class": {"name": "Elite User"},
            "stats": {"uploaded": "1.5 TB", "downloaded": "512 GB", "bonus": "1,234.5"}
        }}});
        let stats = parse_piggo_me(&me).unwrap();
        assert_eq!(stats.uid.as_deref(), Some("12"));
        assert_eq!(stats.uploaded, 1_649_267_441_664);
        assert_eq!(stats.downloaded, 549_755_813_888);
        assert_eq!(stats.ratio, Some(3.0));
        assert_eq!(stats.bonus, Some(1234.5));
        assert_eq!(stats.details.level_name.as_deref(), Some("Elite User"));
        assert_eq!(
            piggo_api_base("https://piggo.me/").unwrap(),
            "https://api.piggo.me/v1/api"
        );
    }

    #[test]
    fn missing_username_means_expired_credentials() {
        assert!(parse_rousi_profile(&json!({"code": 0, "data": {}}), None).is_err());
    }
}
