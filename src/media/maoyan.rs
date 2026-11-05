//! 猫眼榜单：电影实时票房、网络电影、电视剧/网剧/综艺热度（与 MoviePilot「猫眼榜单订阅」同源）。
//!
//! 票房接口可直接请求；热度接口需要页面会话签名，因此在已配置的远程浏览器
//! （Browserless 优先，其次 Lightpanda）中打开猫眼专业版页面后，由页面内 fetch 获取。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{FixedOffset, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::tmdb::{TmdbMedia, TmdbMediaType};

const MAOYAN_BASE: &str = "https://piaofang.maoyan.com";
const HEAT_PAGE: &str = "https://piaofang.maoyan.com/dashboard/web-heat";
/// 后台刷新周期：猫眼热度约每小时更新。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// TMDB 搜索结果缓存时长，避免每轮刷新都重复匹配同名影视。
const TMDB_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
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
    pub const ALL: [Self; 5] = [Self::Movie, Self::Tv, Self::WebSeries, Self::Variety, Self::WebMovie];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::WebMovie => "web_movie",
            Self::Tv => "tv",
            Self::WebSeries => "web_series",
            Self::Variety => "variety",
        }
    }

    /// 票房榜不区分平台。
    pub fn platforms(self) -> &'static [MaoyanPlatform] {
        match self {
            Self::Movie => &[MaoyanPlatform::All],
            _ => &MaoyanPlatform::ALL,
        }
    }

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

    pub fn needs_browser(self) -> bool {
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
    pub const ALL: [Self; 5] = [Self::All, Self::Tencent, Self::Iqiyi, Self::Youku, Self::Mango];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Tencent => "tencent",
            Self::Iqiyi => "iqiyi",
            Self::Youku => "youku",
            Self::Mango => "mango",
        }
    }

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

/// 接口返回的榜单视图：数据来自后台刷新的快照，刷新期间仍返回上一次的数据。
#[derive(Debug, Clone, Serialize)]
pub struct MaoyanRankView {
    pub chart: MaoyanChart,
    pub platform: MaoyanPlatform,
    pub show_date: Option<String>,
    pub fetched_at: Option<String>,
    pub items: Vec<MaoyanRankItem>,
    /// 后台是否正在刷新
    pub refreshing: bool,
    /// 最近一次刷新该榜单失败的原因（有历史数据时仅作提示）
    pub error: Option<String>,
}

type ChartKey = (MaoyanChart, MaoyanPlatform);

/// 猫眼榜单快照：内存中保存最新一次成功的数据（同时持久化到数据库），
/// 刷新失败不会覆盖已有数据。
#[derive(Default)]
pub struct MaoyanStore {
    loaded: tokio::sync::OnceCell<()>,
    entries: Mutex<HashMap<ChartKey, MaoyanRankResponse>>,
    errors: Mutex<HashMap<ChartKey, String>>,
    refreshing: AtomicBool,
    last_attempt: Mutex<Option<Instant>>,
    tmdb_cache: Mutex<HashMap<(String, &'static str), (Instant, Vec<TmdbMedia>)>>,
}

/// 刷新标志的 RAII 守卫：刷新任务结束（包括被取消）时自动清除。
pub struct RefreshGuard<'a>(&'a AtomicBool);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl MaoyanStore {
    pub async fn load_once<F, Fut>(&self, load: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Vec<MaoyanRankResponse>>,
    {
        self.loaded
            .get_or_init(|| async {
                let snapshots = load().await;
                let mut entries = self.entries.lock().await;
                for snapshot in snapshots {
                    entries
                        .entry((snapshot.chart, snapshot.platform))
                        .or_insert(snapshot);
                }
            })
            .await;
    }

    pub fn is_refreshing(&self) -> bool {
        self.refreshing.load(Ordering::SeqCst)
    }

    /// 抢占刷新权；已有刷新在进行时返回 None。
    pub fn try_begin_refresh(&self) -> Option<RefreshGuard<'_>> {
        (!self.refreshing.swap(true, Ordering::SeqCst)).then(|| RefreshGuard(&self.refreshing))
    }

    pub async fn mark_attempt(&self) {
        *self.last_attempt.lock().await = Some(Instant::now());
    }

    /// 是否需要后台刷新：本进程内按上次尝试计时，启动后按快照的抓取时间判断。
    pub async fn is_stale(&self) -> bool {
        if let Some(attempt) = *self.last_attempt.lock().await {
            return attempt.elapsed() >= REFRESH_INTERVAL;
        }
        let entries = self.entries.lock().await;
        let newest = entries
            .values()
            .filter_map(|entry| chrono::DateTime::parse_from_rfc3339(&entry.fetched_at).ok())
            .max();
        match newest {
            Some(newest) => (Utc::now() - newest.with_timezone(&Utc))
                .to_std()
                .map_or(false, |age| age >= REFRESH_INTERVAL),
            None => true,
        }
    }

    pub async fn view(&self, chart: MaoyanChart, platform: MaoyanPlatform, limit: usize) -> MaoyanRankView {
        let entry = self.entries.lock().await.get(&(chart, platform)).cloned();
        let error = self.errors.lock().await.get(&(chart, platform)).cloned();
        let refreshing = self.is_refreshing();
        match entry {
            Some(mut entry) => {
                entry.items.truncate(limit);
                MaoyanRankView {
                    chart,
                    platform,
                    show_date: Some(entry.show_date),
                    fetched_at: Some(entry.fetched_at),
                    items: entry.items,
                    refreshing,
                    error,
                }
            }
            None => MaoyanRankView {
                chart,
                platform,
                show_date: None,
                fetched_at: None,
                items: Vec::new(),
                refreshing,
                error,
            },
        }
    }

    pub async fn put(&self, response: MaoyanRankResponse) {
        let key = (response.chart, response.platform);
        self.errors.lock().await.remove(&key);
        self.entries.lock().await.insert(key, response);
    }

    pub async fn set_error(&self, chart: MaoyanChart, platform: MaoyanPlatform, error: String) {
        self.errors.lock().await.insert((chart, platform), error);
    }

    pub async fn cached_tmdb(&self, query: &str, media_type: &TmdbMediaType) -> Option<Vec<TmdbMedia>> {
        let cache = self.tmdb_cache.lock().await;
        cache
            .get(&(query.to_string(), media_type.as_str()))
            .filter(|(stored, _)| stored.elapsed() < TMDB_CACHE_TTL)
            .map(|(_, results)| results.clone())
    }

    pub async fn store_tmdb(&self, query: String, media_type: &TmdbMediaType, results: Vec<TmdbMedia>) {
        let mut cache = self.tmdb_cache.lock().await;
        cache.retain(|_, (stored, _)| stored.elapsed() < TMDB_CACHE_TTL);
        cache.insert((query, media_type.as_str()), (Instant::now(), results));
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
    let mut results = fetch_chart_payloads_in_browser(settings, &[url.to_string()]).await?;
    results
        .pop()
        .unwrap_or_else(|| Err("浏览器没有返回数据".to_string()))
        .map_err(MaoyanError::Request)
}

/// 在同一个猫眼专业版页面里批量获取多个热度榜，结果与 `urls` 一一对应。
pub async fn fetch_chart_payloads_in_browser(
    settings: &crate::config::GlobalConfig,
    urls: &[String],
) -> Result<Vec<Result<Value, String>>, MaoyanError> {
    crate::sign_in::browser_page_fetch_json(settings, HEAT_PAGE, urls)
        .await
        .map_err(|error| {
            MaoyanError::Browser(format!(
                "猫眼热度榜需要云浏览器获取，请在「自动签到 → 配置签到工具」中配置 Browserless 或 Lightpanda（{error}）"
            ))
        })
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

/// 猫眼的 `*Desc` 展示字段可能使用自定义字体混淆数字（私有区字符），
/// 直接展示会变成方块；只接受由数字和常见单位组成的文本。
fn is_readable_number(value: &str) -> bool {
    !value.is_empty()
        && value.chars().any(|character| character.is_ascii_digit())
        && value
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '.' | ',' | ' ' | '万' | '亿'))
}

fn readable_text(value: Option<&Value>) -> Option<String> {
    text(value).filter(|value| is_readable_number(value))
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite() && *value > 0.0)
}

/// 热度优先取数值字段（不受字体混淆影响），其次才用可读的展示文本。
fn heat_value(entry: &Value) -> Option<String> {
    ["currHeat", "barValue", "heat"]
        .into_iter()
        .find_map(|key| number(entry.get(key)))
        .map(|value| format!("{value:.2}"))
        .or_else(|| readable_text(entry.get("currHeatDesc")))
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
            MaoyanChart::Movie => readable_text(entry.get("sumBoxDesc"))
                .map(|value| format!("累计 {value}")),
            _ => heat_value(entry).map(|value| format!("热度 {value}")),
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

        // 展示字段被字体混淆时改用数值字段；都不可用时不展示热度。
        let obfuscated = json!({"dataList": {"list": [
            {"seriesInfo": {"name": "小城良方"}, "currHeatDesc": "\u{e123}\u{e456}.\u{e789}", "currHeat": 8523.6},
            {"seriesInfo": {"name": "嫁金钗"}, "currHeatDesc": "\u{e123}\u{e456}"}
        ]}});
        let items = parse_chart(MaoyanChart::Tv, &obfuscated, 10).unwrap();
        assert_eq!(items[0].metric.as_deref(), Some("热度 8523.60"));
        assert_eq!(items[1].metric, None);
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
