//! 猫眼榜单：电影实时票房、网络电影、电视剧/网剧/综艺热度（与 MoviePilot「猫眼榜单订阅」同源）。
//!
//! 票房接口可直接请求；热度接口需要页面会话签名，因此在已配置的远程浏览器
//! （Browserless 优先，其次 Lightpanda）中打开猫眼专业版页面后，由页面内 fetch 获取。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{FixedOffset, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::tmdb::{TmdbMedia, TmdbMediaType};

const MAOYAN_BASE: &str = "https://piaofang.maoyan.com";
const HEAT_PAGE: &str = "https://piaofang.maoyan.com/dashboard/web-heat";
const CACHE_TTL: Duration = Duration::from_secs(30 * 60);
pub const MAOYAN_MAX_LIMIT: usize = 30;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaoyanChart {
    /// 电影实时票房
    Movie,
    /// 网络电影热度
    WebMovie,
    /// 电视剧热度
    Tv,
    /// 网剧热度
    WebSeries,
    /// 综艺热度
    Variety,
}

impl MaoyanChart {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "movie" => Some(Self::Movie),
            "web_movie" => Some(Self::WebMovie),
            "tv" => Some(Self::Tv),
            "web_series" => Some(Self::WebSeries),
            "variety" => Some(Self::Variety),
            _ => None,
        }
    }

    pub fn media_type(self) -> TmdbMediaType {
        match self {
            Self::Movie | Self::WebMovie => TmdbMediaType::Movie,
            Self::Tv | Self::WebSeries | Self::Variety => TmdbMediaType::Tv,
        }
    }

    fn series_type(self) -> Option<u8> {
        match self {
            Self::Tv => Some(0),
            Self::WebSeries => Some(1),
            Self::Variety => Some(2),
            Self::Movie | Self::WebMovie => None,
        }
    }

    fn needs_browser(self) -> bool {
        !matches!(self, Self::Movie)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaoyanPlatform {
    All,
    Tencent,
    Iqiyi,
    Youku,
    Mango,
}

impl MaoyanPlatform {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "" | "all" => Some(Self::All),
            "tencent" => Some(Self::Tencent),
            "iqiyi" => Some(Self::Iqiyi),
            "youku" => Some(Self::Youku),
            "mango" => Some(Self::Mango),
            _ => None,
        }
    }

    fn code(self) -> Option<u8> {
        match self {
            Self::All => None,
            Self::Tencent => Some(3),
            Self::Iqiyi => Some(2),
            Self::Youku => Some(1),
            Self::Mango => Some(7),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaoyanRankItem {
    pub rank: usize,
    pub title: String,
    pub release_info: Option<String>,
    pub platform: Option<String>,
    /// 票房或热度的展示文本，例如 "6.71亿" / "热度 8523.6"
    pub metric: Option<String>,
    pub tmdb: Option<TmdbMedia>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaoyanRankResponse {
    pub chart: MaoyanChart,
    pub platform: MaoyanPlatform,
    pub show_date: String,
    pub fetched_at: String,
    pub items: Vec<MaoyanRankItem>,
}

#[derive(Debug, thiserror::Error)]
pub enum MaoyanError {
    #[error("猫眼请求失败：{0}")]
    Request(String),
    #[error("猫眼返回的数据格式无法识别：{0}")]
    Parse(String),
    #[error("{0}")]
    Browser(String),
}

#[derive(Default)]
pub struct MaoyanCache {
    entries: Mutex<HashMap<(MaoyanChart, MaoyanPlatform), (Instant, MaoyanRankResponse)>>,
}

impl MaoyanCache {
    pub async fn get(
        &self,
        chart: MaoyanChart,
        platform: MaoyanPlatform,
    ) -> Option<MaoyanRankResponse> {
        let entries = self.entries.lock().await;
        entries
            .get(&(chart, platform))
            .filter(|(stored, _)| stored.elapsed() < CACHE_TTL)
            .map(|(_, response)| response.clone())
    }

    pub async fn put(&self, response: MaoyanRankResponse) {
        let mut entries = self.entries.lock().await;
        entries.insert((response.chart, response.platform), (Instant::now(), response));
    }
}

pub fn show_date() -> String {
    let shanghai = FixedOffset::east_opt(8 * 3600).expect("UTC+8 is a valid offset");
    Utc::now().with_timezone(&shanghai).format("%Y%m%d").to_string()
}

pub fn chart_url(chart: MaoyanChart, platform: MaoyanPlatform, show_date: &str) -> String {
    match chart {
        MaoyanChart::Movie => format!("{MAOYAN_BASE}/dashboard-ajax/movie"),
        MaoyanChart::WebMovie => format!(
            "{MAOYAN_BASE}/i/api/encrypt/dashboard/webHeatNetData?showDate={show_date}&platformType={}&dateType=0&rankType=0",
            platform.code().unwrap_or(0)
        ),
        _ => {
            let mut url = format!(
                "{MAOYAN_BASE}/i/api/encrypt/dashboard/webHeatData?seriesType={}&showDate={show_date}",
                chart.series_type().unwrap_or(0)
            );
            if let Some(code) = platform.code() {
                url.push_str(&format!("&platformType={code}"));
            }
            url
        }
    }
}

/// 拉取原始榜单 JSON。
pub async fn fetch_chart_payload(
    http: &reqwest::Client,
    settings: &crate::config::GlobalConfig,
    chart: MaoyanChart,
    url: &str,
) -> Result<Value, MaoyanError> {
    if !chart.needs_browser() {
        let response = http
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|error| MaoyanError::Request(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(MaoyanError::Request(format!("HTTP {}", status.as_u16())));
        }
        return response
            .json::<Value>()
            .await
            .map_err(|error| MaoyanError::Parse(error.to_string()));
    }
    let mut results =
        crate::sign_in::browser_page_fetch_json(settings, HEAT_PAGE, &[url.to_string()])
            .await
            .map_err(|error| {
                MaoyanError::Browser(format!(
                    "猫眼热度榜需要云浏览器获取，请在「自动签到 → 配置签到工具」中配置 Browserless 或 Lightpanda（{error}）"
                ))
            })?;
    results
        .pop()
        .unwrap_or_else(|| Err("浏览器没有返回数据".to_string()))
        .map_err(MaoyanError::Request)
}

/// 网络电影在当日数据未生成时返回 `status: false` 并给出最近可用日期。
pub fn latest_available_date(payload: &Value) -> Option<String> {
    if payload.get("status").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let latest = payload
        .pointer("/calendarNet/selectMaxDate")
        .and_then(Value::as_str)?
        .replace('-', "");
    (latest.len() == 8 && latest.bytes().all(|byte| byte.is_ascii_digit())).then_some(latest)
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// 解析榜单条目；坏条目被跳过，不影响同榜其他条目。
pub fn parse_chart(
    chart: MaoyanChart,
    payload: &Value,
    limit: usize,
) -> Result<Vec<MaoyanRankItem>, MaoyanError> {
    let (list_key, info_key, title_key) = match chart {
        MaoyanChart::Movie => ("movieList", Some("movieInfo"), "movieName"),
        MaoyanChart::WebMovie => ("dataList", None, "movieName"),
        _ => ("dataList", Some("seriesInfo"), "name"),
    };
    let list = payload
        .get(list_key)
        .and_then(|container| container.get("list"))
        .and_then(Value::as_array)
        .ok_or_else(|| MaoyanError::Parse(format!("缺少 {list_key}.list")))?;
    let mut items = Vec::new();
    for entry in list {
        if items.len() >= limit {
            break;
        }
        let info = match info_key {
            Some(key) => entry.get(key),
            None => Some(entry),
        };
        let Some(info) = info.filter(|info| info.is_object()) else {
            continue;
        };
        let Some(title) = text(info.get(title_key)) else {
            continue;
        };
        let metric = match chart {
            MaoyanChart::Movie => text(entry.get("sumBoxDesc")).map(|value| format!("累计 {value}")),
            _ => text(entry.get("currHeatDesc"))
                .or_else(|| text(entry.get("currHeat")))
                .map(|value| format!("热度 {value}")),
        };
        items.push(MaoyanRankItem {
            rank: items.len() + 1,
            title,
            release_info: text(info.get("releaseInfo")),
            platform: text(info.get("platformDesc")),
            metric,
            tmdb: None,
        });
    }
    Ok(items)
}

fn normalize(title: &str) -> String {
    title
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 猫眼片名常带 "第二季" 等后缀，TMDB 以剧集为单位，先去掉季号再匹配。
pub fn tmdb_query_title(title: &str) -> String {
    let trimmed = title.trim();
    if let Some(index) = trimmed.find("第")
        && trimmed[index..].contains('季')
        && index > 0
    {
        return trimmed[..index].trim().to_string();
    }
    trimmed.to_string()
}

pub fn pick_tmdb_match(title: &str, results: Vec<TmdbMedia>) -> Option<TmdbMedia> {
    let wanted = normalize(&tmdb_query_title(title));
    let exact = results.iter().position(|media| {
        normalize(&media.title) == wanted
            || media
                .original_title
                .as_deref()
                .is_some_and(|original| normalize(original) == wanted)
    });
    let index = exact.unwrap_or(0);
    results.into_iter().nth(index)
}

/// 为榜单条目并发匹配 TMDB 条目；匹配失败只留空，不影响榜单展示。
pub async fn attach_tmdb<F, Fut>(items: &mut [MaoyanRankItem], media_type: TmdbMediaType, search: F)
where
    F: Fn(String, TmdbMediaType) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<TmdbMedia>>>,
{
    let queries: Vec<_> = items
        .iter()
        .map(|item| (item.rank, tmdb_query_title(&item.title)))
        .collect();
    let mut matches: HashMap<usize, Vec<TmdbMedia>> = futures::stream::iter(queries)
        .map(|(rank, query)| {
            let future = search(query, media_type.clone());
            async move { (rank, future.await) }
        })
        .buffer_unordered(4)
        .filter_map(|(rank, results)| async move { results.map(|results| (rank, results)) })
        .collect()
        .await;
    for item in items.iter_mut() {
        if let Some(results) = matches.remove(&item.rank) {
            item.tmdb = pick_tmdb_match(&item.title, results);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_box_office_and_heat_payloads() {
        let movie = json!({"movieList": {"list": [
            {"movieInfo": {"movieName": "神探之痕迹", "releaseInfo": "上映36天"}, "sumBoxDesc": "6.71亿"},
            {"movieInfo": {}},
            {"movieInfo": {"movieName": "第二部", "releaseInfo": null}}
        ]}});
        let items = parse_chart(MaoyanChart::Movie, &movie, 10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "神探之痕迹");
        assert_eq!(items[0].metric.as_deref(), Some("累计 6.71亿"));
        assert_eq!(items[1].rank, 2);

        let heat = json!({"dataList": {"list": [
            {"seriesInfo": {"name": "庆余年 第二季", "platformDesc": "腾讯视频", "releaseInfo": "上线5天"}, "currHeatDesc": "8523.6"}
        ]}});
        let items = parse_chart(MaoyanChart::Tv, &heat, 1).unwrap();
        assert_eq!(items[0].platform.as_deref(), Some("腾讯视频"));
        assert_eq!(items[0].metric.as_deref(), Some("热度 8523.6"));

        assert!(parse_chart(MaoyanChart::Tv, &json!({"status": false}), 5).is_err());
    }

    #[test]
    fn urls_follow_the_moviepilot_parameters() {
        assert_eq!(
            chart_url(MaoyanChart::WebSeries, MaoyanPlatform::Tencent, "20261105"),
            "https://piaofang.maoyan.com/i/api/encrypt/dashboard/webHeatData?seriesType=1&showDate=20261105&platformType=3"
        );
        assert_eq!(
            chart_url(MaoyanChart::Variety, MaoyanPlatform::All, "20261105"),
            "https://piaofang.maoyan.com/i/api/encrypt/dashboard/webHeatData?seriesType=2&showDate=20261105"
        );
    }

    #[test]
    fn season_suffix_is_removed_for_tmdb_lookup() {
        assert_eq!(tmdb_query_title("庆余年 第二季"), "庆余年");
        assert_eq!(tmdb_query_title("第一炉香"), "第一炉香");
        assert_eq!(
            latest_available_date(&json!({"status": false, "calendarNet": {"selectMaxDate": "2026-11-04"}})),
            Some("20261104".to_string())
        );
    }
}
