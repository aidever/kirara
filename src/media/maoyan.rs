//! 猫眼榜单：电影实时票房、影史/年度票房、全球年度票房、网络电影、电视剧/网剧/综艺热度
//! （与 MoviePilot 插件 GlobalRankPro 的猫眼数据源同源）。
//!
//! 实时票房接口与影史/年度票房页面（服务端直出 HTML）可直接请求；全球年度票房来自
//! Box Office Mojo；热度接口需要页面会话签名，因此在已配置的远程浏览器
//! （Browserless 优先，其次 Lightpanda）中打开猫眼专业版页面后，由页面内 fetch 获取。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::douban::{DoubanClient, DoubanSubject};
use super::tmdb::{TmdbClient, TmdbMedia, TmdbMediaType};

const MAOYAN_BASE: &str = "https://piaofang.maoyan.com";
const HEAT_PAGE: &str = "https://piaofang.maoyan.com/dashboard/web-heat";
/// 后台刷新周期：猫眼热度约每小时更新。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// 匹配结果缓存时长，避免每轮刷新都重复匹配同名影视。
const MATCH_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
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
    /// 剧集综合热度（电视剧 + 网剧，猫眼不带 seriesType 的综合榜）
    WebAll,
    /// 影史总票房（猫眼 rankings/year，不带年份）
    Total,
    /// 国内年度票房（猫眼 rankings/year?year=YYYY）
    Year,
    /// 全球年度票房（Box Office Mojo）
    Global,
}

impl MaoyanChart {
    pub const ALL: [Self; 9] = [
        Self::Movie,
        Self::WebAll,
        Self::Tv,
        Self::WebSeries,
        Self::Variety,
        Self::WebMovie,
        Self::Total,
        Self::Year,
        Self::Global,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::WebMovie => "web_movie",
            Self::Tv => "tv",
            Self::WebSeries => "web_series",
            Self::Variety => "variety",
            Self::WebAll => "web_all",
            Self::Total => "total",
            Self::Year => "year",
            Self::Global => "global",
        }
    }

    /// 各榜单可选的平台，第一个为默认。票房榜不区分平台；
    /// 网络电影分账榜猫眼只持续更新腾讯视频（全网、芒果无效，爱奇艺、优酷已停更）。
    pub fn platforms(self) -> &'static [MaoyanPlatform] {
        match self {
            Self::Movie | Self::Total | Self::Year | Self::Global => &[MaoyanPlatform::All],
            Self::WebMovie => &[MaoyanPlatform::Tencent],
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
            "web_all" | "web-all" => Some(Self::WebAll),
            "total" => Some(Self::Total),
            "year" => Some(Self::Year),
            "global" => Some(Self::Global),
            _ => None,
        }
    }

    pub fn media_type(self) -> TmdbMediaType {
        match self {
            Self::Movie | Self::WebMovie | Self::Total | Self::Year | Self::Global => {
                TmdbMediaType::Movie
            }
            Self::Tv | Self::WebSeries | Self::Variety | Self::WebAll => TmdbMediaType::Tv,
        }
    }

    fn series_type(self) -> Option<u8> {
        match self {
            Self::Tv => Some(0),
            Self::WebSeries => Some(1),
            Self::Variety => Some(2),
            Self::WebAll
            | Self::Movie
            | Self::WebMovie
            | Self::Total
            | Self::Year
            | Self::Global => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Movie => "电影票房",
            Self::WebMovie => "网络电影",
            Self::Tv => "电视剧热度",
            Self::WebSeries => "网剧热度",
            Self::Variety => "综艺热度",
            Self::WebAll => "剧集综合",
            Self::Total => "影史总票房",
            Self::Year => "国内年度票房",
            Self::Global => "全球年度票房",
        }
    }

    /// 榜单的官方页面，供界面标注数据来源。
    pub fn page_url(self, show_date: &str) -> String {
        let year = show_date.get(..4).unwrap_or(show_date);
        match self {
            Self::Movie => format!("{MAOYAN_BASE}/dashboard/movie"),
            Self::WebMovie => format!("{MAOYAN_BASE}/dashboard/web-heat"),
            Self::Tv | Self::WebSeries | Self::Variety | Self::WebAll => HEAT_PAGE.to_string(),
            Self::Total => format!("{MAOYAN_BASE}/rankings/year"),
            Self::Year => format!("{MAOYAN_BASE}/rankings/year?year={year}"),
            Self::Global => format!("https://www.boxofficemojo.com/year/world/{year}/"),
        }
    }

    /// 数据来源名称。
    pub fn origin(self) -> &'static str {
        match self {
            Self::Global => "Box Office Mojo",
            _ => "猫眼专业版",
        }
    }

    pub fn needs_browser(self) -> bool {
        matches!(
            self,
            Self::WebMovie | Self::Tv | Self::WebSeries | Self::Variety | Self::WebAll
        )
    }

    /// 服务端直出 HTML 的票房榜（影史 / 年度 / 全球），直接请求页面解析。
    pub fn is_html(self) -> bool {
        matches!(self, Self::Total | Self::Year | Self::Global)
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
    pub const ALL: [Self; 5] = [
        Self::All,
        Self::Tencent,
        Self::Iqiyi,
        Self::Youku,
        Self::Mango,
    ];

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

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "全网",
            Self::Tencent => "腾讯视频",
            Self::Iqiyi => "爱奇艺",
            Self::Youku => "优酷",
            Self::Mango => "芒果TV",
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
    /// 旧版快照里的合并指标文本；新数据使用下面的分项字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    /// 实时票房（电影票房榜为当日综合票房，网络电影为当月分账，全球榜为北美票房）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub box_office: Option<String>,
    /// 累计票房（影史 / 年度榜的总票房，全球榜的全球票房）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_box_office: Option<String>,
    /// 热度值
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heat: Option<String>,
    /// 票房占比
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub box_rate: Option<String>,
    /// 排片占比
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_rate: Option<String>,
    /// 播放量（只有公开播放量的平台才有，如芒果TV）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_count: Option<String>,
    /// 主演（来自 TMDB 演职员表）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cast: Vec<String>,
    /// TMDB 评分
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb_rating: Option<f64>,
    /// 上次查询 TMDB 评分 / 主演的时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb_checked_at: Option<String>,
    /// 上次补查豆瓣评分的时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub douban_checked_at: Option<String>,
    #[serde(default)]
    pub douban: Option<DoubanSubject>,
    pub tmdb: Option<TmdbMedia>,
}

impl MaoyanRankItem {
    fn new(rank: usize, title: String) -> Self {
        Self {
            rank,
            title,
            release_info: None,
            platform: None,
            metric: None,
            box_office: None,
            total_box_office: None,
            heat: None,
            box_rate: None,
            show_rate: None,
            play_count: None,
            cast: Vec::new(),
            tmdb_rating: None,
            tmdb_checked_at: None,
            douban_checked_at: None,
            douban: None,
            tmdb: None,
        }
    }
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
    #[error("猫眼暂无该榜单的数据")]
    NoData,
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
    match_cache: Mutex<HashMap<(String, &'static str), (Instant, MediaMatch)>>,
    posters: Mutex<std::collections::VecDeque<(String, String)>>,
    infos: Mutex<HashMap<String, (Instant, super::douban::DoubanInfo)>>,
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

    pub async fn view(
        &self,
        chart: MaoyanChart,
        platform: MaoyanPlatform,
        limit: usize,
    ) -> MaoyanRankView {
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

    /// 全部榜单快照及各自最近一次刷新失败的原因，供全球榜单的统一视图使用。
    pub async fn all(&self) -> Vec<(MaoyanRankResponse, Option<String>)> {
        let entries = self.entries.lock().await;
        let errors = self.errors.lock().await;
        entries
            .values()
            .map(|entry| {
                (
                    entry.clone(),
                    errors.get(&(entry.chart, entry.platform)).cloned(),
                )
            })
            .collect()
    }

    /// 还没有任何数据的榜单的失败原因。
    pub async fn pending_errors(&self) -> Vec<(MaoyanChart, MaoyanPlatform, String)> {
        let entries = self.entries.lock().await;
        let errors = self.errors.lock().await;
        errors
            .iter()
            .filter(|(key, _)| !entries.contains_key(key))
            .map(|((chart, platform), error)| (*chart, *platform, error.clone()))
            .collect()
    }

    pub async fn put(&self, response: MaoyanRankResponse) {
        let key = (response.chart, response.platform);
        self.errors.lock().await.remove(&key);
        self.entries.lock().await.insert(key, response);
    }

    pub async fn set_error(&self, chart: MaoyanChart, platform: MaoyanPlatform, error: String) {
        self.errors.lock().await.insert((chart, platform), error);
    }

    /// 已保存快照中的匹配结果，按（片名, 类型）索引；只收录匹配到 TMDB 的条目。
    /// 之前快照里补充过的信息（豆瓣、TMDB 评分、主演），按（片名, 类型）复用。
    pub async fn previous_extras(&self) -> HashMap<(String, &'static str), MaoyanRankItem> {
        let entries = self.entries.lock().await;
        let mut extras: HashMap<(String, &'static str), MaoyanRankItem> = HashMap::new();
        for entry in entries.values() {
            for item in &entry.items {
                let key = (item.title.clone(), entry.chart.media_type().as_str());
                let newer = extras.get(&key).is_none_or(|existing| {
                    existing.tmdb_checked_at < item.tmdb_checked_at
                        || existing.douban_checked_at < item.douban_checked_at
                });
                if newer {
                    extras.insert(key, item.clone());
                }
            }
        }
        extras
    }

    pub async fn previous_matches(&self) -> HashMap<(String, &'static str), MediaMatch> {
        let entries = self.entries.lock().await;
        let mut matches = HashMap::new();
        for entry in entries.values() {
            for item in &entry.items {
                if item.tmdb.is_some() {
                    matches
                        .entry((item.title.clone(), entry.chart.media_type().as_str()))
                        .or_insert_with(|| MediaMatch {
                            douban: item.douban.clone(),
                            tmdb: item.tmdb.clone(),
                        });
                }
            }
        }
        matches
    }

    pub async fn cached_info(&self, key: &str) -> Option<super::douban::DoubanInfo> {
        let infos = self.infos.lock().await;
        infos
            .get(key)
            .filter(|(stored, _)| stored.elapsed() < Duration::from_secs(6 * 60 * 60))
            .map(|(_, info)| info.clone())
    }

    pub async fn store_info(&self, key: String, info: super::douban::DoubanInfo) {
        let mut infos = self.infos.lock().await;
        infos.retain(|_, (stored, _)| stored.elapsed() < Duration::from_secs(6 * 60 * 60));
        if infos.len() >= 300 {
            infos.clear();
        }
        infos.insert(key, (Instant::now(), info));
    }

    pub async fn cached_poster(&self, id: &str) -> Option<String> {
        let posters = self.posters.lock().await;
        posters
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, value)| value.clone())
    }

    pub async fn store_poster(&self, id: String, data_url: String) {
        let mut posters = self.posters.lock().await;
        posters.retain(|(key, _)| key != &id);
        if posters.len() >= 300 {
            posters.pop_front();
        }
        posters.push_back((id, data_url));
    }

    pub async fn cached_match(
        &self,
        title: &str,
        media_type: &TmdbMediaType,
    ) -> Option<MediaMatch> {
        let cache = self.match_cache.lock().await;
        cache
            .get(&(title.to_string(), media_type.as_str()))
            .filter(|(stored, _)| stored.elapsed() < MATCH_CACHE_TTL)
            .map(|(_, found)| found.clone())
    }

    pub async fn store_match(&self, title: String, media_type: &TmdbMediaType, found: MediaMatch) {
        let mut cache = self.match_cache.lock().await;
        cache.retain(|_, (stored, _)| stored.elapsed() < MATCH_CACHE_TTL);
        cache.insert((title, media_type.as_str()), (Instant::now(), found));
    }
}

pub fn show_date() -> String {
    let shanghai = FixedOffset::east_opt(8 * 3600).expect("UTC+8 is a valid offset");
    Utc::now()
        .with_timezone(&shanghai)
        .format("%Y%m%d")
        .to_string()
}

pub fn chart_url(chart: MaoyanChart, platform: MaoyanPlatform, show_date: &str) -> String {
    let year = show_date.get(..4).unwrap_or(show_date);
    match chart {
        MaoyanChart::Movie => format!("{MAOYAN_BASE}/dashboard-ajax/movie"),
        MaoyanChart::Total => format!("{MAOYAN_BASE}/rankings/year"),
        MaoyanChart::Year => format!("{MAOYAN_BASE}/rankings/year?year={year}"),
        MaoyanChart::Global => format!("https://www.boxofficemojo.com/year/world/{year}/"),
        // 网络电影只有分账榜：按月统计（dateType=2，日期取当月 1 日），日榜通常只有一两部。
        MaoyanChart::WebMovie => format!(
            "{MAOYAN_BASE}/i/api/encrypt/dashboard/webHeatNetData?showDate={}01&platformType={}&dateType=2&rankType=0",
            show_date.get(..6).unwrap_or(show_date),
            platform.code().unwrap_or(3)
        ),
        _ => {
            // 不带 seriesType 时猫眼返回电视剧 + 网剧的综合榜。
            let mut url = match chart.series_type() {
                Some(series) => format!(
                    "{MAOYAN_BASE}/i/api/encrypt/dashboard/webHeatData?seriesType={series}&showDate={show_date}"
                ),
                None => format!(
                    "{MAOYAN_BASE}/i/api/encrypt/dashboard/webHeatData?showDate={show_date}"
                ),
            };
            if let Some(code) = platform.code() {
                url.push_str(&format!("&platformType={code}"));
            }
            // 官网默认只给前 10 名，显式请求前 30 名。
            url.push_str(&format!("&limit={MAOYAN_MAX_LIMIT}"));
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

/// 获取并解析影史 / 年度 / 全球票房榜（服务端直出 HTML，无需签名）。
pub async fn fetch_html_chart(
    http: &reqwest::Client,
    chart: MaoyanChart,
    url: &str,
    limit: usize,
) -> Result<Vec<MaoyanRankItem>, MaoyanError> {
    let response = http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
        .header(
            reqwest::header::ACCEPT_LANGUAGE,
            "zh-CN,zh;q=0.9,en-US;q=0.8,en;q=0.7",
        )
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| MaoyanError::Request(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(MaoyanError::Request(format!("HTTP {}", status.as_u16())));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| MaoyanError::Request(error.to_string()))?;
    let html = String::from_utf8_lossy(&bytes);
    let items = match chart {
        MaoyanChart::Global => parse_box_office_mojo(&html, limit),
        _ => parse_maoyan_rankings(&html, limit),
    };
    if items.is_empty() {
        return Err(MaoyanError::Parse("页面中没有找到榜单条目".to_string()));
    }
    Ok(items)
}

fn element_text(element: scraper::ElementRef<'_>) -> String {
    element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 猫眼影史 / 年度票房页：`ul.row` 每行 col0 名次、col1 片名+上映日期、col2 票房（万元）。
pub fn parse_maoyan_rankings(html: &str, limit: usize) -> Vec<MaoyanRankItem> {
    let document = scraper::Html::parse_document(html);
    let row = scraper::Selector::parse("ul.row").expect("valid selector");
    let first = scraper::Selector::parse(".first-line").expect("valid selector");
    let second = scraper::Selector::parse(".second-line").expect("valid selector");
    let rank = scraper::Selector::parse("li.col0").expect("valid selector");
    let box_office = scraper::Selector::parse("li.col2").expect("valid selector");
    let mut items = Vec::new();
    for element in document.select(&row) {
        if items.len() >= limit {
            break;
        }
        let Some(title) = element
            .select(&first)
            .next()
            .map(element_text)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let position = element
            .select(&rank)
            .next()
            .and_then(|cell| element_text(cell).parse::<usize>().ok())
            .unwrap_or(items.len() + 1);
        let total = element
            .select(&box_office)
            .next()
            .and_then(|cell| element_text(cell).replace(',', "").parse::<f64>().ok())
            .filter(|value| *value > 0.0)
            .map(|wan| format_yuan(wan * 10_000.0));
        let mut item = MaoyanRankItem::new(position, title);
        item.release_info = element
            .select(&second)
            .next()
            .map(element_text)
            .filter(|text| !text.is_empty());
        item.total_box_office = total;
        items.push(item);
    }
    items
}

fn format_usd(value: f64) -> String {
    if value >= 100_000_000.0 {
        format!("{:.2}亿美元", value / 100_000_000.0)
    } else if value >= 10_000.0 {
        format!("{:.0}万美元", value / 10_000.0)
    } else {
        format!("{value:.0}美元")
    }
}

/// Box Office Mojo 全球年度榜：每行名次、片名、全球票房、北美票房、北美占比、海外票房、海外占比。
pub fn parse_box_office_mojo(html: &str, limit: usize) -> Vec<MaoyanRankItem> {
    let document = scraper::Html::parse_document(html);
    let row = scraper::Selector::parse("tr").expect("valid selector");
    let cell = scraper::Selector::parse("td").expect("valid selector");
    let money = |text: &str| {
        text.replace(['$', ','], "")
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| *value > 0.0)
    };
    let mut items = Vec::new();
    for element in document.select(&row) {
        if items.len() >= limit {
            break;
        }
        let cells: Vec<String> = element.select(&cell).map(element_text).collect();
        if cells.len() < 3 || cells[1].is_empty() {
            continue;
        }
        let worldwide = money(&cells[2]);
        let domestic = cells.get(3).and_then(|value| money(value));
        let mut item = MaoyanRankItem::new(
            cells[0].parse().unwrap_or(items.len() + 1),
            cells[1].clone(),
        );
        item.total_box_office = worldwide.map(format_usd);
        item.box_office = domestic.map(format_usd);
        items.push(item);
    }
    items
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
    // 无效平台会给出未来的日期，重试也拿不到数据。
    (latest.len() == 8 && latest.bytes().all(|byte| byte.is_ascii_digit()) && latest <= show_date())
        .then_some(latest)
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
        && value.chars().all(|character| {
            character.is_ascii_digit() || matches!(character, '.' | ',' | ' ' | '万' | '亿')
        })
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

fn format_yuan(value: f64) -> String {
    let trim = |text: String| {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            text
        }
    };
    if value >= 100_000_000.0 {
        format!("{}亿", trim(format!("{:.2}", value / 100_000_000.0)))
    } else if value >= 10_000.0 {
        format!("{}万", trim(format!("{:.1}", value / 10_000.0)))
    } else {
        format!("{}元", value.round())
    }
}

/// `{num, unit}` 形式的金额，例如 `{"num": "70.02", "unit": "万"}`。
fn split_unit(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let num = text(value.get("num"))?;
    let unit = text(value.get("unit")).unwrap_or_default();
    Some(format!("{num}{unit}")).filter(|text| is_readable_number(text))
}

/// 百分比文本（如 "29.4%"）；被字体混淆时丢弃。
fn percent_text(value: Option<&Value>) -> Option<String> {
    text(value).filter(|value| {
        value.ends_with('%')
            && value
                .trim_end_matches('%')
                .parse::<f64>()
                .is_ok_and(|number| number > 0.0)
    })
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
    if payload.get("status").and_then(Value::as_bool) == Some(false) {
        return Err(MaoyanError::NoData);
    }
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
        let mut item = MaoyanRankItem::new(items.len() + 1, title);
        item.release_info = text(info.get("releaseInfo"));
        item.platform = text(info.get("platformDesc")).or_else(|| text(entry.get("platformDesc")));
        match chart {
            MaoyanChart::Movie => {
                // 数值字段单位为元，不受字体混淆影响；展示文本只在可读时使用。
                item.box_office = number(entry.get("box"))
                    .map(format_yuan)
                    .or_else(|| split_unit(entry.get("boxSplitUnit")));
                item.total_box_office = readable_text(entry.get("sumBoxDesc"));
                item.box_rate = percent_text(entry.get("boxRate"));
                item.show_rate = percent_text(entry.get("showCountRate"));
            }
            // 分账展示文字被字体混淆（如 ".万"），数值字段单位为元。
            MaoyanChart::WebMovie => {
                item.box_office = number(entry.get("splitBox")).map(format_yuan);
                item.heat = heat_value(entry);
            }
            _ => {
                item.heat = heat_value(entry);
                item.play_count = split_unit(entry.get("playCountSplitUnit"));
            }
        }
        items.push(item);
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
    let exact = results
        .iter()
        .position(|media| title_matches(media, &wanted));
    let index = exact.unwrap_or(0);
    results.into_iter().nth(index)
}

fn title_matches(media: &TmdbMedia, wanted: &str) -> bool {
    normalize(&media.title) == wanted
        || media
            .original_title
            .as_deref()
            .is_some_and(|original| normalize(original) == wanted)
}

fn year_close(media: &TmdbMedia, year: Option<u32>) -> bool {
    match (media.year, year) {
        (Some(found), Some(wanted)) => found.abs_diff(wanted) <= 1,
        _ => false,
    }
}

/// 在豆瓣结果中找与榜单片名完全一致的条目（忽略标点空格），同名时取最新的。
pub fn pick_douban(
    title: &str,
    media_type: &TmdbMediaType,
    subjects: Vec<DoubanSubject>,
) -> Option<DoubanSubject> {
    let full = normalize(title);
    let base = normalize(&tmdb_query_title(title));
    // 完全同名优先；其次是同一部剧的某一季（榜单常不写季号），取最新一季。
    subjects
        .into_iter()
        .filter(|subject| &subject.media_type == media_type)
        .filter_map(|subject| {
            let name = normalize(&subject.title);
            let priority = if name == full {
                2
            } else if name == base || normalize(&tmdb_query_title(&subject.title)) == base {
                1
            } else {
                return None;
            };
            Some(((priority, subject.year.unwrap_or(0)), subject))
        })
        .max_by_key(|(key, _)| *key)
        .map(|(_, subject)| subject)
}

/// 用豆瓣给出的名称和年份在 TMDB 结果中挑选：片名一致优先，其次年份相符。
pub fn pick_tmdb_for(
    names: &[String],
    year: Option<u32>,
    results: Vec<TmdbMedia>,
) -> Option<TmdbMedia> {
    let wanted: Vec<String> = names
        .iter()
        .map(|name| normalize(&tmdb_query_title(name)))
        .collect();
    let exact_with_year = results.iter().position(|media| {
        wanted.iter().any(|name| title_matches(media, name))
            && (year.is_none() || year_close(media, year))
    });
    let exact = || {
        results
            .iter()
            .position(|media| wanted.iter().any(|name| title_matches(media, name)))
    };
    let by_year = || results.iter().position(|media| year_close(media, year));
    let index = exact_with_year.or_else(exact).or_else(by_year)?;
    results.into_iter().nth(index)
}

/// 榜单条目的匹配结果。
#[derive(Debug, Clone, Default)]
pub struct MediaMatch {
    pub douban: Option<DoubanSubject>,
    pub tmdb: Option<TmdbMedia>,
}

/// 匹配一个榜单片名：先查豆瓣，再按豆瓣信息（年份、IMDb、原名、又名）定位 TMDB；
/// 豆瓣找不到时直接搜索 TMDB。返回值第二项表示结果是否可靠（期间没有请求失败），
/// 只有可靠的结果才应缓存，失败的下次刷新会重试。
pub async fn resolve_media(
    title: &str,
    media_type: &TmdbMediaType,
    douban: &DoubanClient,
    tmdb: Option<&TmdbClient>,
) -> (MediaMatch, bool) {
    let mut reliable = true;
    let subject = match douban.search(title).await {
        Ok(subjects) => pick_douban(title, media_type, subjects),
        Err(error) => {
            tracing::debug!(%error, title, "douban search failed");
            reliable = false;
            None
        }
    };
    let Some(tmdb) = tmdb else {
        return (
            MediaMatch {
                douban: subject,
                tmdb: None,
            },
            false,
        );
    };
    fn note(
        result: Result<Option<TmdbMedia>, super::tmdb::TmdbError>,
        reliable: &mut bool,
        title: &str,
    ) -> Option<TmdbMedia> {
        match result {
            Ok(found) => found,
            Err(error) => {
                tracing::debug!(%error, title, "tmdb lookup failed");
                *reliable = false;
                None
            }
        }
    }

    let mut found = None;
    if let Some(subject) = &subject {
        let base = tmdb_query_title(&subject.title);
        // 带季号的剧集在 TMDB 以首季年份收录，不能按本季年份过滤。
        let year = if base == subject.title.trim() {
            subject.year
        } else {
            None
        };
        let names = vec![subject.title.clone()];
        found = note(
            tmdb.search_typed(&base, media_type, year)
                .await
                .map(|results| pick_tmdb_for(&names, year, results)),
            &mut reliable,
            title,
        );
        if found.is_none() {
            match douban.detail(subject).await {
                Ok(detail) => {
                    if let Some(imdb) = detail.imdb.as_deref() {
                        found = note(
                            tmdb.find_by_imdb(imdb, media_type).await,
                            &mut reliable,
                            title,
                        );
                    }
                    let aliases: Vec<String> = detail
                        .original_title
                        .into_iter()
                        .chain(detail.aka)
                        .filter(|name| name.trim() != subject.title.trim())
                        .take(3)
                        .collect();
                    for alias in aliases {
                        if found.is_some() {
                            break;
                        }
                        let names = vec![alias.clone(), subject.title.clone()];
                        found = note(
                            tmdb.search_typed(&tmdb_query_title(&alias), media_type, year)
                                .await
                                .map(|results| pick_tmdb_for(&names, year, results)),
                            &mut reliable,
                            title,
                        );
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, title, "douban detail failed");
                    reliable = false;
                }
            }
        }
    }
    if found.is_none() {
        // 豆瓣没有收录时沿用宽松匹配；豆瓣有条目时仍要求片名或年份相符，避免张冠李戴。
        let names = vec![title.to_string()];
        let year = subject.as_ref().and_then(|subject| subject.year);
        let lenient = subject.is_none();
        found = note(
            tmdb.search_typed(&tmdb_query_title(title), media_type, None)
                .await
                .map(|results| {
                    if lenient {
                        pick_tmdb_match(title, results)
                    } else {
                        pick_tmdb_for(&names, year, results)
                    }
                }),
            &mut reliable,
            title,
        );
    }
    let reliable = reliable || found.is_some();
    (
        MediaMatch {
            douban: subject,
            tmdb: found,
        },
        reliable,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heat_items_keep_the_published_play_count() {
        let heat = json!({"dataList": {"list": [
            {"barValue": 7017.64, "seriesInfo": {"name": "玉簪秋", "platformDesc": "芒果TV独播", "releaseInfo": "上线22天"}, "playCountSplitUnit": {"num": "8174.8", "unit": "万"}},
            {"barValue": 6535.16, "seriesInfo": {"name": "兰香如故", "platformDesc": "腾讯视频独播"}, "playCountSplitUnit": null}
        ]}});
        let items = parse_chart(MaoyanChart::Tv, &heat, 30).unwrap();
        assert_eq!(items[0].play_count.as_deref(), Some("8174.8万"));
        assert_eq!(items[1].play_count, None);
    }

    #[test]
    fn parses_box_office_and_heat_payloads() {
        let movie = json!({"movieList": {"list": [
            {"movieInfo": {"movieName": "神探之痕迹", "releaseInfo": "上映36天"}, "sumBoxDesc": "6.71亿", "box": 70024890, "boxRate": "29.4%", "showCountRate": "15.8%", "boxSplitUnit": {"num": "70.02", "unit": "万"}},
            {"movieInfo": {}},
            {"movieInfo": {"movieName": "第二部", "releaseInfo": null}}
        ]}});
        let items = parse_chart(MaoyanChart::Movie, &movie, 10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "神探之痕迹");
        assert_eq!(items[0].total_box_office.as_deref(), Some("6.71亿"));
        assert_eq!(items[0].box_office.as_deref(), Some("7002.5万"));
        assert_eq!(items[0].box_rate.as_deref(), Some("29.4%"));
        assert_eq!(items[0].show_rate.as_deref(), Some("15.8%"));
        assert_eq!(items[1].box_office, None);
        assert_eq!(items[1].rank, 2);

        let heat = json!({"dataList": {"list": [
            {"seriesInfo": {"name": "庆余年 第二季", "platformDesc": "腾讯视频", "releaseInfo": "上线5天"}, "currHeatDesc": "8523.6"}
        ]}});
        let items = parse_chart(MaoyanChart::Tv, &heat, 1).unwrap();
        assert_eq!(items[0].platform.as_deref(), Some("腾讯视频"));
        assert_eq!(items[0].heat.as_deref(), Some("8523.6"));

        assert!(matches!(
            parse_chart(MaoyanChart::Tv, &json!({"status": false}), 5),
            Err(MaoyanError::NoData)
        ));

        // 真实返回：currHeat 为混淆后的 "."，热度在 barValue；网络电影分账在 splitBox（万元）。
        let tv = json!({"status": true, "dataList": {"list": [
            {"barValue": 6517.03, "currHeat": ".", "currHeatDesc": ".", "seriesInfo": {"name": "小城良方", "platformDesc": "爱奇艺独播", "releaseInfo": "上线16天"}}
        ]}});
        assert_eq!(
            parse_chart(MaoyanChart::Tv, &tv, 5).unwrap()[0]
                .heat
                .as_deref(),
            Some("6517.03")
        );
        let web = json!({"status": true, "dataList": {"list": [
            {"movieName": "僵尸参将2", "releaseInfo": "上映30天", "splitBox": 42900000, "splitBoxDesc": ".万"},
            {"movieName": "大片", "splitBox": 123456789.0},
            {"movieName": "小片", "splitBox": 5500}
        ]}});
        let items = parse_chart(MaoyanChart::WebMovie, &web, 5).unwrap();
        assert_eq!(items[0].box_office.as_deref(), Some("4290万"));
        assert_eq!(items[1].box_office.as_deref(), Some("1.23亿"));
        assert_eq!(items[2].box_office.as_deref(), Some("5500元"));

        // 展示字段被字体混淆时改用数值字段；都不可用时不展示热度。
        let obfuscated = json!({"dataList": {"list": [
            {"seriesInfo": {"name": "小城良方"}, "currHeatDesc": "\u{e123}\u{e456}.\u{e789}", "currHeat": 8523.6},
            {"seriesInfo": {"name": "嫁金钗"}, "currHeatDesc": "\u{e123}\u{e456}"}
        ]}});
        let items = parse_chart(MaoyanChart::Tv, &obfuscated, 10).unwrap();
        assert_eq!(items[0].heat.as_deref(), Some("8523.60"));
        assert_eq!(items[1].heat, None);
    }

    #[test]
    fn urls_follow_the_moviepilot_parameters() {
        assert_eq!(
            chart_url(MaoyanChart::WebSeries, MaoyanPlatform::Tencent, "20261105"),
            "https://piaofang.maoyan.com/i/api/encrypt/dashboard/webHeatData?seriesType=1&showDate=20261105&platformType=3&limit=30"
        );
        assert_eq!(
            chart_url(MaoyanChart::WebMovie, MaoyanPlatform::Tencent, "20261105"),
            "https://piaofang.maoyan.com/i/api/encrypt/dashboard/webHeatNetData?showDate=20261101&platformType=3&dateType=2&rankType=0"
        );
        assert!(
            latest_available_date(
                &json!({"status": false, "calendarNet": {"selectMaxDate": "2999-11-20"}})
            )
            .is_none()
        );
        assert_eq!(
            chart_url(MaoyanChart::Variety, MaoyanPlatform::All, "20261105"),
            "https://piaofang.maoyan.com/i/api/encrypt/dashboard/webHeatData?seriesType=2&showDate=20261105&limit=30"
        );
    }

    #[test]
    fn parses_box_office_pages() {
        let maoyan = r#"<div><ul class="row"><li class="col0">排名</li><li class="col1">片名</li></ul>
            <ul class="row" data-com="hrefTo,href:'/movie/1294273'">
              <li class="col0">1</li>
              <li class="col1"><p class="first-line">哪吒之魔童闹海</p><p class="second-line">2025-01-29 上映</p></li>
              <li class="col2 tr">1544614</li><li class="col3 tr">47.6</li><li class="col4 tr">24</li>
            </ul></div>"#;
        let items = parse_maoyan_rankings(maoyan, 10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "哪吒之魔童闹海");
        assert_eq!(items[0].release_info.as_deref(), Some("2025-01-29 上映"));
        assert_eq!(items[0].total_box_office.as_deref(), Some("154.46亿"));

        let mojo = r#"<table><tr><th>Rank</th><th>Release Group</th></tr>
            <tr><td>1</td><td><a href="/releasegroup/x">Spider-Man: Brand New Day</a></td><td>$2,521,214,671</td><td>$958,944,095</td><td>38.0%</td></tr></table>"#;
        let items = parse_box_office_mojo(mojo, 10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Spider-Man: Brand New Day");
        assert_eq!(items[0].total_box_office.as_deref(), Some("25.21亿美元"));
        assert_eq!(items[0].box_office.as_deref(), Some("9.59亿美元"));
        assert_eq!(
            chart_url(MaoyanChart::Year, MaoyanPlatform::All, "20261105"),
            "https://piaofang.maoyan.com/rankings/year?year=2026"
        );
    }

    fn tmdb(id: i64, title: &str, year: Option<u32>) -> TmdbMedia {
        TmdbMedia {
            tmdb_id: id,
            media_type: TmdbMediaType::Tv,
            title: title.to_string(),
            original_title: None,
            year,
            overview: String::new(),
            poster_path: None,
            is_animation: false,
            genres: Vec::new(),
        }
    }

    fn douban(
        id: &str,
        title: &str,
        year: Option<u32>,
        media_type: TmdbMediaType,
    ) -> DoubanSubject {
        DoubanSubject {
            id: id.to_string(),
            title: title.to_string(),
            year,
            rating: None,
            poster_id: None,
            url: String::new(),
            media_type,
        }
    }

    #[test]
    fn douban_and_tmdb_candidates_are_matched_by_title_and_year() {
        let subjects = vec![
            douban("1", "庆余年 第一季", Some(2019), TmdbMediaType::Tv),
            douban("2", "庆余年 第二季", Some(2024), TmdbMediaType::Tv),
            douban("3", "庆余年第二季", Some(2024), TmdbMediaType::Movie),
        ];
        assert_eq!(
            pick_douban("庆余年 第二季", &TmdbMediaType::Tv, subjects.clone())
                .unwrap()
                .id,
            "2"
        );
        assert!(pick_douban("庆余年之外", &TmdbMediaType::Tv, subjects).is_none());
        let seasons = vec![
            douban("7", "向往的生活 第一季", Some(2017), TmdbMediaType::Tv),
            douban("8", "向往的生活 第八季", Some(2024), TmdbMediaType::Tv),
        ];
        assert_eq!(
            pick_douban("向往的生活", &TmdbMediaType::Tv, seasons)
                .unwrap()
                .id,
            "8"
        );

        let names = vec!["小城良方".to_string()];
        let results = vec![
            tmdb(1, "小城", Some(2026)),
            tmdb(2, "小城良方", Some(2020)),
            tmdb(3, "小城良方", Some(2026)),
        ];
        assert_eq!(
            pick_tmdb_for(&names, Some(2026), results).unwrap().tmdb_id,
            3
        );
        // 没有同名条目时接受年份相符的结果（TMDB 已按年份过滤）；都不符合则不匹配。
        assert_eq!(
            pick_tmdb_for(&names, Some(2026), vec![tmdb(9, "Remedy", Some(2026))])
                .unwrap()
                .tmdb_id,
            9
        );
        assert!(pick_tmdb_for(&names, Some(2026), vec![tmdb(9, "Remedy", Some(2010))]).is_none());
    }

    #[test]
    fn season_suffix_is_removed_for_tmdb_lookup() {
        assert_eq!(tmdb_query_title("庆余年 第二季"), "庆余年");
        assert_eq!(tmdb_query_title("第一炉香"), "第一炉香");
        assert_eq!(
            latest_available_date(
                &json!({"status": false, "calendarNet": {"selectMaxDate": "2020-11-04"}})
            ),
            Some("20201104".to_string())
        );
    }

    /// 用真实云浏览器抓取热度榜原始数据，便于排查格式变化：
    /// `MAOYAN_LIGHTPANDA_SECRETS=path MAOYAN_DUMP_DIR=dir cargo test --lib maoyan::tests::live_dump -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn live_dump() {
        let secrets: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("MAOYAN_LIGHTPANDA_SECRETS").unwrap()).unwrap(),
        )
        .unwrap();
        let mut settings = crate::config::GlobalConfig::default();
        settings.lightpanda.endpoint = secrets["endpoint"].as_str().map(str::to_string);
        settings.lightpanda.token = secrets["token"].as_str().map(str::to_string);
        let dir = std::path::PathBuf::from(std::env::var("MAOYAN_DUMP_DIR").unwrap());
        let date = show_date();
        let keys = [
            (MaoyanChart::Tv, MaoyanPlatform::All),
            (MaoyanChart::WebMovie, MaoyanPlatform::All),
            (MaoyanChart::WebMovie, MaoyanPlatform::Tencent),
        ];
        let mut urls: Vec<String> = keys
            .iter()
            .map(|(chart, platform)| chart_url(*chart, *platform, &date))
            .collect();
        let mut names: Vec<String> = keys
            .iter()
            .map(|(chart, platform)| format!("{}-{}.json", chart.as_str(), platform.as_str()))
            .collect();
        if let Ok(extra) = std::env::var("MAOYAN_EXTRA_URLS") {
            for (index, url) in extra.split_whitespace().enumerate() {
                urls.push(url.to_string());
                names.push(format!("extra-{index}.json"));
            }
        }
        let results = fetch_chart_payloads_in_browser(&settings, &urls)
            .await
            .unwrap();
        for (name, result) in names.into_iter().zip(results) {
            let body = match result {
                Ok(value) => serde_json::to_string_pretty(&value).unwrap(),
                Err(error) => format!("ERROR {error}"),
            };
            std::fs::write(dir.join(name), body).unwrap();
        }
    }
}
