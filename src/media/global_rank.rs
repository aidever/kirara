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

use super::douban::DoubanSubject;
use super::maoyan::{MaoyanChart, MaoyanPlatform, MaoyanRankResponse};
use super::tmdb::{TmdbMedia, TmdbMediaType};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub const NETFLIX_GLOBAL_TSV: &str =
    "https://www.netflix.com/tudum/top10/data/all-weeks-global.tsv";
pub const NETFLIX_COUNTRIES_TSV: &str =
    "https://www.netflix.com/tudum/top10/data/all-weeks-countries.tsv";
const HONGGUO_SITE: &str = "https://hongguoduanju.com";
/// 音乐榜每榜保留的条数（网易云热歌榜有 200 条，全存没有意义）。
pub const MUSIC_LIMIT: usize = 50;
/// TMDB 匹配缓存时长：Netflix 榜单每周才更新，同名条目跨周复用。
const MATCH_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankSource {
    Maoyan,
    Netflix,
    Hongguo,
    Music,
}

impl RankSource {
    pub const ALL: [Self; 4] = [Self::Maoyan, Self::Netflix, Self::Hongguo, Self::Music];
    /// 由本模块抓取并保存快照的数据源；猫眼有自己的抓取流程与存储。
    pub const FETCHED: [Self; 3] = [Self::Netflix, Self::Hongguo, Self::Music];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Maoyan => "maoyan",
            Self::Netflix => "netflix",
            Self::Hongguo => "hongguo",
            Self::Music => "music",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "maoyan" => Some(Self::Maoyan),
            "netflix" => Some(Self::Netflix),
            "hongguo" => Some(Self::Hongguo),
            "music" => Some(Self::Music),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Maoyan => "猫眼榜单",
            Self::Netflix => "Netflix 榜单",
            Self::Hongguo => "红果短剧榜单",
            Self::Music => "音乐榜单",
        }
    }

    /// 数据来源说明，展示在数据源总览里。
    pub fn origin(self) -> &'static str {
        match self {
            Self::Maoyan => "猫眼专业版 · Box Office Mojo",
            Self::Netflix => "Netflix Tudum Top 10",
            Self::Hongguo => "红果短剧官网",
            Self::Music => "网易云音乐 · QQ 音乐 · Apple Music",
        }
    }

    pub fn homepage(self) -> &'static str {
        match self {
            Self::Maoyan => "https://piaofang.maoyan.com/dashboard",
            Self::Netflix => "https://www.netflix.com/tudum/top10",
            Self::Hongguo => "https://hongguoduanju.com/rank/hot-drama",
            Self::Music => "https://music.163.com/#/discover/toplist",
        }
    }

    /// 刷新周期：猫眼热度约每小时更新；Netflix 每周发榜，半天检查一次足够；短剧与音乐榜每天多次变化。
    pub fn refresh_interval(self) -> Duration {
        match self {
            Self::Maoyan => super::maoyan::REFRESH_INTERVAL,
            Self::Netflix => Duration::from_secs(12 * 60 * 60),
            Self::Hongguo | Self::Music => Duration::from_secs(3 * 60 * 60),
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 统一的榜单条目：四个数据源的字段取并集，与来源无关的字段留空（不序列化）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RankItem {
    pub rank: usize,
    /// 展示标题：匹配到 TMDB 时为中文片名
    pub title: String,
    /// 来源给出的原始片名（Netflix 的英文名）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_title: Option<String>,
    /// 可订阅的影视类型（猫眼 / Netflix）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<TmdbMediaType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    /// 来源站点上的条目页面（短剧详情、歌曲页）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    /// 名次变化：正数为上升，0 为持平
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank_change: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub new_entry: bool,

    // ---- 评分与媒体身份 ----
    /// 豆瓣条目（含评分）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub douban: Option<DoubanSubject>,
    /// 最近一次查询豆瓣的时间；查过但没有结果时用来避免反复查询
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub douban_checked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<TmdbMedia>,

    // ---- 猫眼 ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_info: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub box_office: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_box_office: Option<String>,
    /// 热度：猫眼为热度值，红果为「11578万热度」这样的文本
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub box_rate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_rate: Option<String>,

    // ---- Netflix ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season_title: Option<String>,
    /// 周观看时长（小时）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours_viewed: Option<u64>,
    /// 周观看次数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub views: Option<u64>,
    /// 片长（分钟）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weeks_in_top10: Option<u32>,

    // ---- 红果短剧 ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_count: Option<u32>,
    /// 免费可看的集数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_episode_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paid: Option<bool>,
    /// 红果站内评分文本，例如「9.2分」
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub likes: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status_tags: Vec<String>,
    /// 前几集的试看视频地址
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_url: Option<String>,

    // ---- 音乐 ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// 时长（秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u32>,
    /// 上期名次
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rank: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankBoard {
    /// 源内唯一键，例如 `global:films_en`、`hk:tv`、`hot`、`netease:hot`、`tv:tencent`
    pub key: String,
    pub source: RankSource,
    pub name: String,
    /// 榜单种类：猫眼为榜单类型，Netflix 为地区代码，红果为榜单代码，音乐为平台代码
    pub board_type: String,
    /// 分组：猫眼为平台，Netflix 为地区，音乐为平台，红果为空
    pub group: String,
    pub group_name: String,
    /// `movie` / `tv`（猫眼、Netflix）
    #[serde(default)]
    pub kind: Option<String>,
    /// 数据周期：Netflix 的周起始日、猫眼的统计日期、音乐榜的更新日期
    #[serde(default)]
    pub period: Option<String>,
    /// 来源站点上的榜单页面
    #[serde(default)]
    pub url: Option<String>,
    /// 数据来源名称，例如「猫眼专业版」「Netflix Tudum」
    #[serde(default)]
    pub origin: String,
    /// 本榜数据的抓取时间；沿用旧数据的榜单保留原来的时间
    #[serde(default)]
    pub updated_at: String,
    /// 本轮抓取失败，展示的是上一次的数据
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub count: usize,
    pub items: Vec<RankItem>,
}

impl RankBoard {
    pub fn finish(mut self) -> Self {
        self.count = self.items.len();
        self
    }
}

/// 快照格式版本：字段含义变化时递增，旧版本快照在加载时丢弃并重新抓取。
pub const SNAPSHOT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankSnapshot {
    #[serde(default)]
    pub version: u32,
    pub source: RankSource,
    pub fetched_at: String,
    pub boards: Vec<RankBoard>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RankSourceView {
    pub source: RankSource,
    pub enabled: bool,
    pub fetched_at: Option<String>,
    pub boards: Vec<RankBoard>,
    pub refreshing: bool,
    /// 最近一次刷新失败的原因（有历史数据时仅作提示）
    pub error: Option<String>,
    /// 最近一次刷新的结果摘要
    pub message: Option<String>,
}

/// 数据源总览里的一行。
#[derive(Debug, Clone, Serialize)]
pub struct RankSourceSummary {
    pub source: RankSource,
    pub name: &'static str,
    pub origin: &'static str,
    pub homepage: &'static str,
    pub enabled: bool,
    pub board_count: usize,
    pub item_count: usize,
    /// 沿用上一次数据的榜单数量
    pub stale_count: usize,
    /// 带豆瓣评分的条目数（仅影视源有意义）
    pub rated_count: usize,
    /// 已匹配 TMDB、可直接订阅的条目数
    pub matched_count: usize,
    pub fetched_at: Option<String>,
    pub period: Option<String>,
    pub refreshing: bool,
    pub error: Option<String>,
    pub message: Option<String>,
}

impl RankSourceSummary {
    pub fn from_view(view: &RankSourceView) -> Self {
        let items = || view.boards.iter().flat_map(|board| &board.items);
        Self {
            source: view.source,
            name: view.source.label(),
            origin: view.source.origin(),
            homepage: view.source.homepage(),
            enabled: view.enabled,
            board_count: view.boards.len(),
            item_count: items().count(),
            stale_count: view.boards.iter().filter(|board| board.stale).count(),
            rated_count: items()
                .filter(|item| {
                    item.douban
                        .as_ref()
                        .is_some_and(|douban| douban.rating.is_some())
                        || item.score.is_some()
                })
                .count(),
            matched_count: items().filter(|item| item.tmdb.is_some()).count(),
            fetched_at: view.fetched_at.clone(),
            period: (view.source == RankSource::Netflix)
                .then(|| view.boards.iter().find_map(|board| board.period.clone()))
                .flatten(),
            refreshing: view.refreshing,
            error: view.error.clone(),
            message: view.message.clone(),
        }
    }
}

/// `/global-rank` 的过滤参数，语义与 GlobalRankPro 的 `/rank` 接口一致。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RankFilter {
    /// 榜单种类，逗号分隔多值
    #[serde(default)]
    pub board_type: Option<String>,
    /// `movie`（或 `film`）/ `tv`
    #[serde(default)]
    pub kind: Option<String>,
    /// 每个榜单最多返回的条目数
    #[serde(default)]
    pub limit: Option<usize>,
    /// 榜单名包含该关键字
    #[serde(default)]
    pub board: Option<String>,
    /// 分组代码：猫眼平台（tencent / iqiyi / youku / mango / all）
    #[serde(default)]
    pub platform: Option<String>,
}

impl RankFilter {
    pub fn apply(&self, boards: Vec<RankBoard>) -> Vec<RankBoard> {
        let types: Vec<&str> = self
            .board_type
            .as_deref()
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let kind = self
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
            .map(|kind| match kind {
                "film" => "movie",
                other => other,
            });
        let keyword = self
            .board
            .as_deref()
            .map(str::trim)
            .filter(|keyword| !keyword.is_empty());
        let platform = self
            .platform
            .as_deref()
            .map(str::trim)
            .filter(|platform| !platform.is_empty());
        boards
            .into_iter()
            .filter(|board| {
                types.is_empty()
                    || types
                        .iter()
                        .any(|wanted| *wanted == board.board_type || *wanted == board.key)
            })
            .filter(|board| kind.is_none_or(|kind| board.kind.as_deref() == Some(kind)))
            .filter(|board| {
                keyword.is_none_or(|keyword| {
                    board.name.contains(keyword) || board.group_name.contains(keyword)
                })
            })
            .filter(|board| platform.is_none_or(|platform| board.group == platform))
            .map(|mut board| {
                if let Some(limit) = self.limit.filter(|limit| *limit > 0) {
                    board.items.truncate(limit);
                }
                board.finish()
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 设置
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaoyanSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 抓取哪些榜单
    #[serde(default = "MaoyanSettings::default_charts")]
    pub charts: Vec<MaoyanChart>,
    /// 平台专项榜：热度榜额外抓取哪些平台（全网榜始终抓取）
    #[serde(default = "MaoyanSettings::default_platforms")]
    pub platforms: Vec<MaoyanPlatform>,
    /// 自动订阅：全网电影榜（电影票房 + 网络电影）前 N 条，0 为不订阅
    #[serde(default)]
    pub subscribe_movie: usize,
    /// 自动订阅：全网剧集综合榜前 N 条
    #[serde(default)]
    pub subscribe_tv: usize,
    /// 自动订阅：全网综艺榜前 N 条
    #[serde(default)]
    pub subscribe_variety: usize,
    /// 自动订阅：各平台剧集综合榜 / 综艺榜前 N 条
    #[serde(default)]
    pub subscribe_tencent: usize,
    #[serde(default)]
    pub subscribe_iqiyi: usize,
    #[serde(default)]
    pub subscribe_mango: usize,
    #[serde(default)]
    pub subscribe_youku: usize,
}

impl MaoyanSettings {
    fn default_charts() -> Vec<MaoyanChart> {
        MaoyanChart::ALL.to_vec()
    }

    fn default_platforms() -> Vec<MaoyanPlatform> {
        MaoyanPlatform::ALL
            .into_iter()
            .filter(|platform| *platform != MaoyanPlatform::All)
            .collect()
    }

    pub fn platform_subscribe(&self, platform: MaoyanPlatform) -> usize {
        match platform {
            MaoyanPlatform::All => 0,
            MaoyanPlatform::Tencent => self.subscribe_tencent,
            MaoyanPlatform::Iqiyi => self.subscribe_iqiyi,
            MaoyanPlatform::Mango => self.subscribe_mango,
            MaoyanPlatform::Youku => self.subscribe_youku,
        }
    }

    /// 某榜单的某平台是否需要抓取：全网榜跟随榜单开关；平台专项榜需要勾选了平台，
    /// 或者为该平台配置了自动订阅条数。
    pub fn wants(&self, chart: MaoyanChart, platform: MaoyanPlatform) -> bool {
        if !self.enabled || !self.charts.contains(&chart) || !chart.platforms().contains(&platform)
        {
            return false;
        }
        if platform == MaoyanPlatform::All || chart.platforms().len() == 1 {
            return true;
        }
        self.platforms.contains(&platform)
            || (self.platform_subscribe(platform) > 0
                && matches!(chart, MaoyanChart::WebAll | MaoyanChart::Variety))
    }

    /// 该榜单前多少条参与自动订阅。
    pub fn subscribe_count(&self, chart: MaoyanChart, platform: MaoyanPlatform) -> usize {
        match (chart, platform) {
            (MaoyanChart::Movie | MaoyanChart::WebMovie, _) => self.subscribe_movie,
            (MaoyanChart::WebAll, MaoyanPlatform::All) => self.subscribe_tv,
            (MaoyanChart::Variety, MaoyanPlatform::All) => self.subscribe_variety,
            (MaoyanChart::WebAll | MaoyanChart::Variety, platform) => {
                self.platform_subscribe(platform)
            }
            _ => 0,
        }
    }

    pub fn any_subscribe(&self) -> bool {
        self.enabled
            && (self.subscribe_movie
                + self.subscribe_tv
                + self.subscribe_variety
                + self.subscribe_tencent
                + self.subscribe_iqiyi
                + self.subscribe_mango
                + self.subscribe_youku)
                > 0
    }
}

impl Default for MaoyanSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            charts: Self::default_charts(),
            platforms: Self::default_platforms(),
            subscribe_movie: 0,
            subscribe_tv: 0,
            subscribe_variety: 0,
            subscribe_tencent: 0,
            subscribe_iqiyi: 0,
            subscribe_mango: 0,
            subscribe_youku: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetflixSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "NetflixSettings::default_regions")]
    pub regions: Vec<String>,
}

impl NetflixSettings {
    fn default_regions() -> Vec<String> {
        NETFLIX_REGIONS
            .iter()
            .map(|(code, _, _)| code.to_string())
            .collect()
    }
}

impl Default for NetflixSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            regions: Self::default_regions(),
        }
    }
}

pub const HONGGUO_LIMIT_CHOICES: [usize; 4] = [20, 40, 60, 100];
pub const MUSIC_LIMIT_CHOICES: [usize; 3] = [30, 50, 100];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HongguoSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "HongguoSettings::default_boards")]
    pub boards: Vec<String>,
    /// 每榜抓取条数，官网每页 20 条
    #[serde(default = "HongguoSettings::default_limit")]
    pub limit: usize,
}

impl HongguoSettings {
    fn default_boards() -> Vec<String> {
        HONGGUO_BOARDS
            .iter()
            .map(|(key, _, _)| key.to_string())
            .collect()
    }

    fn default_limit() -> usize {
        20
    }
}

impl Default for HongguoSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            boards: Self::default_boards(),
            limit: Self::default_limit(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MusicSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 榜单键，例如 `netease:hot`
    #[serde(default = "MusicSettings::default_boards")]
    pub boards: Vec<String>,
    #[serde(default = "MusicSettings::default_limit")]
    pub limit: usize,
}

impl MusicSettings {
    fn default_boards() -> Vec<String> {
        MUSIC_BOARDS
            .iter()
            .map(|board| format!("{}:{}", board.0, board.2))
            .collect()
    }

    fn default_limit() -> usize {
        MUSIC_LIMIT
    }
}

impl Default for MusicSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            boards: Self::default_boards(),
            limit: Self::default_limit(),
        }
    }
}

/// 自动订阅使用的默认参数。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RankSubscribeDefaults {
    #[serde(default)]
    pub quality_profile_id: Option<i64>,
    #[serde(default)]
    pub downloader_id: Option<i64>,
    /// 留空表示使用全部已启用的站点
    #[serde(default)]
    pub site_ids: Vec<i64>,
    #[serde(default)]
    pub movie_save_path: Option<String>,
    #[serde(default)]
    pub tv_save_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RankSettings {
    /// 后台按各来源的周期自动刷新；关闭后只在手动点击时刷新
    #[serde(default = "default_true")]
    pub auto_refresh: bool,
    #[serde(default)]
    pub maoyan: MaoyanSettings,
    #[serde(default)]
    pub netflix: NetflixSettings,
    #[serde(default)]
    pub hongguo: HongguoSettings,
    #[serde(default)]
    pub music: MusicSettings,
    #[serde(default)]
    pub subscribe: RankSubscribeDefaults,
}

impl Default for RankSettings {
    fn default() -> Self {
        Self {
            auto_refresh: true,
            maoyan: MaoyanSettings::default(),
            netflix: NetflixSettings::default(),
            hongguo: HongguoSettings::default(),
            music: MusicSettings::default(),
            subscribe: RankSubscribeDefaults::default(),
        }
    }
}

const MAX_SUBSCRIBE_COUNT: usize = 30;

fn dedup<T: PartialEq + Clone>(values: &[T]) -> Vec<T> {
    let mut seen: Vec<T> = Vec::new();
    for value in values {
        if !seen.contains(value) {
            seen.push(value.clone());
        }
    }
    seen
}

fn nearest_choice(value: usize, choices: &[usize]) -> usize {
    choices
        .iter()
        .copied()
        .min_by_key(|choice| choice.abs_diff(value))
        .unwrap_or(value)
}

impl RankSettings {
    /// 去掉无效取值并收敛到允许的范围。
    pub fn normalized(mut self) -> Self {
        self.maoyan.charts = dedup(&self.maoyan.charts);
        self.maoyan.platforms = dedup(&self.maoyan.platforms)
            .into_iter()
            .filter(|platform| *platform != MaoyanPlatform::All)
            .collect();
        for count in [
            &mut self.maoyan.subscribe_movie,
            &mut self.maoyan.subscribe_tv,
            &mut self.maoyan.subscribe_variety,
            &mut self.maoyan.subscribe_tencent,
            &mut self.maoyan.subscribe_iqiyi,
            &mut self.maoyan.subscribe_mango,
            &mut self.maoyan.subscribe_youku,
        ] {
            *count = (*count).min(MAX_SUBSCRIBE_COUNT);
        }
        self.netflix.regions = NETFLIX_REGIONS
            .iter()
            .map(|(code, _, _)| code.to_string())
            .filter(|code| self.netflix.regions.contains(code))
            .collect();
        self.hongguo.boards = HONGGUO_BOARDS
            .iter()
            .map(|(key, _, _)| key.to_string())
            .filter(|key| self.hongguo.boards.contains(key))
            .collect();
        self.hongguo.limit = nearest_choice(self.hongguo.limit, &HONGGUO_LIMIT_CHOICES);
        self.music.boards = MusicSettings::default_boards()
            .into_iter()
            .filter(|key| self.music.boards.contains(key))
            .collect();
        self.music.limit = nearest_choice(self.music.limit, &MUSIC_LIMIT_CHOICES);
        self.subscribe.site_ids = dedup(&self.subscribe.site_ids);
        let clean = |path: &mut Option<String>| {
            *path = path
                .take()
                .map(|path| path.trim().to_string())
                .filter(|path| !path.is_empty());
        };
        clean(&mut self.subscribe.movie_save_path);
        clean(&mut self.subscribe.tv_save_path);
        self
    }

    pub fn enabled(&self, source: RankSource) -> bool {
        match source {
            RankSource::Maoyan => self.maoyan.enabled,
            RankSource::Netflix => self.netflix.enabled,
            RankSource::Hongguo => self.hongguo.enabled,
            RankSource::Music => self.music.enabled,
        }
    }
}

/// 设置页的可选项。
#[derive(Debug, Clone, Serialize)]
pub struct RankOption {
    pub value: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RankSettingsOptions {
    pub maoyan_charts: Vec<RankOption>,
    pub maoyan_platforms: Vec<RankOption>,
    pub netflix_regions: Vec<RankOption>,
    pub hongguo_boards: Vec<RankOption>,
    pub hongguo_limits: Vec<usize>,
    pub music_boards: Vec<RankOption>,
    pub music_limits: Vec<usize>,
    pub max_subscribe_count: usize,
}

impl RankSettingsOptions {
    pub fn build() -> Self {
        let option = |value: &str, label: &str, group: Option<&str>| RankOption {
            value: value.to_string(),
            label: label.to_string(),
            group: group.map(str::to_string),
        };
        Self {
            maoyan_charts: MaoyanChart::ALL
                .into_iter()
                .map(|chart| {
                    let group = if chart.media_type() == TmdbMediaType::Movie {
                        "电影"
                    } else {
                        "剧集 / 综艺"
                    };
                    option(chart.as_str(), chart.label(), Some(group))
                })
                .collect(),
            maoyan_platforms: MaoyanSettings::default_platforms()
                .into_iter()
                .map(|platform| option(platform.as_str(), platform.label(), None))
                .collect(),
            netflix_regions: NETFLIX_REGIONS
                .iter()
                .map(|(code, name, _)| option(code, name, None))
                .collect(),
            hongguo_boards: HONGGUO_BOARDS
                .iter()
                .map(|(key, _, name)| option(key, name, None))
                .collect(),
            hongguo_limits: HONGGUO_LIMIT_CHOICES.to_vec(),
            music_boards: MUSIC_BOARDS
                .iter()
                .map(|board| option(&format!("{}:{}", board.0, board.2), board.3, Some(board.1)))
                .collect(),
            music_limits: MUSIC_LIMIT_CHOICES.to_vec(),
            max_subscribe_count: MAX_SUBSCRIBE_COUNT,
        }
    }
}

/// 自动订阅记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankSubscribeRecord {
    /// 去重键：同一条目只自动订阅一次，之后手动删掉订阅也不会再次添加
    pub unique: String,
    pub title: String,
    pub media_type: TmdbMediaType,
    pub tmdb_id: i64,
    pub tmdb_title: String,
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub season: Option<u32>,
    #[serde(default)]
    pub poster_path: Option<String>,
    /// 来自哪个榜单
    pub board: String,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub release_info: Option<String>,
    #[serde(default)]
    pub subscription_id: Option<i64>,
    pub time: String,
}

/// 一轮刷新的结果，对应 GlobalRankPro 的 `RankSourceRefresh`。
#[derive(Debug, Clone, Default)]
pub struct FetchOutcome {
    pub boards: Vec<RankBoard>,
    /// 部分榜单失败的原因
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// 快照存储
// ---------------------------------------------------------------------------

/// 条目的媒体身份（TMDB 匹配与豆瓣评分），跨轮次复用。
#[derive(Debug, Clone, Default)]
pub struct RankIdentity {
    pub tmdb: Option<TmdbMedia>,
    pub douban: Option<DoubanSubject>,
    pub douban_checked_at: Option<String>,
}

#[derive(Default)]
pub struct GlobalRankStore {
    loaded: tokio::sync::OnceCell<()>,
    snapshots: Mutex<HashMap<RankSource, RankSnapshot>>,
    errors: Mutex<HashMap<RankSource, String>>,
    messages: Mutex<HashMap<RankSource, String>>,
    refreshing: [AtomicBool; 4],
    last_attempt: Mutex<HashMap<RankSource, Instant>>,
    matches: Mutex<HashMap<(String, &'static str), (Instant, Option<TmdbMedia>)>>,
    settings: Mutex<Option<RankSettings>>,
    posters: Mutex<std::collections::VecDeque<(String, String)>>,
}

pub struct RefreshGuard<'a>(&'a AtomicBool);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn slot(source: RankSource) -> usize {
    match source {
        RankSource::Maoyan => 0,
        RankSource::Netflix => 1,
        RankSource::Hongguo => 2,
        RankSource::Music => 3,
    }
}

/// 海报代理的内存缓存张数。
const POSTER_CACHE_SIZE: usize = 200;

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
                    // 旧格式的快照缺少分项字段，丢弃后由后台重新抓取。
                    if snapshot.version == SNAPSHOT_VERSION {
                        entries.entry(snapshot.source).or_insert(snapshot);
                    }
                }
            })
            .await;
    }

    pub async fn cached_settings(&self) -> Option<RankSettings> {
        self.settings.lock().await.clone()
    }

    pub async fn store_settings(&self, settings: RankSettings) {
        *self.settings.lock().await = Some(settings);
    }

    pub fn is_refreshing(&self, source: RankSource) -> bool {
        self.refreshing[slot(source)].load(Ordering::SeqCst)
    }

    pub fn try_begin_refresh(&self, source: RankSource) -> Option<RefreshGuard<'_>> {
        let flag = &self.refreshing[slot(source)];
        (!flag.swap(true, Ordering::SeqCst)).then(|| RefreshGuard(flag))
    }

    pub async fn mark_attempt(&self, source: RankSource) {
        self.last_attempt
            .lock()
            .await
            .insert(source, Instant::now());
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

    pub async fn view(&self, source: RankSource, enabled: bool) -> RankSourceView {
        let snapshot = self.snapshot(source).await;
        RankSourceView {
            source,
            enabled,
            fetched_at: snapshot
                .as_ref()
                .map(|snapshot| snapshot.fetched_at.clone()),
            boards: snapshot.map(|snapshot| snapshot.boards).unwrap_or_default(),
            refreshing: self.is_refreshing(source),
            error: self.error(source).await,
            message: self.message(source).await,
        }
    }

    pub async fn put(&self, snapshot: RankSnapshot) {
        self.errors.lock().await.remove(&snapshot.source);
        self.snapshots
            .lock()
            .await
            .insert(snapshot.source, snapshot);
    }

    pub async fn error(&self, source: RankSource) -> Option<String> {
        self.errors.lock().await.get(&source).cloned()
    }

    pub async fn set_error(&self, source: RankSource, error: String) {
        self.errors.lock().await.insert(source, error);
    }

    pub async fn message(&self, source: RankSource) -> Option<String> {
        self.messages.lock().await.get(&source).cloned()
    }

    pub async fn set_message(&self, source: RankSource, message: String) {
        self.messages.lock().await.insert(source, message);
    }

    pub async fn cached_match(
        &self,
        title: &str,
        media_type: &TmdbMediaType,
    ) -> Option<Option<TmdbMedia>> {
        let cache = self.matches.lock().await;
        cache
            .get(&(title.to_string(), media_type.as_str()))
            .filter(|(stored, _)| stored.elapsed() < MATCH_CACHE_TTL)
            .map(|(_, found)| found.clone())
    }

    pub async fn store_match(
        &self,
        title: String,
        media_type: &TmdbMediaType,
        found: Option<TmdbMedia>,
    ) {
        let mut cache = self.matches.lock().await;
        cache.retain(|_, (stored, _)| stored.elapsed() < MATCH_CACHE_TTL);
        cache.insert((title, media_type.as_str()), (Instant::now(), found));
    }

    /// 上一次快照里的媒体身份，按（原始片名, 类型）索引；重启后不必重新搜索 TMDB 和豆瓣。
    pub async fn previous_identities(
        &self,
        source: RankSource,
    ) -> HashMap<(String, &'static str), RankIdentity> {
        let snapshots = self.snapshots.lock().await;
        let mut found: HashMap<(String, &'static str), RankIdentity> = HashMap::new();
        if let Some(snapshot) = snapshots.get(&source) {
            for board in &snapshot.boards {
                for item in &board.items {
                    let Some(media_type) = &item.media_type else {
                        continue;
                    };
                    let original = item
                        .original_title
                        .as_deref()
                        .unwrap_or(&item.title)
                        .to_string();
                    let entry = found.entry((original, media_type.as_str())).or_default();
                    if entry.tmdb.is_none() {
                        entry.tmdb = item.tmdb.clone();
                    }
                    if entry.douban.is_none() {
                        entry.douban = item.douban.clone();
                    }
                    if entry.douban_checked_at.is_none() {
                        entry.douban_checked_at = item.douban_checked_at.clone();
                    }
                }
            }
        }
        found
    }

    pub async fn cached_poster(&self, url: &str) -> Option<String> {
        self.posters
            .lock()
            .await
            .iter()
            .find(|(key, _)| key == url)
            .map(|(_, data)| data.clone())
    }

    pub async fn store_poster(&self, url: String, data_url: String) {
        let mut posters = self.posters.lock().await;
        if posters.iter().any(|(key, _)| *key == url) {
            return;
        }
        if posters.len() >= POSTER_CACHE_SIZE {
            posters.pop_front();
        }
        posters.push_back((url, data_url));
    }
}

// ---------------------------------------------------------------------------
// 海报代理
// ---------------------------------------------------------------------------

/// 只代理榜单里出现的图床，避免成为开放代理：字节图床（红果海报）、Apple 封面、
/// 网易云与 QQ 音乐封面。
pub fn poster_url_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" || parsed.port().is_some() || !parsed.username().is_empty() {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let path = parsed.path();
    let under = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    (under("byteimg.com") && path.starts_with("/novel-pic/"))
        || (under("ecombdimg.com") && path.starts_with("/obj/"))
        || (under("mzstatic.com") && path.starts_with("/image/"))
        || (under("music.126.net") && !path.is_empty())
        || (host == "y.qq.com" && path.starts_with("/music/photo_new/"))
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.len() > 12 && &bytes[4..8] == b"ftyp" {
        // HEIC / AVIF：字节图床默认返回 HEIC，浏览器不一定能显示，仍原样返回类型。
        Some(if &bytes[8..12] == b"avif" {
            "image/avif"
        } else {
            "image/heic"
        })
    } else {
        None
    }
}

const POSTER_MAX_BYTES: usize = 3 * 1024 * 1024;

/// 由服务端代取榜单海报，返回 data URL（桌面端同样可用）。
pub async fn fetch_poster(http: &reqwest::Client, url: &str) -> Result<String, String> {
    if !poster_url_allowed(url) {
        return Err("不支持代理该图片地址".to_string());
    }
    let referer = if url.contains("byteimg.com") || url.contains("ecombdimg.com") {
        Some("https://hongguoduanju.com/")
    } else {
        None
    };
    let bytes = get_bytes(http, url, referer, Duration::from_secs(20)).await?;
    if bytes.len() > POSTER_MAX_BYTES {
        return Err("图片过大".to_string());
    }
    let mime = image_mime(&bytes).ok_or("来源返回的不是图片")?;
    use base64::Engine;
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

/// 带重试的 GET：海外源（Netflix、Apple）在部分网络下偶发中途断流，重试两次。
async fn get_bytes(
    http: &reqwest::Client,
    url: &str,
    referer: Option<&str>,
    timeout: Duration,
) -> Result<bytes::Bytes, String> {
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
    let response = request
        .send()
        .await
        .map_err(|error| (format!("请求失败：{error}"), true))?;
    let status = response.status();
    if !status.is_success() {
        return Err((
            format!("HTTP {}", status.as_u16()),
            status.is_server_error(),
        ));
    }
    response
        .bytes()
        .await
        .map_err(|error| (format!("读取响应失败：{error}"), true))
}

// ---------------------------------------------------------------------------
// Netflix
// ---------------------------------------------------------------------------

/// 地区代码与中文名；顺序即前端展示顺序。Netflix 未进入中国大陆。
/// （代码, 中文名, Tudum 页面路径）
pub const NETFLIX_REGIONS: [(&str, &str, &str); 7] = [
    ("global", "全球", ""),
    ("us", "美国", "united-states"),
    ("hk", "中国香港", "hong-kong"),
    ("tw", "中国台湾", "taiwan"),
    ("jp", "日本", "japan"),
    ("kr", "韩国", "south-korea"),
    ("sg", "新加坡", "singapore"),
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
    let get = |index: Option<usize>| {
        index
            .and_then(|index| fields.get(index))
            .map(|value| value.trim())
    };
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
        runtime_hours: get(columns.runtime)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0.0),
        weeks_in_top10: get(columns.weeks)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
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
    let latest = rows
        .iter()
        .map(|row| row.week.clone())
        .max()
        .unwrap_or_default();
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
            regions: regions
                .iter()
                .map(|region| region.to_ascii_uppercase())
                .collect(),
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
        let Some(country) = columns
            .country
            .and_then(|index| fields.get(index))
            .map(|value| value.trim().to_ascii_uppercase())
        else {
            return;
        };
        if !self.regions.contains(&country) {
            return;
        }
        let Some(row) = netflix_row(&fields, columns) else {
            return;
        };
        let entry = self
            .rows
            .entry(country)
            .or_insert_with(|| (row.week.clone(), Vec::new()));
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

impl NetflixRow {
    pub fn media_type(&self) -> TmdbMediaType {
        if self.category.starts_with("TV") {
            TmdbMediaType::Tv
        } else {
            TmdbMediaType::Movie
        }
    }

    pub fn into_item(self) -> RankItem {
        let media_type = self.media_type();
        let runtime_minutes =
            (self.runtime_hours > 0.0).then(|| (self.runtime_hours * 60.0).round() as u32);
        RankItem {
            rank: self.rank,
            season_title: self.season_title.clone(),
            hours_viewed: Some(self.hours_viewed).filter(|value| *value > 0),
            views: Some(self.views).filter(|value| *value > 0),
            runtime_minutes,
            weeks_in_top10: Some(self.weeks_in_top10).filter(|value| *value > 0),
            original_title: Some(self.title.clone()),
            new_entry: self.weeks_in_top10 == 1,
            media_type: Some(media_type),
            title: self.title,
            ..RankItem::default()
        }
    }
}

/// 全球榜四个类目：（TSV 类目, 键, 名称, 类型）。
const NETFLIX_GLOBAL_CATEGORIES: [(&str, &str, &str, &str); 4] = [
    ("Films (English)", "films_en", "电影（英语）", "movie"),
    (
        "Films (Non-English)",
        "films_other",
        "电影（非英语）",
        "movie",
    ),
    ("TV (English)", "tv_en", "剧集（英语）", "tv"),
    ("TV (Non-English)", "tv_other", "剧集（非英语）", "tv"),
];

pub fn netflix_boards(
    global: Option<(String, Vec<NetflixRow>)>,
    countries: &HashMap<String, (String, Vec<NetflixRow>)>,
    regions: &[String],
) -> Vec<RankBoard> {
    let now = Utc::now().to_rfc3339();
    let mut boards = Vec::new();
    let board = |key: String,
                 name: String,
                 region: &str,
                 region_name: &str,
                 kind: &str,
                 week: &str,
                 slug: &str,
                 rows: Vec<NetflixRow>| {
        RankBoard {
            key,
            source: RankSource::Netflix,
            name,
            board_type: region.to_string(),
            group: region.to_string(),
            group_name: region_name.to_string(),
            kind: Some(kind.to_string()),
            period: Some(week.to_string()),
            url: Some(if slug.is_empty() {
                "https://www.netflix.com/tudum/top10".to_string()
            } else {
                format!("https://www.netflix.com/tudum/top10/{slug}")
            }),
            origin: "Netflix Tudum".to_string(),
            updated_at: now.clone(),
            stale: false,
            count: 0,
            items: rows.into_iter().map(NetflixRow::into_item).collect(),
        }
        .finish()
    };
    for (region, region_name, slug) in NETFLIX_REGIONS {
        if !regions.iter().any(|wanted| wanted == region) {
            continue;
        }
        if region == "global" {
            let Some((week, rows)) = &global else {
                continue;
            };
            for (category, key, name, kind) in NETFLIX_GLOBAL_CATEGORIES {
                let mut items: Vec<NetflixRow> = rows
                    .iter()
                    .filter(|row| row.category == category)
                    .cloned()
                    .collect();
                if items.is_empty() {
                    continue;
                }
                items.sort_by_key(|row| row.rank);
                boards.push(board(
                    format!("global:{key}"),
                    format!("全球 · {name}"),
                    region,
                    region_name,
                    kind,
                    week,
                    slug,
                    items,
                ));
            }
            continue;
        }
        let Some((week, rows)) = countries.get(region) else {
            continue;
        };
        for (category, kind, name) in [("Films", "movie", "电影"), ("TV", "tv", "剧集")] {
            let mut items: Vec<NetflixRow> = rows
                .iter()
                .filter(|row| row.category == category)
                .cloned()
                .collect();
            if items.is_empty() {
                continue;
            }
            items.sort_by_key(|row| row.rank);
            boards.push(board(
                format!("{region}:{kind}"),
                format!("{region_name} · {name}"),
                region,
                region_name,
                kind,
                week,
                slug,
                items,
            ));
        }
    }
    boards
}

pub async fn fetch_netflix_global(
    http: &reqwest::Client,
) -> Result<(String, Vec<NetflixRow>), String> {
    let bytes = get_bytes(http, NETFLIX_GLOBAL_TSV, None, Duration::from_secs(120)).await?;
    let parsed = parse_netflix_global(&String::from_utf8_lossy(&bytes));
    if parsed.1.is_empty() {
        return Err("Netflix 全球榜数据为空或格式变化".to_string());
    }
    Ok(parsed)
}

pub async fn fetch_netflix_countries(
    http: &reqwest::Client,
    regions: &[String],
) -> Result<HashMap<String, (String, Vec<NetflixRow>)>, String> {
    let mut last_error = String::new();
    for attempt in 0..2u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        match fetch_netflix_countries_once(http, regions).await {
            Ok(rows) => return Ok(rows),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

async fn fetch_netflix_countries_once(
    http: &reqwest::Client,
    regions: &[String],
) -> Result<HashMap<String, (String, Vec<NetflixRow>)>, String> {
    let regions: Vec<&str> = regions
        .iter()
        .map(String::as_str)
        .filter(|region| *region != "global")
        .collect();
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
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("读取响应失败：{error}"))?
    {
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
            map.values()
                .find_map(|value| find_rank_payload(value, depth + 1))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_rank_payload(value, depth + 1)),
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
        .map(|values| {
            values
                .iter()
                .filter_map(|value| json_text(Some(value)))
                .collect()
        })
        .unwrap_or_default()
}

/// 海报缩略后缀从 640 提到 1080（源图上限 896×1280）。
fn upgrade_cover(url: &str) -> String {
    match url.rfind("~tplv-") {
        Some(index) => {
            let (base, suffix) = url.split_at(index);
            let extension = suffix
                .rfind('.')
                .map(|dot| &suffix[dot..])
                .unwrap_or(".image");
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

/// 试看前几集（官网对未登录用户开放的集数有限）。
const HONGGUO_PREVIEW_EPISODES: usize = 3;
/// 翻页上限：每页 20 条，100 条需要 5 页，留一页余量。
const HONGGUO_MAX_PAGES: usize = 6;

fn json_u32(value: Option<&Value>) -> Option<u32> {
    match value? {
        Value::Number(number) => number.as_u64().map(|value| value as u32),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

pub fn parse_hongguo_items(payload: &Value) -> Vec<RankItem> {
    let Some(rows) = payload.get("rankList").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let title = json_text(row.get("title"))?;
            let series_id = json_text(row.get("seriesId")).or_else(|| json_text(row.get("id")));
            let status_tags = json_strings(row.get("statusTags"));
            let vids = json_strings(row.get("episodeVids"));
            // episodeCount 是总集数；旧结构没有该字段时退回可试看的集数。
            let episode_count = json_u32(row.get("episodeCount"))
                .filter(|count| *count > 0)
                .or_else(|| Some(vids.len() as u32).filter(|count| *count > 0));
            Some(RankItem {
                rank: json_u32(row.get("rank")).map_or(index + 1, |rank| rank as usize),
                title,
                poster: json_text(row.get("cover")).map(|cover| upgrade_cover(&cover)),
                url: series_id
                    .as_ref()
                    .map(|id| format!("{HONGGUO_SITE}/detail?series_id={id}")),
                play_url: json_text(row.get("playHref"))
                    .filter(|href| href.starts_with('/'))
                    .map(|href| format!("{HONGGUO_SITE}{href}")),
                overview: json_text(row.get("description")),
                heat: json_text(row.get("heatText")),
                score: json_text(row.get("scoreText")),
                favorite: json_text(row.get("favoriteText")),
                likes: json_text(row.get("likeText")),
                tags: json_strings(row.get("tags")),
                new_entry: status_tags.iter().any(|tag| tag == "新剧"),
                status_tags,
                episode_count,
                open_episode_count: json_u32(row.get("openEpisodeCount"))
                    .filter(|count| *count > 0),
                paid: row.get("isPaid").and_then(Value::as_bool),
                preview_urls: vids
                    .iter()
                    .take(HONGGUO_PREVIEW_EPISODES)
                    .filter(|vid| vid.bytes().all(|byte| byte.is_ascii_alphanumeric()))
                    .map(|vid| format!("{HONGGUO_SITE}/video/public/{vid}/360p.mp4"))
                    .collect(),
                series_id,
                ..RankItem::default()
            })
        })
        .collect()
}

/// 抓取一页；返回（条目, 是否还有下一页）。
async fn fetch_hongguo_page(
    http: &reqwest::Client,
    route: &str,
    page: usize,
) -> Result<(Vec<RankItem>, bool), String> {
    let url = if page > 1 {
        format!("{HONGGUO_SITE}/rank/{route}?page={page}")
    } else {
        format!("{HONGGUO_SITE}/rank/{route}")
    };
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
            Some(payload) if payload.get("isSuccess").and_then(Value::as_bool) == Some(false) => {
                last_error = "官网返回失败状态".to_string();
            }
            Some(payload) => {
                let items = parse_hongguo_items(&payload);
                if !items.is_empty() {
                    let more = payload
                        .pointer("/pagination/nextUrl")
                        .and_then(Value::as_str)
                        .is_some_and(|next| !next.is_empty());
                    return Ok((items, more));
                }
                last_error = "官网返回的榜单为空".to_string();
            }
            None => last_error = "页面中没有找到榜单数据，官网结构可能已变化".to_string(),
        }
    }
    Err(last_error)
}

/// 按每页 20 条向后翻页，直到凑够 `limit` 条或没有下一页。后续页失败时保留已取到的条目。
pub async fn fetch_hongguo_board(
    http: &reqwest::Client,
    route: &str,
    limit: usize,
) -> Result<Vec<RankItem>, String> {
    let mut items: Vec<RankItem> = Vec::new();
    for page in 1..=HONGGUO_MAX_PAGES {
        if page > 1 {
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        match fetch_hongguo_page(http, route, page).await {
            Ok((rows, more)) => {
                for row in rows {
                    if !items
                        .iter()
                        .any(|item| item.series_id.is_some() && item.series_id == row.series_id)
                    {
                        items.push(row);
                    }
                }
                if items.len() >= limit || !more {
                    break;
                }
            }
            Err(error) if items.is_empty() => return Err(error),
            Err(error) => {
                tracing::debug!(route, page, %error, "hongguo page failed; keeping earlier pages");
                break;
            }
        }
    }
    items.truncate(limit);
    Ok(items)
}

pub async fn fetch_hongguo_boards(
    http: &reqwest::Client,
    settings: &HongguoSettings,
) -> FetchOutcome {
    let mut boards = Vec::new();
    let mut errors = Vec::new();
    let wanted = HONGGUO_BOARDS
        .into_iter()
        .filter(|(key, _, _)| settings.boards.iter().any(|board| board == key));
    for (index, (key, route, name)) in wanted.enumerate() {
        if index > 0 {
            // 官网是 SSR 直出，放慢节奏避免被当成爬虫。
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        match fetch_hongguo_board(http, route, settings.limit).await {
            Ok(items) => boards.push(
                RankBoard {
                    key: key.to_string(),
                    source: RankSource::Hongguo,
                    name: format!("红果{name}"),
                    board_type: key.to_string(),
                    group: String::new(),
                    group_name: String::new(),
                    kind: None,
                    period: None,
                    url: Some(format!("{HONGGUO_SITE}/rank/{route}")),
                    origin: "红果短剧官网".to_string(),
                    updated_at: Utc::now().to_rfc3339(),
                    stale: false,
                    count: 0,
                    items,
                }
                .finish(),
            ),
            Err(error) => errors.push(format!("红果{name}：{error}")),
        }
    }
    FetchOutcome {
        boards,
        error: (!errors.is_empty()).then(|| errors.join("；")),
    }
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

async fn get_json(
    http: &reqwest::Client,
    url: &str,
    referer: Option<&str>,
) -> Result<Value, String> {
    let bytes = get_bytes(http, url, referer, Duration::from_secs(30)).await?;
    // QQ 音乐的响应开头带空格。
    let text = String::from_utf8_lossy(&bytes);
    serde_json::from_str(text.trim()).map_err(|error| format!("JSON 解析失败：{error}"))
}

/// 一个音乐榜的解析结果。
#[derive(Debug, Default)]
pub struct MusicChart {
    /// 平台给出的榜名（没有时用内置名称）
    pub name: Option<String>,
    pub period: Option<String>,
    pub items: Vec<RankItem>,
}

fn join_names(values: Option<&Value>) -> Option<String> {
    let names: Vec<String> = values
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| json_text(row.get("name")))
                .collect()
        })
        .unwrap_or_default();
    Some(names.join(" / ")).filter(|value| !value.is_empty())
}

pub fn parse_netease(payload: &Value, limit: usize) -> Result<MusicChart, String> {
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
            let album = track.get("album");
            // lastRank 是上期名次（从 0 开始），缺失表示新上榜。
            let previous = track
                .get("lastRank")
                .and_then(Value::as_i64)
                .filter(|previous| *previous >= 0);
            let song_id = json_text(track.get("id"));
            Some(RankItem {
                rank,
                title,
                artist: join_names(track.get("artists")),
                album: album.and_then(|album| json_text(album.get("name"))),
                duration: track
                    .get("duration")
                    .and_then(Value::as_u64)
                    .map(|millis| (millis / 1000) as u32)
                    .filter(|seconds| *seconds > 0),
                poster: album
                    .and_then(|album| json_text(album.get("picUrl")))
                    .map(|url| format!("{}?param=300y300", https(&url))),
                url: song_id
                    .as_ref()
                    .map(|id| format!("https://music.163.com/#/song?id={id}")),
                song_id,
                last_rank: previous.map(|previous| previous as u32 + 1),
                rank_change: previous.map(|previous| previous + 1 - rank as i64),
                new_entry: previous.is_none(),
                ..RankItem::default()
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
    Ok(MusicChart {
        name: json_text(result.get("name")),
        period,
        items,
    })
}

pub fn parse_qq(payload: &Value, limit: usize) -> Result<MusicChart, String> {
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
            let rank = index + 1;
            // 热歌、新歌榜的 cur_count / old_count 是本期 / 上期名次（上期 0 表示新上榜）；
            // 飙升、流行指数榜里这两个字段是飙升值和指数，不能当名次用。
            let ranked = json_u32(row.get("cur_count")) == Some(rank as u32);
            let previous = json_u32(row.get("old_count")).filter(|_| ranked);
            let song_mid = json_text(data.get("songmid"));
            Some(RankItem {
                rank,
                title,
                artist: join_names(data.get("singer")),
                album: json_text(data.get("albumname")),
                duration: json_u32(data.get("interval")).filter(|seconds| *seconds > 0),
                poster: json_text(data.get("albummid")).map(|mid| {
                    format!("https://y.qq.com/music/photo_new/T002R300x300M000{mid}.jpg")
                }),
                // 分享页不需要登录，也不会被重定向到客户端下载页。
                url: song_mid.as_ref().map(|mid| {
                    format!("https://i.y.qq.com/n2/m/share/details/song.html?songmid={mid}")
                }),
                song_id: song_mid,
                last_rank: previous.filter(|previous| *previous > 0),
                rank_change: previous
                    .filter(|previous| *previous > 0)
                    .map(|previous| previous as i64 - rank as i64),
                new_entry: previous == Some(0),
                ..RankItem::default()
            })
        })
        .collect();
    Ok(MusicChart {
        name: json_text(payload.pointer("/topinfo/ListName"))
            .map(|name| name.replace("巅峰榜·", "")),
        period: json_text(payload.get("update_time")).or_else(|| json_text(payload.get("date"))),
        items,
    })
}

pub fn parse_apple(payload: &Value, limit: usize) -> Result<MusicChart, String> {
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
            Some(RankItem {
                rank: index + 1,
                title: json_text(row.get("name"))?,
                artist: json_text(row.get("artistName")),
                poster: json_text(row.get("artworkUrl100"))
                    .map(|url| url.replace("100x100bb", "300x300bb")),
                url: json_text(row.get("url")),
                song_id: json_text(row.get("id")),
                genre: row
                    .get("genres")
                    .and_then(Value::as_array)
                    .and_then(|genres| genres.iter().find_map(|genre| json_text(genre.get("name"))))
                    .filter(|genre| genre != "音乐" && genre != "Music"),
                ..RankItem::default()
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
    Ok(MusicChart {
        name: None,
        period,
        items,
    })
}

/// 抓取一个音乐榜，返回（解析结果, 榜单页面地址）。
pub async fn fetch_music_board(
    http: &reqwest::Client,
    platform: &str,
    native_id: &str,
    limit: usize,
) -> Result<(MusicChart, String), String> {
    match platform {
        "netease" => {
            let payload = get_json(
                http,
                &format!("https://music.163.com/api/playlist/detail?id={native_id}"),
                Some("https://music.163.com"),
            )
            .await?;
            Ok((
                parse_netease(&payload, limit)?,
                format!("https://music.163.com/#/discover/toplist?id={native_id}"),
            ))
        }
        "qq" => {
            let payload = get_json(
                http,
                &format!("https://c.y.qq.com/v8/fcg-bin/fcg_v8_toplist_cp.fcg?topid={native_id}&format=json&page=detail&tpl=3&type=top"),
                Some("https://y.qq.com"),
            )
            .await?;
            Ok((
                parse_qq(&payload, limit)?,
                format!("https://y.qq.com/n/ryqq/toplist/{native_id}"),
            ))
        }
        _ => {
            // Apple 的公开 Feed 最多 100 条。
            let count = limit.min(100);
            let payload = get_json(
                http,
                &format!("https://rss.applemarketingtools.com/api/v2/cn/music/most-played/{count}/songs.json"),
                None,
            )
            .await?;
            Ok((
                parse_apple(&payload, limit)?,
                "https://music.apple.com/cn/playlist/pl.f4d106fed2bd41149aaacabb233eb5eb"
                    .to_string(),
            ))
        }
    }
}

pub async fn fetch_music_boards(http: &reqwest::Client, settings: &MusicSettings) -> FetchOutcome {
    let limit = settings.limit;
    let wanted = MUSIC_BOARDS.into_iter().filter(|board| {
        settings
            .boards
            .contains(&format!("{}:{}", board.0, board.2))
    });
    let results = futures::future::join_all(wanted.map(
        |(platform, platform_name, key, name, native)| async move {
            let result = fetch_music_board(http, platform, native, limit).await;
            (platform, platform_name, key, name, result)
        },
    ))
    .await;
    let mut boards = Vec::new();
    let mut errors = Vec::new();
    for (platform, platform_name, key, name, result) in results {
        match result {
            Ok((chart, url)) if !chart.items.is_empty() => boards.push(
                RankBoard {
                    key: format!("{platform}:{key}"),
                    source: RankSource::Music,
                    name: chart
                        .name
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| name.to_string()),
                    board_type: platform.to_string(),
                    group: platform.to_string(),
                    group_name: platform_name.to_string(),
                    kind: None,
                    period: chart.period,
                    url: Some(url),
                    origin: platform_name.to_string(),
                    updated_at: Utc::now().to_rfc3339(),
                    stale: false,
                    count: 0,
                    items: chart.items,
                }
                .finish(),
            ),
            Ok(_) => errors.push(format!("{platform_name}{name}：返回为空")),
            Err(error) => errors.push(format!("{platform_name}{name}：{error}")),
        }
    }
    FetchOutcome {
        boards,
        error: (!errors.is_empty()).then(|| errors.join("；")),
    }
}

/// 用上一次的快照补齐本轮没抓到的榜单：只补仍在抓取范围内（`wanted`）的榜，
/// 补回来的榜标记 `stale` 并保留原来的更新时间。返回补回的榜名。
pub fn fill_missing_boards(
    boards: &mut Vec<RankBoard>,
    previous: Option<&RankSnapshot>,
    wanted: impl Fn(&RankBoard) -> bool,
) -> Vec<String> {
    let mut kept = Vec::new();
    let Some(previous) = previous else {
        return kept;
    };
    for old in &previous.boards {
        if old.items.is_empty() || !wanted(old) || boards.iter().any(|board| board.key == old.key) {
            continue;
        }
        let mut stale = old.clone();
        stale.stale = true;
        kept.push(stale.name.clone());
        boards.push(stale);
    }
    kept
}

/// 刷新结果摘要，例如「已抓取 9 个榜单 450 条，其中 1 个沿用上次数据：QQ 音乐新歌榜」。
pub fn refresh_message(boards: &[RankBoard], stale: &[String]) -> String {
    let items: usize = boards.iter().map(|board| board.items.len()).sum();
    let mut message = format!("已抓取 {} 个榜单 {} 条", boards.len(), items);
    if !stale.is_empty() {
        message.push_str(&format!(
            "，其中 {} 个沿用上次数据：{}",
            stale.len(),
            stale.join("、")
        ));
    }
    message
}

// ---------------------------------------------------------------------------
// 猫眼 → 统一模型
// ---------------------------------------------------------------------------

/// 把猫眼的一个榜单快照转成统一榜单；`error` 为最近一次刷新失败的原因（此时数据是旧的）。
pub fn maoyan_board(response: MaoyanRankResponse, error: Option<&str>) -> RankBoard {
    let chart = response.chart;
    let platform = response.platform;
    let media_type = chart.media_type();
    let single = chart.platforms().len() == 1;
    let date = &response.show_date;
    let period = if date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()) {
        format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..])
    } else {
        date.clone()
    };
    RankBoard {
        key: format!("{}:{}", chart.as_str(), platform.as_str()),
        source: RankSource::Maoyan,
        name: if single || platform == MaoyanPlatform::All {
            chart.label().to_string()
        } else {
            format!("{} · {}", chart.label(), platform.label())
        },
        board_type: chart.as_str().to_string(),
        group: if single {
            "all".to_string()
        } else {
            platform.as_str().to_string()
        },
        group_name: if single {
            MaoyanPlatform::All.label().to_string()
        } else {
            platform.label().to_string()
        },
        kind: Some(media_type.as_str().to_string()),
        period: Some(period).filter(|period| !period.is_empty()),
        url: Some(chart.page_url(&response.show_date)),
        origin: chart.origin().to_string(),
        updated_at: response.fetched_at,
        stale: error.is_some(),
        count: 0,
        items: response
            .items
            .into_iter()
            .map(|item| RankItem {
                rank: item.rank,
                title: item.title,
                media_type: Some(media_type.clone()),
                poster: item
                    .tmdb
                    .as_ref()
                    .and_then(|media| media.poster_path.clone()),
                overview: item
                    .tmdb
                    .as_ref()
                    .map(|media| media.overview.clone())
                    .filter(|text| !text.is_empty()),
                year: item
                    .douban
                    .as_ref()
                    .and_then(|douban| douban.year)
                    .or_else(|| item.tmdb.as_ref().and_then(|media| media.year)),
                release_info: item.release_info,
                platform: item.platform,
                box_office: item.box_office,
                total_box_office: item.total_box_office,
                heat: item.heat,
                box_rate: item.box_rate,
                show_rate: item.show_rate,
                douban: item.douban,
                tmdb: item.tmdb,
                ..RankItem::default()
            })
            .collect(),
    }
    .finish()
}

/// 榜单在界面上的固定顺序（沿用旧数据补回的榜单也要回到原位）。
pub fn board_order(board: &RankBoard) -> usize {
    let position = |found: Option<usize>| found.unwrap_or(usize::MAX / 2);
    match board.source {
        RankSource::Maoyan => {
            let (chart, platform) = board.key.split_once(':').unwrap_or((&board.key, ""));
            let chart = position(
                MaoyanChart::ALL
                    .iter()
                    .position(|item| item.as_str() == chart),
            );
            let platform = MaoyanPlatform::ALL
                .iter()
                .position(|item| item.as_str() == platform)
                .unwrap_or(0);
            chart * 10 + platform
        }
        RankSource::Netflix => {
            let (region, category) = board.key.split_once(':').unwrap_or((&board.key, ""));
            let region = position(
                NETFLIX_REGIONS
                    .iter()
                    .position(|(code, _, _)| *code == region),
            );
            let category = match category {
                "tv_en" | "tv" => 0,
                "tv_other" => 1,
                "films_en" | "movie" => 2,
                _ => 3,
            };
            region * 10 + category
        }
        RankSource::Hongguo => position(
            HONGGUO_BOARDS
                .iter()
                .position(|(key, _, _)| *key == board.key),
        ),
        RankSource::Music => position(
            MUSIC_BOARDS
                .iter()
                .position(|item| board.key.split_once(':') == Some((item.0, item.2))),
        ),
    }
}

/// 查过豆瓣但没有评分的条目，隔多久再查一次（新片上线几天后才开分）。
const DOUBAN_RECHECK_DAYS: i64 = 3;

/// 条目是否需要（再）查一次豆瓣评分。
pub fn needs_douban_check(item: &RankItem, now: chrono::DateTime<Utc>) -> bool {
    if item.media_type.is_none()
        || item
            .douban
            .as_ref()
            .is_some_and(|douban| douban.rating.is_some())
    {
        return false;
    }
    match item
        .douban_checked_at
        .as_deref()
        .and_then(|checked| chrono::DateTime::parse_from_rfc3339(checked).ok())
    {
        Some(checked) => {
            now - checked.with_timezone(&Utc) >= chrono::Duration::days(DOUBAN_RECHECK_DAYS)
        }
        None => true,
    }
}

/// 片名里明确写出的季号，例如「庆余年 第二季」「某剧 第12季」「Show Season 3」。
pub fn explicit_season(title: &str) -> Option<u32> {
    fn chinese_number(text: &str) -> Option<u32> {
        if let Ok(number) = text.parse::<u32>() {
            return Some(number);
        }
        let digit = |character: char| {
            "一二三四五六七八九"
                .chars()
                .position(|item| item == character)
                .map(|index| index as u32 + 1)
        };
        let characters: Vec<char> = text
            .chars()
            .map(|character| if character == '两' { '二' } else { character })
            .collect();
        match characters.as_slice() {
            [single] if *single == '十' => Some(10),
            [single] => digit(*single),
            ['十', ones] => digit(*ones).map(|ones| 10 + ones),
            [tens, '十'] => digit(*tens).map(|tens| tens * 10),
            [tens, '十', ones] => Some(digit(*tens)? * 10 + digit(*ones)?),
            _ => None,
        }
    }
    if let Some(start) = title.rfind('第') {
        let rest = &title[start + '第'.len_utf8()..];
        if let Some(end) = rest.find('季') {
            return chinese_number(rest[..end].trim()).filter(|season| *season > 0);
        }
    }
    let lower = title.to_ascii_lowercase();
    let index = lower.rfind("season ")?;
    lower[index + 7..]
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok())
        .filter(|season| *season > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn regions(codes: &[&str]) -> Vec<String> {
        codes.iter().map(|code| code.to_string()).collect()
    }

    #[test]
    fn netflix_global_keeps_latest_week_and_structured_metrics() {
        let tsv = "week\tcategory\tweekly_rank\tshow_title\tseason_title\tweekly_hours_viewed\truntime\tweekly_views\tcumulative_weeks_in_top_10\n\
2026-11-01\tFilms (English)\t2\tReturn to Sender\tN/A\t5700000\t1.5833\t3600000\t1\n\
2026-11-01\tFilms (English)\t1\tAnimals\tN/A\t8400000\t1.8\t4700000\t4\n\
2026-11-01\tTV (Non-English)\t1\tSquid Game\tSquid Game: Season 3\t120000000\t6.2\t19400000\t2\n\
2026-10-25\tFilms (English)\t1\tOld\tN/A\t1\t1\t1\t1\n";
        let (week, rows) = parse_netflix_global(tsv);
        assert_eq!(week, "2026-11-01");
        assert_eq!(rows.len(), 3);
        let boards = netflix_boards(
            Some((week.clone(), rows.clone())),
            &HashMap::new(),
            &regions(&["global", "hk"]),
        );
        assert_eq!(boards.len(), 2);
        assert_eq!(boards[0].key, "global:films_en");
        assert_eq!(boards[0].board_type, "global");
        assert_eq!(boards[0].count, 2);
        assert_eq!(boards[0].period.as_deref(), Some("2026-11-01"));
        let animals = &boards[0].items[0];
        assert_eq!(animals.title, "Animals");
        assert_eq!(animals.views, Some(4_700_000));
        assert_eq!(animals.hours_viewed, Some(8_400_000));
        assert_eq!(animals.runtime_minutes, Some(108));
        assert_eq!(animals.weeks_in_top10, Some(4));
        assert!(boards[0].items[1].new_entry);
        assert_eq!(boards[1].kind.as_deref(), Some("tv"));
        assert_eq!(
            boards[1].items[0].season_title.as_deref(),
            Some("Squid Game: Season 3")
        );
        assert_eq!(
            boards[1].items[0].original_title.as_deref(),
            Some("Squid Game")
        );
        // 未勾选全球榜时不生成全球榜。
        assert!(netflix_boards(Some((week, rows)), &HashMap::new(), &regions(&["hk"])).is_empty());
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
        let boards = netflix_boards(None, &rows, &regions(&["global", "us", "hk"]));
        assert_eq!(
            boards
                .iter()
                .map(|board| board.key.as_str())
                .collect::<Vec<_>>(),
            vec!["hk:movie", "hk:tv"]
        );
        assert_eq!(
            boards[0].url.as_deref(),
            Some("https://www.netflix.com/tudum/top10/hong-kong")
        );
        assert_eq!(boards[1].items[0].weeks_in_top10, Some(3));
        assert_eq!(boards[1].items[0].views, None);
    }

    #[test]
    fn hongguo_payload_is_found_in_both_ssr_shapes() {
        let row = json!({"rank": 1, "title": "万妖图录传", "seriesId": "77", "cover": "https://p6-novel.byteimg.com/novel-pic/abc~tplv-shrink:640:0.image",
            "heatText": "11578万热度", "scoreText": "评分9.5", "favoriteText": "165.1万收藏", "likeText": "417.3万点赞",
            "tags": ["玄幻", "穿越"], "statusTags": ["新剧"], "description": "简介", "episodeCount": 202, "openEpisodeCount": 5,
            "isPaid": false, "playHref": "/player/77", "episodeVids": ["101", "102", "103", "104", "bad/../id"]});
        let direct = json!(["rank_hot-drama/page", "content", {"isSuccess": true, "rankList": [row.clone()]}]);
        let attr = serde_json::to_string(&direct)
            .unwrap()
            .replace('"', "&quot;");
        let html = format!(
            "<html><body><script data-fn-name=\"r\" data-fn-args='{attr}'></script></body></html>"
        );
        let payload = extract_hongguo_payload(&html).expect("direct shape");
        let items = parse_hongguo_items(&payload);
        let item = &items[0];
        assert_eq!(item.title, "万妖图录传");
        assert_eq!(
            item.poster.as_deref(),
            Some("https://p6-novel.byteimg.com/novel-pic/abc~tplv-shrink:1080:0.image")
        );
        assert_eq!(
            item.url.as_deref(),
            Some("https://hongguoduanju.com/detail?series_id=77")
        );
        assert_eq!(
            item.play_url.as_deref(),
            Some("https://hongguoduanju.com/player/77")
        );
        assert_eq!(item.heat.as_deref(), Some("11578万热度"));
        assert_eq!(item.score.as_deref(), Some("评分9.5"));
        assert_eq!(item.favorite.as_deref(), Some("165.1万收藏"));
        assert_eq!(item.likes.as_deref(), Some("417.3万点赞"));
        assert_eq!(item.tags, vec!["玄幻", "穿越"]);
        assert_eq!(item.status_tags, vec!["新剧"]);
        assert!(item.new_entry);
        assert_eq!(item.episode_count, Some(202));
        assert_eq!(item.open_episode_count, Some(5));
        assert_eq!(item.paid, Some(false));
        assert_eq!(item.series_id.as_deref(), Some("77"));
        assert_eq!(
            item.preview_urls,
            vec![
                "https://hongguoduanju.com/video/public/101/360p.mp4",
                "https://hongguoduanju.com/video/public/102/360p.mp4",
                "https://hongguoduanju.com/video/public/103/360p.mp4"
            ]
        );

        let nested = json!(["rank_hot-drama/page", [{"key": "content", "routerDataFnArgs": serde_json::to_string(&json!({"isSuccess": true, "rankList": [row]})).unwrap()}]]);
        let attr = serde_json::to_string(&nested)
            .unwrap()
            .replace('&', "&amp;")
            .replace('"', "&quot;");
        let html =
            format!("<script data-fn-name=\"mergeLoaderData\" data-fn-args=\"{attr}\"></script>");
        assert!(extract_hongguo_payload(&html).is_some());
    }

    #[test]
    fn music_parsers_keep_structured_fields() {
        let netease = json!({"result": {"name": "飙升榜", "updateTime": 1794184922479i64, "tracks": [
            {"id": 1, "name": "好坏", "artists": [{"name": "A"}, {"name": "B"}], "album": {"name": "专辑", "picUrl": "http://p2.music.126.net/x.jpg"}, "duration": 228239},
            {"id": 2, "name": "烂奇迹", "artists": [], "album": {}, "duration": 0, "lastRank": 0},
            {"id": 3, "name": "燃", "lastRank": 0}
        ]}});
        let chart = parse_netease(&netease, 50).unwrap();
        assert_eq!(chart.name.as_deref(), Some("飙升榜"));
        assert!(chart.period.is_some());
        let items = chart.items;
        assert!(items[0].new_entry);
        assert_eq!(items[0].artist.as_deref(), Some("A / B"));
        assert_eq!(items[0].album.as_deref(), Some("专辑"));
        assert_eq!(items[0].duration, Some(228));
        assert_eq!(items[0].song_id.as_deref(), Some("1"));
        assert_eq!(
            items[0].url.as_deref(),
            Some("https://music.163.com/#/song?id=1")
        );
        assert_eq!(
            items[0].poster.as_deref(),
            Some("https://p2.music.126.net/x.jpg?param=300y300")
        );
        assert_eq!(items[1].last_rank, Some(1));
        assert_eq!(items[1].rank_change, Some(-1));
        assert_eq!(items[1].duration, None);
        assert_eq!(items[2].rank_change, Some(-2));

        let qq = json!({"update_time": "2026-11-09", "topinfo": {"ListName": "巅峰榜·热歌"}, "songlist": [
            {"cur_count": "1", "old_count": "3", "data": {"songname": "奇迹", "albumname": "专辑", "albummid": "003EQ", "interval": 252, "singer": [{"name": "贺峻霖"}], "songmid": "abc"}},
            {"cur_count": "155303", "old_count": "0", "data": {"songname": "飙升值", "songmid": "def"}}
        ]});
        let chart = parse_qq(&qq, 50).unwrap();
        assert_eq!(chart.name.as_deref(), Some("热歌"));
        assert_eq!(chart.period.as_deref(), Some("2026-11-09"));
        assert_eq!(chart.items[0].duration, Some(252));
        assert_eq!(chart.items[0].last_rank, Some(3));
        assert_eq!(chart.items[0].rank_change, Some(2));
        assert_eq!(
            chart.items[0].url.as_deref(),
            Some("https://i.y.qq.com/n2/m/share/details/song.html?songmid=abc")
        );
        assert_eq!(
            chart.items[0].poster.as_deref(),
            Some("https://y.qq.com/music/photo_new/T002R300x300M000003EQ.jpg")
        );
        assert_eq!(chart.items[1].rank, 2);
        assert_eq!(chart.items[1].rank_change, None);
        assert!(!chart.items[1].new_entry);

        let apple = json!({"feed": {"updated": "Mon, 9 Nov 2026 10:00:00 +0000", "results": [{"id": "9", "name": "Song", "artistName": "Singer", "artworkUrl100": "https://is1.mzstatic.com/a/100x100bb.jpg", "url": "https://music.apple.com/x", "genres": [{"name": "流行"}, {"name": "音乐"}]}]}});
        let chart = parse_apple(&apple, 50).unwrap();
        assert_eq!(chart.period.as_deref(), Some("2026-11-09"));
        assert_eq!(
            chart.items[0].poster.as_deref(),
            Some("https://is1.mzstatic.com/a/300x300bb.jpg")
        );
        assert_eq!(chart.items[0].genre.as_deref(), Some("流行"));
        assert_eq!(chart.items[0].artist.as_deref(), Some("Singer"));
    }

    fn board(key: &str, board_type: &str, kind: Option<&str>, items: usize) -> RankBoard {
        RankBoard {
            key: key.to_string(),
            source: RankSource::Netflix,
            name: format!("榜 {key}"),
            board_type: board_type.to_string(),
            group: board_type.to_string(),
            group_name: String::new(),
            kind: kind.map(str::to_string),
            period: None,
            url: None,
            origin: String::new(),
            updated_at: "2026-11-09T00:00:00Z".to_string(),
            stale: false,
            count: 0,
            items: (1..=items)
                .map(|rank| RankItem {
                    rank,
                    title: format!("条目 {rank}"),
                    ..RankItem::default()
                })
                .collect(),
        }
        .finish()
    }

    #[test]
    fn stale_boards_are_kept_only_while_still_wanted() {
        let previous = RankSnapshot {
            version: SNAPSHOT_VERSION,
            source: RankSource::Netflix,
            fetched_at: "2026-11-08T00:00:00Z".to_string(),
            boards: vec![
                board("hk:tv", "hk", Some("tv"), 3),
                board("jp:tv", "jp", Some("tv"), 2),
                board("us:tv", "us", Some("tv"), 0),
            ],
        };
        let mut boards = vec![board("global:tv_en", "global", Some("tv"), 10)];
        let kept = fill_missing_boards(&mut boards, Some(&previous), |board| {
            board.board_type != "jp"
        });
        assert_eq!(kept, vec!["榜 hk:tv"]);
        assert_eq!(boards.len(), 2);
        assert!(boards[1].stale);
        assert_eq!(boards[1].updated_at, "2026-11-09T00:00:00Z");
        assert_eq!(
            refresh_message(&boards, &kept),
            "已抓取 2 个榜单 13 条，其中 1 个沿用上次数据：榜 hk:tv"
        );
    }

    #[test]
    fn filter_matches_plugin_rank_parameters() {
        let boards = vec![
            board("global:tv_en", "global", Some("tv"), 10),
            board("hk:movie", "hk", Some("movie"), 10),
            board("hk:tv", "hk", Some("tv"), 10),
        ];
        let filter = RankFilter {
            board_type: Some("hk, jp".to_string()),
            kind: Some("film".to_string()),
            limit: Some(3),
            ..RankFilter::default()
        };
        let found = filter.apply(boards.clone());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "hk:movie");
        assert_eq!(found[0].count, 3);
        let by_name = RankFilter {
            board: Some("global".to_string()),
            ..RankFilter::default()
        };
        assert_eq!(by_name.apply(boards).len(), 1);
    }

    #[test]
    fn settings_are_normalized_and_drive_maoyan_scope() {
        let mut settings = RankSettings::default();
        settings.netflix.regions = regions(&["sg", "xx", "global"]);
        settings.hongguo.boards = regions(&["ai", "nope", "hot"]);
        settings.hongguo.limit = 55;
        settings.music.limit = 1000;
        settings.maoyan.subscribe_tv = 999;
        settings.maoyan.platforms = vec![
            MaoyanPlatform::All,
            MaoyanPlatform::Iqiyi,
            MaoyanPlatform::Iqiyi,
        ];
        settings.maoyan.subscribe_mango = 2;
        settings.subscribe.movie_save_path = Some("  ".to_string());
        let settings = settings.normalized();
        assert_eq!(settings.netflix.regions, regions(&["global", "sg"]));
        assert_eq!(settings.hongguo.boards, regions(&["hot", "ai"]));
        assert_eq!(settings.hongguo.limit, 60);
        assert_eq!(settings.music.limit, 100);
        assert_eq!(settings.maoyan.subscribe_tv, 30);
        assert_eq!(settings.maoyan.platforms, vec![MaoyanPlatform::Iqiyi]);
        assert_eq!(settings.subscribe.movie_save_path, None);

        let maoyan = &settings.maoyan;
        assert!(maoyan.wants(MaoyanChart::Movie, MaoyanPlatform::All));
        assert!(maoyan.wants(MaoyanChart::Tv, MaoyanPlatform::Iqiyi));
        assert!(!maoyan.wants(MaoyanChart::Tv, MaoyanPlatform::Tencent));
        // 为平台配置了自动订阅条数时，即使没勾选平台专项榜也要抓剧集综合榜。
        assert!(maoyan.wants(MaoyanChart::WebAll, MaoyanPlatform::Mango));
        assert!(!maoyan.wants(MaoyanChart::Tv, MaoyanPlatform::Mango));
        assert_eq!(
            maoyan.subscribe_count(MaoyanChart::WebAll, MaoyanPlatform::All),
            30
        );
        assert_eq!(
            maoyan.subscribe_count(MaoyanChart::Variety, MaoyanPlatform::Mango),
            2
        );
        assert_eq!(
            maoyan.subscribe_count(MaoyanChart::Total, MaoyanPlatform::All),
            0
        );

        // 旧配置缺字段时使用默认值。
        let parsed: RankSettings =
            serde_json::from_str(r#"{"netflix": {"enabled": false}}"#).unwrap();
        assert!(!parsed.netflix.enabled);
        assert_eq!(parsed.netflix.regions.len(), NETFLIX_REGIONS.len());
        assert!(parsed.maoyan.enabled && parsed.auto_refresh);
    }

    #[test]
    fn poster_proxy_only_allows_known_image_hosts() {
        assert!(poster_url_allowed(
            "https://p6-novel.byteimg.com/novel-pic/abc~tplv-shrink:1080:0.image"
        ));
        assert!(poster_url_allowed(
            "https://is1-ssl.mzstatic.com/image/thumb/x/300x300bb.jpg"
        ));
        assert!(poster_url_allowed(
            "https://p2.music.126.net/x.jpg?param=300y300"
        ));
        assert!(poster_url_allowed(
            "https://y.qq.com/music/photo_new/T002R300x300M000003EQ.jpg"
        ));
        assert!(!poster_url_allowed(
            "http://p6-novel.byteimg.com/novel-pic/abc"
        ));
        assert!(!poster_url_allowed("https://evilbyteimg.com/novel-pic/abc"));
        assert!(!poster_url_allowed(
            "https://byteimg.com.evil.example/novel-pic/abc"
        ));
        assert!(!poster_url_allowed(
            "https://p6-novel.byteimg.com:8443/novel-pic/abc"
        ));
        assert!(!poster_url_allowed("https://127.0.0.1/novel-pic/abc"));
        assert_eq!(image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_mime(b"<html>"), None);
    }

    #[test]
    fn explicit_season_reads_chinese_and_english_numbering() {
        assert_eq!(explicit_season("庆余年 第二季"), Some(2));
        assert_eq!(explicit_season("某剧第12季"), Some(12));
        assert_eq!(explicit_season("某剧 第十季"), Some(10));
        assert_eq!(explicit_season("某剧 第十二季"), Some(12));
        assert_eq!(explicit_season("某剧 第二十一季"), Some(21));
        assert_eq!(explicit_season("Squid Game: Season 3"), Some(3));
        assert_eq!(explicit_season("第一次亲密接触"), None);
        assert_eq!(explicit_season("小城良方"), None);
    }

    #[test]
    fn maoyan_boards_keep_structured_metrics_and_stale_flag() {
        let response: MaoyanRankResponse = serde_json::from_value(json!({
            "chart": "web_all", "platform": "tencent", "show_date": "20261109", "fetched_at": "2026-11-09T10:00:00Z",
            "items": [{"rank": 1, "title": "小城良方", "release_info": "上线16天", "platform": "爱奇艺独播", "heat": "6517.03",
                "douban": {"id": "1", "title": "小城良方", "year": 2026, "rating": 7.9, "poster_id": null, "url": "https://movie.douban.com/subject/1/", "media_type": "tv"},
                "tmdb": null}]
        }))
        .unwrap();
        let board = maoyan_board(response, Some("猫眼返回的榜单为空"));
        assert_eq!(board.key, "web_all:tencent");
        assert_eq!(board.name, "剧集综合 · 腾讯视频");
        assert_eq!(board.board_type, "web_all");
        assert_eq!(board.group, "tencent");
        assert_eq!(board.period.as_deref(), Some("2026-11-09"));
        assert_eq!(board.origin, "猫眼专业版");
        assert!(board.stale);
        assert_eq!(board.count, 1);
        let item = &board.items[0];
        assert_eq!(item.heat.as_deref(), Some("6517.03"));
        assert_eq!(item.year, Some(2026));
        assert_eq!(
            item.douban.as_ref().and_then(|douban| douban.rating),
            Some(7.9)
        );
        assert!(!needs_douban_check(item, Utc::now()));

        let mut unrated = RankItem {
            media_type: Some(TmdbMediaType::Tv),
            ..RankItem::default()
        };
        assert!(needs_douban_check(&unrated, Utc::now()));
        unrated.douban_checked_at = Some(Utc::now().to_rfc3339());
        assert!(!needs_douban_check(&unrated, Utc::now()));
        assert!(needs_douban_check(
            &unrated,
            Utc::now() + chrono::Duration::days(4)
        ));
        // 音乐、短剧没有影视类型，不查豆瓣。
        assert!(!needs_douban_check(&RankItem::default(), Utc::now()));

        let mut order = vec![board.clone()];
        order[0].key = "movie:all".to_string();
        assert!(board_order(&order[0]) < board_order(&board));
    }
}
