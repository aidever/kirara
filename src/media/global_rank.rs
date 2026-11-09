//! 全球榜单：在猫眼之外，复刻 MoviePilot 插件 GlobalRankPro 的其余数据源。
//!
//! - Netflix：官方 Tudum Top 10 静态 TSV（全球榜四类目 + 国家/地区榜），每周二更新；
//!   条目只有英文片名，抓取后按 TMDB 匹配中文片名与海报，可直接订阅。
//! - 红果短剧：官网 SSR 榜单页内联的 JSON（`data-fn-args`），四个热播榜；
//!   短剧基本不在 TMDB / 豆瓣里，不做媒体匹配，仅展示。
//! - 音乐：网易云 / QQ 音乐的站内榜单接口与 Apple Music 官方 RSS Feed，仅展示。
//!
//! 每个数据源独立刷新、独立保存快照（数据库一源一行），刷新失败保留上一次的数据。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::tmdb::{TmdbMedia, TmdbMediaType};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub const NETFLIX_GLOBAL_TSV: &str = "https://www.netflix.com/tudum/top10/data/all-weeks-global.tsv";
pub const NETFLIX_COUNTRIES_TSV: &str = "https://www.netflix.com/tudum/top10/data/all-weeks-countries.tsv";
const HONGGUO_SITE: &str = "https://hongguoduanju.com";
/// 音乐榜每榜保留的条数（网易云热歌榜有 200 条，全存没有意义）。
pub const MUSIC_LIMIT: usize = 50;
/// TMDB 匹配缓存时长：Netflix 榜单每周才更新，同名条目跨周复用。
const MATCH_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankSource {
    Netflix,
    Hongguo,
    Music,
}

impl RankSource {
    pub const ALL: [Self; 3] = [Self::Netflix, Self::Hongguo, Self::Music];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Netflix => "netflix",
            Self::Hongguo => "hongguo",
            Self::Music => "music",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "netflix" => Some(Self::Netflix),
            "hongguo" => Some(Self::Hongguo),
            "music" => Some(Self::Music),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Netflix => "Netflix",
            Self::Hongguo => "红果短剧",
            Self::Music => "音乐榜",
        }
    }

    /// 刷新周期：Netflix 每周发榜，半天检查一次足够；短剧与音乐榜每天多次变化。
    pub fn refresh_interval(self) -> Duration {
        match self {
            Self::Netflix => Duration::from_secs(12 * 60 * 60),
            Self::Hongguo | Self::Music => Duration::from_secs(3 * 60 * 60),
        }
    }
}

/// 统一的榜单条目：三个数据源字段取并集，与源无关的字段留空。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RankEntry {
    pub rank: usize,
    pub title: String,
    /// 原始片名：Netflix 条目匹配 TMDB 后标题换成中文，英文原名保留在这里
    #[serde(default)]
    pub original_title: Option<String>,
    /// 副标题：Netflix 的季名、音乐的歌手
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub poster: Option<String>,
    /// 官方页面（短剧详情、歌曲页）
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    /// 指标文本，例如「观看 470 万次」「11578万热度」「3:48」
    #[serde(default)]
    pub metrics: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 名次变化：正数为上升，0 为持平；`new_entry` 表示新上榜
    #[serde(default)]
    pub rank_change: Option<i64>,
    #[serde(default)]
    pub new_entry: bool,
    /// 可订阅的影视类型（仅 Netflix）
    #[serde(default)]
    pub media_type: Option<TmdbMediaType>,
    #[serde(default)]
    pub tmdb: Option<TmdbMedia>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankBoard {
    /// 源内唯一键，例如 `global:films_en`、`hk:tv`、`hot`、`netease:hot`
    pub key: String,
    pub name: String,
    /// 分组：Netflix 为地区代码，音乐为平台代码，红果为空
    pub group: String,
    pub group_name: String,
    /// `movie` / `tv`（仅 Netflix）
    #[serde(default)]
    pub kind: Option<String>,
    /// 数据周期，例如 Netflix 的周起始日、音乐榜的更新日期
    #[serde(default)]
    pub period: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    pub items: Vec<RankEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankSnapshot {
    pub source: RankSource,
    pub fetched_at: String,
    pub boards: Vec<RankBoard>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RankSourceView {
    pub source: RankSource,
    pub fetched_at: Option<String>,
    pub boards: Vec<RankBoard>,
    pub refreshing: bool,
    /// 最近一次刷新失败的原因（有历史数据时仅作提示）
    pub error: Option<String>,
}

#[derive(Default)]
pub struct GlobalRankStore {
    loaded: tokio::sync::OnceCell<()>,
    snapshots: Mutex<HashMap<RankSource, RankSnapshot>>,
    errors: Mutex<HashMap<RankSource, String>>,
    refreshing: [AtomicBool; 3],
    last_attempt: Mutex<HashMap<RankSource, Instant>>,
    matches: Mutex<HashMap<(String, &'static str), (Instant, Option<TmdbMedia>)>>,
}

pub struct RefreshGuard<'a>(&'a AtomicBool);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn slot(source: RankSource) -> usize {
    match source {
        RankSource::Netflix => 0,
        RankSource::Hongguo => 1,
        RankSource::Music => 2,
    }
}

impl GlobalRankStore {
    pub async fn load_once<F, Fut>(&self, load: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Vec<RankSnapshot>>,
    {
        self.loaded
            .get_or_init(|| async {
                let snapshots = load().await;
                let mut entries = self.snapshots.lock().await;
                for snapshot in snapshots {
                    entries.entry(snapshot.source).or_insert(snapshot);
                }
            })
            .await;
    }

    pub fn is_refreshing(&self, source: RankSource) -> bool {
        self.refreshing[slot(source)].load(Ordering::SeqCst)
    }

    pub fn try_begin_refresh(&self, source: RankSource) -> Option<RefreshGuard<'_>> {
        let flag = &self.refreshing[slot(source)];
        (!flag.swap(true, Ordering::SeqCst)).then(|| RefreshGuard(flag))
    }

    pub async fn mark_attempt(&self, source: RankSource) {
        self.last_attempt.lock().await.insert(source, Instant::now());
    }

    pub async fn is_stale(&self, source: RankSource) -> bool {
        if let Some(attempt) = self.last_attempt.lock().await.get(&source) {
            return attempt.elapsed() >= source.refresh_interval();
        }
        let snapshots = self.snapshots.lock().await;
        match snapshots
            .get(&source)
            .and_then(|snapshot| chrono::DateTime::parse_from_rfc3339(&snapshot.fetched_at).ok())
        {
            Some(fetched) => (Utc::now() - fetched.with_timezone(&Utc))
                .to_std()
                .map_or(false, |age| age >= source.refresh_interval()),
            None => true,
        }
    }

    pub async fn snapshot(&self, source: RankSource) -> Option<RankSnapshot> {
        self.snapshots.lock().await.get(&source).cloned()
    }

    pub async fn view(&self, source: RankSource) -> RankSourceView {
        let snapshot = self.snapshot(source).await;
        let error = self.errors.lock().await.get(&source).cloned();
        RankSourceView {
            source,
            fetched_at: snapshot.as_ref().map(|snapshot| snapshot.fetched_at.clone()),
            boards: snapshot.map(|snapshot| snapshot.boards).unwrap_or_default(),
            refreshing: self.is_refreshing(source),
            error,
        }
    }

    pub async fn put(&self, snapshot: RankSnapshot) {
        self.errors.lock().await.remove(&snapshot.source);
        self.snapshots.lock().await.insert(snapshot.source, snapshot);
    }

    pub async fn set_error(&self, source: RankSource, error: String) {
        self.errors.lock().await.insert(source, error);
    }

    pub async fn cached_match(&self, title: &str, media_type: &TmdbMediaType) -> Option<Option<TmdbMedia>> {
        let cache = self.matches.lock().await;
        cache
            .get(&(title.to_string(), media_type.as_str()))
            .filter(|(stored, _)| stored.elapsed() < MATCH_CACHE_TTL)
            .map(|(_, found)| found.clone())
    }

    pub async fn store_match(&self, title: String, media_type: &TmdbMediaType, found: Option<TmdbMedia>) {
        let mut cache = self.matches.lock().await;
        cache.retain(|_, (stored, _)| stored.elapsed() < MATCH_CACHE_TTL);
        cache.insert((title, media_type.as_str()), (Instant::now(), found));
    }

    /// 上一次快照里已匹配到的 TMDB 条目，按（英文片名, 类型）索引；重启后不必重新搜索。
    pub async fn previous_matches(&self, source: RankSource) -> HashMap<(String, &'static str), TmdbMedia> {
        let snapshots = self.snapshots.lock().await;
        let mut found = HashMap::new();
        if let Some(snapshot) = snapshots.get(&source) {
            for board in &snapshot.boards {
                for item in &board.items {
                    if let (Some(media), Some(media_type)) = (&item.tmdb, &item.media_type) {
                        let original = item.original_title.as_deref().unwrap_or(&item.title).to_string();
                        found.entry((original, media_type.as_str())).or_insert_with(|| media.clone());
                    }
                }
            }
        }
        found
    }
}

/// 带重试的 GET：海外源（Netflix、Apple）在部分网络下偶发中途断流，重试两次。
async fn get_bytes(http: &reqwest::Client, url: &str, referer: Option<&str>, timeout: Duration) -> Result<bytes::Bytes, String> {
    let mut last_error = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
        }
        match get_bytes_once(http, url, referer, timeout).await {
            Ok(bytes) => return Ok(bytes),
            // 4xx 不会因为重试而变好。
            Err((error, false)) => return Err(error),
            Err((error, true)) => last_error = error,
        }
    }
    Err(last_error)
}

async fn get_bytes_once(
    http: &reqwest::Client,
    url: &str,
    referer: Option<&str>,
    timeout: Duration,
) -> Result<bytes::Bytes, (String, bool)> {
    let mut request = http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
        .timeout(timeout);
    if let Some(referer) = referer {
        request = request.header(reqwest::header::REFERER, referer);
    }
    let response = request.send().await.map_err(|error| (format!("请求失败：{error}"), true))?;
    let status = response.status();
    if !status.is_success() {
        return Err((format!("HTTP {}", status.as_u16()), status.is_server_error()));
    }
    response.bytes().await.map_err(|error| (format!("读取响应失败：{error}"), true))
}

// ---------------------------------------------------------------------------
// Netflix
// ---------------------------------------------------------------------------

/// 地区代码与中文名；顺序即前端展示顺序。Netflix 未进入中国大陆。
pub const NETFLIX_REGIONS: [(&str, &str); 7] = [
    ("global", "全球"),
    ("us", "美国"),
    ("hk", "中国香港"),
    ("tw", "中国台湾"),
    ("jp", "日本"),
    ("kr", "韩国"),
    ("sg", "新加坡"),
];

#[derive(Debug, Clone, PartialEq)]
pub struct NetflixRow {
    pub week: String,
    pub category: String,
    pub rank: usize,
    pub title: String,
    pub season_title: Option<String>,
    pub hours_viewed: u64,
    pub views: u64,
    pub runtime_hours: f64,
    pub weeks_in_top10: u32,
}

fn tsv_index(header: &[&str], name: &str) -> Option<usize> {
    header.iter().position(|column| column.trim() == name)
}

fn parse_u64(value: Option<&&str>) -> u64 {
    value
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .map_or(0, |value| value as u64)
}

fn netflix_row(fields: &[&str], columns: &NetflixColumns) -> Option<NetflixRow> {
    let get = |index: Option<usize>| index.and_then(|index| fields.get(index)).map(|value| value.trim());
    let title = get(columns.title)?.to_string();
    if title.is_empty() {
        return None;
    }
    let season_title = get(columns.season)
        .filter(|value| !value.is_empty() && *value != "N/A")
        .map(str::to_string);
    Some(NetflixRow {
        week: get(columns.week)?.to_string(),
        category: get(columns.category)?.to_string(),
        rank: get(columns.rank)?.parse().ok()?,
        title,
        season_title,
        hours_viewed: parse_u64(columns.hours.and_then(|index| fields.get(index))),
        views: parse_u64(columns.views.and_then(|index| fields.get(index))),
        runtime_hours: get(columns.runtime).and_then(|value| value.parse().ok()).unwrap_or(0.0),
        weeks_in_top10: get(columns.weeks).and_then(|value| value.parse().ok()).unwrap_or(0),
    })
}

struct NetflixColumns {
    week: Option<usize>,
    category: Option<usize>,
    rank: Option<usize>,
    title: Option<usize>,
    season: Option<usize>,
    hours: Option<usize>,
    views: Option<usize>,
    runtime: Option<usize>,
    weeks: Option<usize>,
    country: Option<usize>,
}

impl NetflixColumns {
    fn new(header: &str) -> Self {
        let header: Vec<&str> = header.trim_end_matches(['\r', '\n']).split('\t').collect();
        Self {
            week: tsv_index(&header, "week"),
            category: tsv_index(&header, "category"),
            rank: tsv_index(&header, "weekly_rank"),
            title: tsv_index(&header, "show_title"),
            season: tsv_index(&header, "season_title"),
            hours: tsv_index(&header, "weekly_hours_viewed"),
            views: tsv_index(&header, "weekly_views"),
            runtime: tsv_index(&header, "runtime"),
            weeks: tsv_index(&header, "cumulative_weeks_in_top_10"),
            country: tsv_index(&header, "country_iso2"),
        }
    }
}

/// 解析全球榜 TSV，只保留最新一周。
pub fn parse_netflix_global(text: &str) -> (String, Vec<NetflixRow>) {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return (String::new(), Vec::new());
    };
    let columns = NetflixColumns::new(header);
    let rows: Vec<NetflixRow> = lines
        .filter_map(|line| netflix_row(&line.split('\t').collect::<Vec<_>>(), &columns))
        .collect();
    let latest = rows.iter().map(|row| row.week.clone()).max().unwrap_or_default();
    let rows = rows.into_iter().filter(|row| row.week == latest).collect();
    (latest, rows)
}

/// 国家/地区榜 TSV 的增量解析器：文件约 30MB，按块读入、逐行处理，只保留所需地区最新一周。
pub struct NetflixCountryParser {
    regions: Vec<String>,
    columns: Option<NetflixColumns>,
    pending: Vec<u8>,
    rows: HashMap<String, (String, Vec<NetflixRow>)>,
}

impl NetflixCountryParser {
    pub fn new(regions: &[&str]) -> Self {
        Self {
            regions: regions.iter().map(|region| region.to_ascii_uppercase()).collect(),
            columns: None,
            pending: Vec::new(),
            rows: HashMap::new(),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        let Some(last_newline) = self.pending.iter().rposition(|byte| *byte == b'\n') else {
            return;
        };
        let complete: Vec<u8> = self.pending.drain(..=last_newline).collect();
        for line in complete.split(|byte| *byte == b'\n') {
            self.line(&String::from_utf8_lossy(line));
        }
    }

    fn line(&mut self, line: &str) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            return;
        }
        let Some(columns) = &self.columns else {
            self.columns = Some(NetflixColumns::new(line));
            return;
        };
        let fields: Vec<&str> = line.split('\t').collect();
        let Some(country) = columns.country.and_then(|index| fields.get(index)).map(|value| value.trim().to_ascii_uppercase()) else {
            return;
        };
        if !self.regions.contains(&country) {
            return;
        }
        let Some(row) = netflix_row(&fields, columns) else {
            return;
        };
        let entry = self.rows.entry(country).or_insert_with(|| (row.week.clone(), Vec::new()));
        if row.week > entry.0 {
            *entry = (row.week.clone(), Vec::new());
        }
        if row.week == entry.0 {
            entry.1.push(row);
        }
    }

    /// 结束解析，返回 地区代码（小写）→（最新周, 条目）。
    pub fn finish(mut self) -> HashMap<String, (String, Vec<NetflixRow>)> {
        let rest = std::mem::take(&mut self.pending);
        if !rest.is_empty() {
            self.line(&String::from_utf8_lossy(&rest));
        }
        self.rows
            .into_iter()
            .map(|(country, value)| (country.to_ascii_lowercase(), value))
            .collect()
    }
}

fn trim_decimal(text: String) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

fn format_wan(value: u64, unit: &str) -> String {
    if value >= 100_000_000 {
        format!("{}亿{unit}", trim_decimal(format!("{:.2}", value as f64 / 100_000_000.0)))
    } else {
        format!("{}万{unit}", trim_decimal(format!("{:.1}", value as f64 / 10_000.0)))
    }
}

fn format_runtime(hours: f64) -> Option<String> {
    if hours <= 0.0 {
        return None;
    }
    let minutes = (hours * 60.0).round() as u64;
    Some(match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes} 分钟"),
        (hours, 0) => format!("{hours} 小时"),
        (hours, minutes) => format!("{hours} 小时 {minutes} 分"),
    })
}

impl NetflixRow {
    pub fn media_type(&self) -> TmdbMediaType {
        if self.category.starts_with("TV") {
            TmdbMediaType::Tv
        } else {
            TmdbMediaType::Movie
        }
    }

    pub fn into_entry(self) -> RankEntry {
        let mut metrics = Vec::new();
        if self.views > 0 {
            metrics.push(format!("观看 {}", format_wan(self.views, "次")));
        }
        if self.hours_viewed > 0 {
            metrics.push(format_wan(self.hours_viewed, "小时"));
        }
        if let Some(runtime) = format_runtime(self.runtime_hours) {
            metrics.push(format!("片长 {runtime}"));
        }
        if self.weeks_in_top10 > 0 {
            metrics.push(format!("在榜 {} 周", self.weeks_in_top10));
        }
        let media_type = self.media_type();
        RankEntry {
            rank: self.rank,
            subtitle: self.season_title.clone(),
            metrics,
            original_title: Some(self.title.clone()),
            new_entry: self.weeks_in_top10 == 1,
            media_type: Some(media_type),
            title: self.title,
            ..RankEntry::default()
        }
    }
}

/// 全球榜四个类目：（TSV 类目, 键, 名称, 类型）。
const NETFLIX_GLOBAL_CATEGORIES: [(&str, &str, &str, &str); 4] = [
    ("Films (English)", "films_en", "电影（英语）", "movie"),
    ("Films (Non-English)", "films_other", "电影（非英语）", "movie"),
    ("TV (English)", "tv_en", "剧集（英语）", "tv"),
    ("TV (Non-English)", "tv_other", "剧集（非英语）", "tv"),
];

pub fn netflix_boards(
    global: Option<(String, Vec<NetflixRow>)>,
    countries: &HashMap<String, (String, Vec<NetflixRow>)>,
) -> Vec<RankBoard> {
    let mut boards = Vec::new();
    if let Some((week, rows)) = global {
        for (category, key, name, kind) in NETFLIX_GLOBAL_CATEGORIES {
            let mut items: Vec<NetflixRow> = rows.iter().filter(|row| row.category == category).cloned().collect();
            if items.is_empty() {
                continue;
            }
            items.sort_by_key(|row| row.rank);
            boards.push(RankBoard {
                key: format!("global:{key}"),
                name: format!("全球 · {name}"),
                group: "global".to_string(),
                group_name: "全球".to_string(),
                kind: Some(kind.to_string()),
                period: Some(week.clone()),
                url: Some("https://www.netflix.com/tudum/top10".to_string()),
                items: items.into_iter().map(NetflixRow::into_entry).collect(),
            });
        }
    }
    for (region, region_name) in NETFLIX_REGIONS.iter().skip(1) {
        let Some((week, rows)) = countries.get(*region) else {
            continue;
        };
        for (category, kind, name) in [("Films", "movie", "电影"), ("TV", "tv", "剧集")] {
            let mut items: Vec<NetflixRow> = rows.iter().filter(|row| row.category == category).cloned().collect();
            if items.is_empty() {
                continue;
            }
            items.sort_by_key(|row| row.rank);
            boards.push(RankBoard {
                key: format!("{region}:{kind}"),
                name: format!("{region_name} · {name}"),
                group: region.to_string(),
                group_name: region_name.to_string(),
                kind: Some(kind.to_string()),
                period: Some(week.clone()),
                url: Some(format!("https://www.netflix.com/tudum/top10/{}", region_name_slug(region))),
                items: items.into_iter().map(NetflixRow::into_entry).collect(),
            });
        }
    }
    boards
}

fn region_name_slug(region: &str) -> &'static str {
    match region {
        "us" => "united-states",
        "hk" => "hong-kong",
        "tw" => "taiwan",
        "jp" => "japan",
        "kr" => "south-korea",
        "sg" => "singapore",
        _ => "",
    }
}

pub async fn fetch_netflix_global(http: &reqwest::Client) -> Result<(String, Vec<NetflixRow>), String> {
    let bytes = get_bytes(http, NETFLIX_GLOBAL_TSV, None, Duration::from_secs(120)).await?;
    let parsed = parse_netflix_global(&String::from_utf8_lossy(&bytes));
    if parsed.1.is_empty() {
        return Err("Netflix 全球榜数据为空或格式变化".to_string());
    }
    Ok(parsed)
}

pub async fn fetch_netflix_countries(
    http: &reqwest::Client,
) -> Result<HashMap<String, (String, Vec<NetflixRow>)>, String> {
    let mut last_error = String::new();
    for attempt in 0..2u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        match fetch_netflix_countries_once(http).await {
            Ok(rows) => return Ok(rows),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

async fn fetch_netflix_countries_once(
    http: &reqwest::Client,
) -> Result<HashMap<String, (String, Vec<NetflixRow>)>, String> {
    let regions: Vec<&str> = NETFLIX_REGIONS.iter().skip(1).map(|(code, _)| *code).collect();
    let mut response = http
        .get(NETFLIX_COUNTRIES_TSV)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|error| format!("请求失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    let mut parser = NetflixCountryParser::new(&regions);
    while let Some(chunk) = response.chunk().await.map_err(|error| format!("读取响应失败：{error}"))? {
        parser.feed(&chunk);
    }
    let rows = parser.finish();
    if rows.is_empty() {
        return Err("Netflix 地区榜数据为空或格式变化".to_string());
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// 红果短剧
// ---------------------------------------------------------------------------

/// （键, 路由, 名称）
pub const HONGGUO_BOARDS: [(&str, &str, &str); 4] = [
    ("hot", "hot-drama", "热播榜"),
    ("real", "hot-real-drama", "真人剧热播榜"),
    ("comic", "hot-comic-drama", "漫剧热播榜"),
    ("ai", "hot-ai-drama", "AI 剧热播榜"),
];

/// 在内联参数里递归查找含 `rankList` 的对象；官网会随机返回两种 SSR 结构，
/// 其中一种把数据塞在「JSON 字符串里的 JSON」中，所以字符串也要尝试再解析一层。
fn find_rank_payload(value: &Value, depth: usize) -> Option<Value> {
    if depth > 8 {
        return None;
    }
    match value {
        Value::Object(map) => {
            if map.get("rankList").is_some_and(Value::is_array) {
                return Some(value.clone());
            }
            map.values().find_map(|value| find_rank_payload(value, depth + 1))
        }
        Value::Array(values) => values.iter().find_map(|value| find_rank_payload(value, depth + 1)),
        Value::String(text) => {
            let text = text.trim();
            if text.starts_with('{') || text.starts_with('[') {
                serde_json::from_str::<Value>(text)
                    .ok()
                    .and_then(|value| find_rank_payload(&value, depth + 1))
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn extract_hongguo_payload(html: &str) -> Option<Value> {
    let document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("script[data-fn-args]").expect("valid selector");
    document.select(&selector).find_map(|script| {
        let raw = script.value().attr("data-fn-args")?;
        if !raw.contains("rankList") {
            return None;
        }
        let args: Value = serde_json::from_str(raw).ok()?;
        find_rank_payload(&args, 0)
    })
}

fn json_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn json_strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(|value| json_text(Some(value))).collect())
        .unwrap_or_default()
}

/// 海报缩略后缀从 640 提到 1080（源图上限 896×1280）。
fn upgrade_cover(url: &str) -> String {
    match url.rfind("~tplv-") {
        Some(index) => {
            let (base, suffix) = url.split_at(index);
            let extension = suffix.rfind('.').map(|dot| &suffix[dot..]).unwrap_or(".image");
            let height = suffix
                .split(':')
                .nth(2)
                .map(|rest| rest.split('.').next().unwrap_or("0"))
                .unwrap_or("0");
            format!("{base}~tplv-shrink:1080:{height}{extension}")
        }
        None => url.to_string(),
    }
}

pub fn parse_hongguo_items(payload: &Value) -> Vec<RankEntry> {
    let Some(rows) = payload.get("rankList").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let title = json_text(row.get("title"))?;
            let series_id = json_text(row.get("seriesId")).or_else(|| json_text(row.get("id")));
            let mut tags = json_strings(row.get("statusTags"));
            tags.extend(json_strings(row.get("tags")));
            let metrics = ["heatText", "scoreText", "favoriteText", "likeText"]
                .into_iter()
                .filter_map(|key| json_text(row.get(key)))
                .collect();
            let episodes = row.get("episodeCount").and_then(Value::as_u64).filter(|count| *count > 0);
            Some(RankEntry {
                rank: row
                    .get("rank")
                    .and_then(Value::as_u64)
                    .map_or(index + 1, |rank| rank as usize),
                title,
                subtitle: episodes.map(|count| format!("共 {count} 集")),
                poster: json_text(row.get("cover")).map(|cover| upgrade_cover(&cover)),
                url: series_id.map(|id| format!("{HONGGUO_SITE}/detail?series_id={id}")),
                overview: json_text(row.get("description")),
                metrics,
                tags,
                new_entry: json_strings(row.get("statusTags")).iter().any(|tag| tag == "新剧"),
                ..RankEntry::default()
            })
        })
        .collect()
}

pub async fn fetch_hongguo_board(http: &reqwest::Client, route: &str) -> Result<Vec<RankEntry>, String> {
    let url = format!("{HONGGUO_SITE}/rank/{route}");
    // 官网偶发返回空榜单，重试一次。
    let mut last_error = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        let bytes = match get_bytes(http, &url, None, Duration::from_secs(30)).await {
            Ok(bytes) => bytes,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let html = String::from_utf8_lossy(&bytes);
        match extract_hongguo_payload(&html) {
            Some(payload) => {
                let items = parse_hongguo_items(&payload);
                if !items.is_empty() {
                    return Ok(items);
                }
                last_error = "官网返回的榜单为空".to_string();
            }
            None => last_error = "页面中没有找到榜单数据，官网结构可能已变化".to_string(),
        }
    }
    Err(last_error)
}

// ---------------------------------------------------------------------------
// 音乐
// ---------------------------------------------------------------------------

/// （平台, 平台名, 榜单键, 榜单名, 平台原生 id）
pub const MUSIC_BOARDS: [(&str, &str, &str, &str, &str); 9] = [
    ("netease", "网易云音乐", "hot", "热歌榜", "3778678"),
    ("netease", "网易云音乐", "new", "新歌榜", "3779629"),
    ("netease", "网易云音乐", "soaring", "飙升榜", "19723756"),
    ("netease", "网易云音乐", "original", "原创榜", "2884035"),
    ("qq", "QQ 音乐", "hot", "热歌榜", "26"),
    ("qq", "QQ 音乐", "new", "新歌榜", "27"),
    ("qq", "QQ 音乐", "soaring", "飙升榜", "62"),
    ("qq", "QQ 音乐", "index", "流行指数榜", "4"),
    ("apple", "Apple Music", "most-played", "热门歌曲", ""),
];

fn https(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

fn format_duration(seconds: u64) -> Option<String> {
    (seconds > 0).then(|| format!("{}:{:02}", seconds / 60, seconds % 60))
}

async fn get_json(http: &reqwest::Client, url: &str, referer: Option<&str>) -> Result<Value, String> {
    let bytes = get_bytes(http, url, referer, Duration::from_secs(30)).await?;
    // QQ 音乐的响应开头带空格。
    let text = String::from_utf8_lossy(&bytes);
    serde_json::from_str(text.trim()).map_err(|error| format!("JSON 解析失败：{error}"))
}

pub fn parse_netease(payload: &Value, limit: usize) -> Result<(Option<String>, Vec<RankEntry>), String> {
    let result = payload.get("result").ok_or("网易云返回结构异常")?;
    let tracks = result
        .get("tracks")
        .and_then(Value::as_array)
        .filter(|tracks| !tracks.is_empty())
        .ok_or("网易云返回空曲目列表")?;
    let items = tracks
        .iter()
        .take(limit)
        .enumerate()
        .filter_map(|(index, track)| {
            let title = json_text(track.get("name"))?;
            let rank = index + 1;
            let artists: Vec<String> = track
                .get("artists")
                .and_then(Value::as_array)
                .map(|artists| artists.iter().filter_map(|artist| json_text(artist.get("name"))).collect())
                .unwrap_or_default();
            let album = track.get("album");
            let poster = album
                .and_then(|album| json_text(album.get("picUrl")))
                .map(|url| format!("{}?param=300y300", https(&url)));
            // lastRank 是上期名次（从 0 开始），缺失表示新上榜。
            let previous = track.get("lastRank").and_then(Value::as_i64);
            let mut metrics = Vec::new();
            if let Some(album) = album.and_then(|album| json_text(album.get("name"))) {
                metrics.push(album);
            }
            if let Some(duration) = format_duration(track.get("duration").and_then(Value::as_u64).unwrap_or(0) / 1000) {
                metrics.push(duration);
            }
            Some(RankEntry {
                rank,
                title,
                subtitle: Some(artists.join(" / ")).filter(|value| !value.is_empty()),
                poster,
                url: json_text(track.get("id")).map(|id| format!("https://music.163.com/#/song?id={id}")),
                metrics,
                rank_change: previous.map(|previous| previous + 1 - rank as i64),
                new_entry: previous.is_none(),
                ..RankEntry::default()
            })
        })
        .collect();
    let period = result
        .get("updateTime")
        .and_then(Value::as_i64)
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| {
            time.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).expect("valid offset"))
                .format("%Y-%m-%d")
                .to_string()
        });
    Ok((period, items))
}

pub fn parse_qq(payload: &Value, limit: usize) -> Result<(Option<String>, Vec<RankEntry>), String> {
    let songs = payload
        .get("songlist")
        .and_then(Value::as_array)
        .filter(|songs| !songs.is_empty())
        .ok_or("QQ 音乐返回空曲目列表")?;
    let items = songs
        .iter()
        .take(limit)
        .enumerate()
        .filter_map(|(index, row)| {
            let data = row.get("data")?;
            let title = json_text(data.get("songname"))?;
            let singers: Vec<String> = data
                .get("singer")
                .and_then(Value::as_array)
                .map(|singers| singers.iter().filter_map(|singer| json_text(singer.get("name"))).collect())
                .unwrap_or_default();
            let mut metrics = Vec::new();
            if let Some(album) = json_text(data.get("albumname")) {
                metrics.push(album);
            }
            if let Some(duration) = format_duration(data.get("interval").and_then(Value::as_u64).unwrap_or(0)) {
                metrics.push(duration);
            }
            Some(RankEntry {
                rank: index + 1,
                title,
                subtitle: Some(singers.join(" / ")).filter(|value| !value.is_empty()),
                poster: json_text(data.get("albummid"))
                    .map(|mid| format!("https://y.qq.com/music/photo_new/T002R300x300M000{mid}.jpg")),
                url: json_text(data.get("songmid"))
                    .map(|mid| format!("https://y.qq.com/n/ryqq/songDetail/{mid}")),
                metrics,
                ..RankEntry::default()
            })
        })
        .collect();
    Ok((json_text(payload.get("update_time")).or_else(|| json_text(payload.get("date"))), items))
}

pub fn parse_apple(payload: &Value, limit: usize) -> Result<(Option<String>, Vec<RankEntry>), String> {
    let feed = payload.get("feed").ok_or("Apple Music 返回结构异常")?;
    let results = feed
        .get("results")
        .and_then(Value::as_array)
        .filter(|results| !results.is_empty())
        .ok_or("Apple Music 返回空曲目列表")?;
    let items = results
        .iter()
        .take(limit)
        .enumerate()
        .filter_map(|(index, row)| {
            Some(RankEntry {
                rank: index + 1,
                title: json_text(row.get("name"))?,
                subtitle: json_text(row.get("artistName")),
                poster: json_text(row.get("artworkUrl100")).map(|url| url.replace("100x100bb", "300x300bb")),
                url: json_text(row.get("url")),
                tags: row
                    .get("genres")
                    .and_then(Value::as_array)
                    .map(|genres| genres.iter().filter_map(|genre| json_text(genre.get("name"))).take(2).collect())
                    .unwrap_or_default(),
                ..RankEntry::default()
            })
        })
        .collect();
    // Feed 的 updated 是 RFC 2822（如 "Mon, 9 Nov 2026 10:00:00 +0000"），旧版本是 RFC 3339。
    let period = json_text(feed.get("updated")).and_then(|value| {
        chrono::DateTime::parse_from_rfc2822(&value)
            .or_else(|_| chrono::DateTime::parse_from_rfc3339(&value))
            .ok()
            .map(|time| time.format("%Y-%m-%d").to_string())
    });
    Ok((period, items))
}

pub async fn fetch_music_board(
    http: &reqwest::Client,
    platform: &str,
    native_id: &str,
) -> Result<(Option<String>, Vec<RankEntry>, String), String> {
    match platform {
        "netease" => {
            let payload = get_json(
                http,
                &format!("https://music.163.com/api/playlist/detail?id={native_id}"),
                Some("https://music.163.com"),
            )
            .await?;
            let (period, items) = parse_netease(&payload, MUSIC_LIMIT)?;
            Ok((period, items, format!("https://music.163.com/#/discover/toplist?id={native_id}")))
        }
        "qq" => {
            let payload = get_json(
                http,
                &format!("https://c.y.qq.com/v8/fcg-bin/fcg_v8_toplist_cp.fcg?topid={native_id}&format=json&page=detail&tpl=3&type=top"),
                Some("https://y.qq.com"),
            )
            .await?;
            let (period, items) = parse_qq(&payload, MUSIC_LIMIT)?;
            Ok((period, items, format!("https://y.qq.com/n/ryqq/toplist/{native_id}")))
        }
        _ => {
            let payload = get_json(
                http,
                &format!("https://rss.applemarketingtools.com/api/v2/cn/music/most-played/{MUSIC_LIMIT}/songs.json"),
                None,
            )
            .await?;
            let (period, items) = parse_apple(&payload, MUSIC_LIMIT)?;
            Ok((period, items, "https://music.apple.com/cn/new/top-charts".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn netflix_global_keeps_latest_week_and_formats_metrics() {
        let tsv = "week\tcategory\tweekly_rank\tshow_title\tseason_title\tweekly_hours_viewed\truntime\tweekly_views\tcumulative_weeks_in_top_10\n\
2026-11-01\tFilms (English)\t2\tReturn to Sender\tN/A\t5700000\t1.5833\t3600000\t1\n\
2026-11-01\tFilms (English)\t1\tAnimals\tN/A\t8400000\t1.8\t4700000\t4\n\
2026-11-01\tTV (Non-English)\t1\tSquid Game\tSquid Game: Season 3\t120000000\t6.2\t19400000\t2\n\
2026-10-25\tFilms (English)\t1\tOld\tN/A\t1\t1\t1\t1\n";
        let (week, rows) = parse_netflix_global(tsv);
        assert_eq!(week, "2026-11-01");
        assert_eq!(rows.len(), 3);
        let boards = netflix_boards(Some((week, rows)), &HashMap::new());
        assert_eq!(boards.len(), 2);
        assert_eq!(boards[0].key, "global:films_en");
        assert_eq!(boards[0].items[0].title, "Animals");
        assert_eq!(boards[0].items[0].metrics, vec!["观看 470万次", "840万小时", "片长 1 小时 48 分", "在榜 4 周"]);
        assert!(boards[0].items[1].new_entry);
        assert_eq!(boards[1].kind.as_deref(), Some("tv"));
        assert_eq!(boards[1].items[0].subtitle.as_deref(), Some("Squid Game: Season 3"));
        assert_eq!(boards[1].items[0].metrics[1], "1.2亿小时");
        assert_eq!(boards[1].items[0].original_title.as_deref(), Some("Squid Game"));
    }

    #[test]
    fn netflix_country_parser_streams_and_keeps_latest_week_per_region() {
        let mut parser = NetflixCountryParser::new(&["hk", "us"]);
        let text = "country_name\tcountry_iso2\tweek\tcategory\tweekly_rank\tshow_title\tseason_title\tcumulative_weeks_in_top_10\n\
Argentina\tAR\t2026-11-01\tFilms\t1\tX\tN/A\t1\n\
Hong Kong\tHK\t2026-11-01\tFilms\t1\tSurviving Hurricane Otis\tN/A\t2\n\
Hong Kong\tHK\t2026-11-01\tTV\t1\tA Show\tA Show: Season 1\t3\n\
Hong Kong\tHK\t2026-10-25\tFilms\t1\tOlder\tN/A\t1\n";
        // 故意在行中间切块，验证跨块拼接。
        let (first, second) = text.as_bytes().split_at(97);
        parser.feed(first);
        parser.feed(second);
        let rows = parser.finish();
        assert!(!rows.contains_key("ar"));
        let (week, hk) = &rows["hk"];
        assert_eq!(week, "2026-11-01");
        assert_eq!(hk.len(), 2);
        let boards = netflix_boards(None, &rows);
        assert_eq!(boards.iter().map(|board| board.key.as_str()).collect::<Vec<_>>(), vec!["hk:movie", "hk:tv"]);
        assert_eq!(boards[1].items[0].metrics, vec!["在榜 3 周"]);
    }

    #[test]
    fn hongguo_payload_is_found_in_both_ssr_shapes() {
        let row = json!({"rank": 1, "title": "万妖图录传", "seriesId": "77", "cover": "https://p6-novel.byteimg.com/novel-pic/abc~tplv-shrink:640:0.image",
            "heatText": "11578万热度", "scoreText": "评分9.5", "favoriteText": "165.1万收藏", "likeText": "417.3万点赞",
            "tags": ["玄幻", "穿越"], "statusTags": ["新剧"], "description": "简介", "episodeCount": 202});
        let direct = json!(["rank_hot-drama/page", "content", {"isSuccess": true, "rankList": [row.clone()]}]);
        let attr = serde_json::to_string(&direct).unwrap().replace('"', "&quot;");
        let html = format!("<html><body><script data-fn-name=\"r\" data-fn-args='{attr}'></script></body></html>");
        let payload = extract_hongguo_payload(&html).expect("direct shape");
        let items = parse_hongguo_items(&payload);
        assert_eq!(items[0].title, "万妖图录传");
        assert_eq!(items[0].poster.as_deref(), Some("https://p6-novel.byteimg.com/novel-pic/abc~tplv-shrink:1080:0.image"));
        assert_eq!(items[0].url.as_deref(), Some("https://hongguoduanju.com/detail?series_id=77"));
        assert_eq!(items[0].metrics.len(), 4);
        assert_eq!(items[0].tags, vec!["新剧", "玄幻", "穿越"]);
        assert!(items[0].new_entry);
        assert_eq!(items[0].subtitle.as_deref(), Some("共 202 集"));

        let nested = json!(["rank_hot-drama/page", [{"key": "content", "routerDataFnArgs": serde_json::to_string(&json!({"isSuccess": true, "rankList": [row]})).unwrap()}]]);
        let attr = serde_json::to_string(&nested).unwrap().replace('&', "&amp;").replace('"', "&quot;");
        let html = format!("<script data-fn-name=\"mergeLoaderData\" data-fn-args=\"{attr}\"></script>");
        assert!(extract_hongguo_payload(&html).is_some());
    }

    #[test]
    fn music_parsers_map_rank_change_and_metrics() {
        let netease = json!({"result": {"name": "飙升榜", "updateTime": 1794184922479i64, "tracks": [
            {"id": 1, "name": "好坏", "artists": [{"name": "A"}, {"name": "B"}], "album": {"name": "专辑", "picUrl": "http://p2.music.126.net/x.jpg"}, "duration": 228239},
            {"id": 2, "name": "烂奇迹", "artists": [], "album": {}, "duration": 0, "lastRank": 0},
            {"id": 3, "name": "燃", "lastRank": 0}
        ]}});
        let (period, items) = parse_netease(&netease, 50).unwrap();
        assert!(period.is_some());
        assert!(items[0].new_entry);
        assert_eq!(items[0].subtitle.as_deref(), Some("A / B"));
        assert_eq!(items[0].poster.as_deref(), Some("https://p2.music.126.net/x.jpg?param=300y300"));
        assert_eq!(items[0].metrics, vec!["专辑", "3:48"]);
        assert_eq!(items[1].rank_change, Some(-1));
        assert_eq!(items[2].rank_change, Some(-2));

        let qq = json!({"update_time": "2026-11-09", "songlist": [{"data": {"songname": "奇迹", "albummid": "003EQ", "interval": 252, "singer": [{"name": "贺峻霖"}], "songmid": "abc"}}]});
        let (period, items) = parse_qq(&qq, 50).unwrap();
        assert_eq!(period.as_deref(), Some("2026-11-09"));
        assert_eq!(items[0].metrics, vec!["4:12"]);
        assert_eq!(items[0].poster.as_deref(), Some("https://y.qq.com/music/photo_new/T002R300x300M000003EQ.jpg"));

        let apple = json!({"feed": {"updated": "Mon, 9 Nov 2026 10:00:00 +0000", "results": [{"name": "Song", "artistName": "Singer", "artworkUrl100": "https://is1.mzstatic.com/a/100x100bb.jpg", "url": "https://music.apple.com/x", "genres": [{"name": "流行"}]}]}});
        let (period, items) = parse_apple(&apple, 50).unwrap();
        assert_eq!(period.as_deref(), Some("2026-11-09"));
        assert_eq!(items[0].poster.as_deref(), Some("https://is1.mzstatic.com/a/300x300bb.jpg"));
        assert_eq!(items[0].tags, vec!["流行"]);
    }
}
