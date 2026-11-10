//! 全球榜单的服务层：设置、各数据源的刷新、统一视图、豆瓣补评分与猫眼榜单自动订阅。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::Utc;

use super::{CreateSubscriptionRequest, MediaService, MediaServiceError};
use crate::media::douban::{DoubanClient, DoubanError, DoubanSubject};
use crate::media::global_rank::{
    self, FetchOutcome, RankBoard, RankFilter, RankItem, RankSettings, RankSnapshot, RankSource,
    RankSourceSummary, RankSourceView, RankSubscribeRecord, SNAPSHOT_VERSION,
};
use crate::media::maoyan::{self, MaoyanChart, MaoyanPlatform};
use crate::media::tmdb::{TmdbMedia, TmdbMediaType};
use crate::net::client_factory;

const SETTINGS_KEY: &str = "settings";
const HISTORY_KEY: &str = "subscribe_history";
/// 自动订阅记录保留的条数。
const HISTORY_LIMIT: usize = 500;
/// 单轮最多查询多少个片名的豆瓣评分；超出的留到下一轮（约每 1.2 秒一个请求）。
const DOUBAN_ROUND_BUDGET: usize = 150;
/// 补评分期间每查完多少个片名把结果写回快照一次，界面可以逐步看到评分。
const DOUBAN_PUBLISH_EVERY: usize = 12;
/// 猫眼条目每轮最多补查多少部的 TMDB 评分与主演、多少个片名的豆瓣评分。
const MAOYAN_TMDB_BUDGET: usize = 200;
const MAOYAN_DOUBAN_BUDGET: usize = 60;
/// 评分会随时间变化，补充信息超过这么多天重新查询。
const EXTRAS_RECHECK_DAYS: i64 = 3;
/// 猫眼条目展示的主演人数。
const CAST_SIZE: usize = 4;

type MaoyanParsed = (MaoyanChart, MaoyanPlatform, String, Vec<maoyan::MaoyanRankItem>);

fn extras_due(checked: Option<&str>, now: chrono::DateTime<Utc>) -> bool {
    match checked.and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok()) {
        Some(checked) => now - checked.with_timezone(&Utc) >= chrono::Duration::days(EXTRAS_RECHECK_DAYS),
        None => true,
    }
}

fn has_douban_rating(item: &maoyan::MaoyanRankItem) -> bool {
    item.douban.as_ref().is_some_and(|douban| douban.rating.is_some())
}

/// 刷新请求的结果，对应 GlobalRankPro 的 `RankSourceRefresh`。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RankRefreshStatus {
    pub source: RankSource,
    /// 是否已在后台刷新（包括之前就在进行的刷新）
    pub refreshing: bool,
    pub message: String,
}

impl MediaService {
    // ---- 猫眼条目补充信息 ----

    /// 给猫眼条目补上评分（豆瓣、TMDB）和主演。之前查到的直接沿用，到期才重查；
    /// 豆瓣限流时停止，剩下的下一轮再补。
    pub(super) async fn enrich_maoyan_items(
        &self,
        http: &reqwest::Client,
        parsed: &mut [MaoyanParsed],
    ) {
        use futures::StreamExt;

        let now = Utc::now();
        let previous = self.maoyan.previous_extras().await;
        for (chart, _, _, items) in parsed.iter_mut() {
            let kind = chart.media_type().as_str();
            for item in items.iter_mut() {
                let Some(old) = previous.get(&(item.title.clone(), kind)) else {
                    continue;
                };
                let same_tmdb = old.tmdb.as_ref().map(|media| media.tmdb_id)
                    == item.tmdb.as_ref().map(|media| media.tmdb_id);
                if same_tmdb {
                    item.cast = old.cast.clone();
                    item.tmdb_rating = old.tmdb_rating;
                    item.tmdb_checked_at = old.tmdb_checked_at.clone();
                }
                if !has_douban_rating(item) {
                    if has_douban_rating(old) {
                        item.douban = old.douban.clone();
                    } else {
                        item.douban_checked_at = old.douban_checked_at.clone();
                    }
                }
            }
        }

        // TMDB 评分与主演：一部作品一次请求。
        let mut wanted = Vec::new();
        let mut seen = HashSet::new();
        for (_, _, _, items) in parsed.iter() {
            for item in items {
                if let Some(media) = &item.tmdb
                    && extras_due(item.tmdb_checked_at.as_deref(), now)
                    && seen.insert((media.tmdb_id, media.media_type.as_str()))
                {
                    wanted.push((media.tmdb_id, media.media_type.clone()));
                }
            }
        }
        wanted.truncate(MAOYAN_TMDB_BUDGET);
        let mut tmdb_checked = 0usize;
        if !wanted.is_empty()
            && let Ok(tmdb) = self.tmdb_client().await
        {
            let tmdb = &tmdb;
            let fetched: Vec<_> = futures::stream::iter(wanted)
                .map(|(id, media_type)| async move {
                    let result = tmdb.extras(id, media_type.clone(), CAST_SIZE).await;
                    ((id, media_type.as_str()), result)
                })
                .buffer_unordered(6)
                .collect()
                .await;
            let stamp = Utc::now().to_rfc3339();
            let fetched: HashMap<_, _> = fetched
                .into_iter()
                .filter_map(|(key, result)| result.ok().map(|extras| (key, extras)))
                .collect();
            tmdb_checked = fetched.len();
            for (_, _, _, items) in parsed.iter_mut() {
                for item in items.iter_mut() {
                    let Some(media) = &item.tmdb else { continue };
                    if let Some(extras) = fetched.get(&(media.tmdb_id, media.media_type.as_str())) {
                        item.cast = extras.cast.clone();
                        item.tmdb_rating = extras.rating;
                        item.tmdb_checked_at = Some(stamp.clone());
                    }
                }
            }
        }

        // 豆瓣评分：匹配阶段没拿到评分的片名，用中文片名再搜一次。
        let mut pending: Vec<(String, TmdbMediaType)> = Vec::new();
        let mut seen = HashSet::new();
        for (chart, _, _, items) in parsed.iter() {
            let media_type = chart.media_type();
            for item in items {
                if !has_douban_rating(item)
                    && extras_due(item.douban_checked_at.as_deref(), now)
                    && seen.insert((item.title.clone(), media_type.as_str()))
                {
                    pending.push((item.title.clone(), media_type.clone()));
                }
            }
        }
        let douban = DoubanClient::new(http.clone());
        let mut douban_checked = 0usize;
        let mut douban_rated = 0usize;
        let mut blocked = false;
        for (title, media_type) in pending.into_iter().take(MAOYAN_DOUBAN_BUDGET) {
            let found = match douban.search_rated(&title).await {
                Ok(subjects) => maoyan::pick_douban(&title, &media_type, subjects),
                Err(DoubanError::RateLimited) => {
                    blocked = true;
                    break;
                }
                Err(error) => {
                    tracing::debug!(%error, title, "maoyan douban rating lookup failed");
                    continue;
                }
            };
            douban_checked += 1;
            if found.as_ref().is_some_and(|subject| subject.rating.is_some()) {
                douban_rated += 1;
            }
            let stamp = Utc::now().to_rfc3339();
            for (chart, _, _, items) in parsed.iter_mut() {
                if chart.media_type() != media_type {
                    continue;
                }
                for item in items.iter_mut().filter(|item| item.title == title) {
                    if found.is_some() {
                        item.douban = found.clone();
                    }
                    item.douban_checked_at = Some(stamp.clone());
                }
            }
        }
        tracing::info!(
            tmdb_checked,
            douban_checked,
            douban_rated,
            douban_blocked = blocked,
            "maoyan ratings and cast enriched"
        );
    }

    // ---- 设置 ----

    pub async fn rank_settings(&self) -> RankSettings {
        if let Some(settings) = self.global_rank.cached_settings().await {
            return settings;
        }
        let settings = match self.db.get_global_rank_state(SETTINGS_KEY).await {
            Ok(Some(payload)) => serde_json::from_str::<RankSettings>(&payload)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "invalid global rank settings; using defaults");
                    RankSettings::default()
                })
                .normalized(),
            Ok(None) => RankSettings::default(),
            Err(error) => {
                tracing::warn!(%error, "failed to load global rank settings");
                // 读取失败时不缓存，下次再试。
                return RankSettings::default();
            }
        };
        self.global_rank.store_settings(settings.clone()).await;
        settings
    }

    /// 保存设置；抓取范围变化的数据源随后在后台刷新一次。
    pub async fn update_rank_settings(
        &self,
        settings: RankSettings,
    ) -> Result<RankSettings, MediaServiceError> {
        let settings = settings.normalized();
        let defaults = &settings.subscribe;
        if let Some(id) = defaults.quality_profile_id
            && self.db.get_quality_profile(id).await?.is_none()
        {
            return Err(MediaServiceError::NotFound(format!("quality profile {id}")));
        }
        if let Some(id) = defaults.downloader_id
            && self.db.get_downloader(id).await?.is_none()
        {
            return Err(MediaServiceError::NotFound(format!("downloader {id}")));
        }
        if settings.maoyan.any_subscribe()
            && (defaults.quality_profile_id.is_none() || defaults.downloader_id.is_none())
        {
            return Err(MediaServiceError::Invalid(
                "开启自动订阅前，需要先选择订阅使用的质量配置和下载器".to_string(),
            ));
        }
        let previous = self.rank_settings().await;
        let payload = serde_json::to_string(&settings)
            .map_err(|error| MediaServiceError::Serialization(error.to_string()))?;
        self.db.set_global_rank_state(SETTINGS_KEY, payload).await?;
        self.global_rank.store_settings(settings.clone()).await;

        // 只有“需要更多数据”的变化才触发抓取；关掉榜单由视图过滤，立即生效。
        let maoyan_grew = settings.maoyan.enabled
            && (!previous.maoyan.enabled
                || MaoyanChart::ALL.into_iter().any(|chart| {
                    chart.platforms().iter().any(|platform| {
                        settings.maoyan.wants(chart, *platform)
                            && !previous.maoyan.wants(chart, *platform)
                    })
                }));
        if maoyan_grew {
            self.spawn_maoyan_refresh();
        }
        let grew =
            |now: &[String], before: &[String]| now.iter().any(|item| !before.contains(item));
        let netflix_grew = settings.netflix.enabled
            && (!previous.netflix.enabled
                || grew(&settings.netflix.regions, &previous.netflix.regions));
        let hongguo_grew = settings.hongguo.enabled
            && (!previous.hongguo.enabled
                || grew(&settings.hongguo.boards, &previous.hongguo.boards)
                || settings.hongguo.limit > previous.hongguo.limit);
        let music_grew = settings.music.enabled
            && (!previous.music.enabled
                || grew(&settings.music.boards, &previous.music.boards)
                || settings.music.limit > previous.music.limit);
        for (source, changed) in [
            (RankSource::Netflix, netflix_grew),
            (RankSource::Hongguo, hongguo_grew),
            (RankSource::Music, music_grew),
        ] {
            if changed {
                self.spawn_forced_rank_refresh(source);
            }
        }
        Ok(settings)
    }

    // ---- 视图 ----

    pub(super) async fn ensure_global_rank_loaded(&self) {
        let db = self.db.clone();
        self.global_rank
            .load_once(|| async move {
                match db.list_global_rank_snapshots().await {
                    Ok(rows) => rows
                        .into_iter()
                        .filter_map(|(_, payload)| {
                            serde_json::from_str::<RankSnapshot>(&payload).ok()
                        })
                        .collect(),
                    Err(error) => {
                        tracing::warn!(%error, "failed to load global rank snapshots");
                        Vec::new()
                    }
                }
            })
            .await;
    }

    /// 某个数据源当前要展示的全部榜单（已按设置过滤）。不触发抓取。
    async fn rank_source_view(
        &self,
        source: RankSource,
        settings: &RankSettings,
    ) -> RankSourceView {
        let enabled = settings.enabled(source);
        if source == RankSource::Maoyan {
            return self.maoyan_rank_view(settings).await;
        }
        self.ensure_global_rank_loaded().await;
        let mut view = self.global_rank.view(source, enabled).await;
        if !enabled {
            view.boards.clear();
            return view;
        }
        view.boards.retain(|board| board_wanted(board, settings));
        if source == RankSource::Hongguo {
            for board in &mut view.boards {
                board.items.truncate(settings.hongguo.limit);
            }
        }
        if source == RankSource::Music {
            for board in &mut view.boards {
                board.items.truncate(settings.music.limit);
            }
        }
        view.boards = view.boards.into_iter().map(RankBoard::finish).collect();
        view
    }

    async fn maoyan_rank_view(&self, settings: &RankSettings) -> RankSourceView {
        self.ensure_maoyan_loaded().await;
        let scope = &settings.maoyan;
        let mut boards: Vec<RankBoard> = self
            .maoyan
            .all()
            .await
            .into_iter()
            .filter(|(entry, _)| {
                scope.wants(entry.chart, entry.platform) && !entry.items.is_empty()
            })
            .map(|(entry, error)| global_rank::maoyan_board(entry, error.as_deref()))
            .collect();
        boards.sort_by_key(global_rank::board_order);
        let mut pending: Vec<(MaoyanChart, MaoyanPlatform, String)> = self
            .maoyan
            .pending_errors()
            .await
            .into_iter()
            .filter(|(chart, platform, _)| scope.wants(*chart, *platform))
            .collect();
        pending.sort_by_key(|(chart, platform, _)| {
            (
                MaoyanChart::ALL.iter().position(|item| item == chart),
                MaoyanPlatform::ALL.iter().position(|item| item == platform),
            )
        });
        // 同一个原因（例如没有配置云浏览器）会在每个热度榜上重复一遍，合并后再展示。
        let mut reasons: Vec<(String, Vec<String>)> = Vec::new();
        for (chart, platform, error) in pending {
            let name = if platform == MaoyanPlatform::All || chart.platforms().len() == 1 {
                chart.label().to_string()
            } else {
                format!("{}（{}）", chart.label(), platform.label())
            };
            match reasons.iter_mut().find(|(reason, _)| *reason == error) {
                Some((_, names)) => names.push(name),
                None => reasons.push((error, vec![name])),
            }
        }
        let error = (!reasons.is_empty()).then(|| {
            reasons
                .into_iter()
                .map(|(reason, names)| {
                    let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join("、");
                    let more = if names.len() > 3 {
                        format!(" 等 {} 个榜单", names.len())
                    } else {
                        String::new()
                    };
                    format!("{shown}{more}：{reason}")
                })
                .collect::<Vec<_>>()
                .join("；")
        });
        RankSourceView {
            source: RankSource::Maoyan,
            enabled: scope.enabled,
            fetched_at: boards.iter().map(|board| board.updated_at.clone()).max(),
            boards: if scope.enabled { boards } else { Vec::new() },
            refreshing: self.maoyan.is_refreshing(),
            error,
            message: self.global_rank.message(RankSource::Maoyan).await,
        }
    }

    /// 全球榜单：返回后台快照；已启用但尚无数据时触发一次后台刷新，前端轮询等待。
    pub async fn global_rank(&self, source: RankSource, filter: &RankFilter) -> RankSourceView {
        let settings = self.rank_settings().await;
        let mut view = self.rank_source_view(source, &settings).await;
        if view.enabled && view.boards.is_empty() && !view.refreshing && view.error.is_none() {
            view.refreshing = self.spawn_global_rank_refresh(source);
        }
        view.boards = filter.apply(std::mem::take(&mut view.boards));
        view
    }

    /// 数据源总览：每个来源的榜单数、条目数、更新时间与状态。
    pub async fn global_rank_overview(&self) -> Vec<RankSourceSummary> {
        let settings = self.rank_settings().await;
        let mut summaries = Vec::new();
        for source in RankSource::ALL {
            let view = self.rank_source_view(source, &settings).await;
            summaries.push(RankSourceSummary::from_view(&view));
        }
        summaries
    }

    // ---- 刷新 ----

    /// 在后台刷新一个数据源；已有刷新在进行时不重复启动。返回刷新是否在进行。
    pub fn spawn_global_rank_refresh(&self, source: RankSource) -> bool {
        if source == RankSource::Maoyan {
            return self.spawn_maoyan_refresh();
        }
        if self.global_rank.is_refreshing(source) {
            return true;
        }
        let service = self.clone();
        tokio::spawn(async move {
            service.refresh_global_rank(source).await;
        });
        true
    }

    /// 设置变化后的刷新：即使数据周期没变（Netflix 同一周）也重新抓取。
    fn spawn_forced_rank_refresh(&self, source: RankSource) {
        let service = self.clone();
        tokio::spawn(async move {
            service.refresh_global_rank_with(source, true).await;
        });
    }

    /// 手动刷新：`None` 表示全部已启用的数据源。
    pub async fn request_rank_refresh(&self, source: Option<RankSource>) -> Vec<RankRefreshStatus> {
        let settings = self.rank_settings().await;
        let targets: Vec<RankSource> = match source {
            Some(source) => vec![source],
            None => RankSource::ALL.to_vec(),
        };
        targets
            .into_iter()
            .map(|source| {
                if !settings.enabled(source) {
                    return RankRefreshStatus {
                        source,
                        refreshing: false,
                        message: "数据源未启用，跳过抓取".to_string(),
                    };
                }
                let already = match source {
                    RankSource::Maoyan => self.maoyan.is_refreshing(),
                    other => self.global_rank.is_refreshing(other),
                };
                if source == RankSource::Maoyan {
                    self.spawn_maoyan_refresh();
                } else if !already {
                    self.spawn_forced_rank_refresh(source);
                }
                RankRefreshStatus {
                    source,
                    refreshing: true,
                    message: if already {
                        "正在刷新中".to_string()
                    } else {
                        "已开始刷新".to_string()
                    },
                }
            })
            .collect()
    }

    pub(super) async fn refresh_global_rank(&self, source: RankSource) {
        self.refresh_global_rank_with(source, false).await;
    }

    async fn refresh_global_rank_with(&self, source: RankSource, force: bool) {
        self.ensure_global_rank_loaded().await;
        let settings = self.rank_settings().await;
        if !settings.enabled(source) {
            return;
        }
        let Some(_guard) = self.global_rank.try_begin_refresh(source) else {
            return;
        };
        self.global_rank.mark_attempt(source).await;
        let started = Instant::now();
        let http = match self
            .db
            .get_settings()
            .await
            .map_err(|error| error.to_string())
            .and_then(|config| {
                client_factory::build_client(config.proxy.as_deref())
                    .map_err(|error| error.to_string())
            }) {
            Ok(http) => http,
            Err(error) => {
                tracing::warn!(source = source.as_str(), %error, "global rank refresh skipped");
                self.global_rank.set_error(source, error).await;
                return;
            }
        };
        let previous = self.global_rank.snapshot(source).await;
        let outcome = match source {
            RankSource::Netflix => match self
                .fetch_netflix(&http, &settings, previous.as_ref(), force)
                .await
            {
                Ok(Some(outcome)) => outcome,
                // 数据未更新（同一周且身份都已补全），保留现有快照。
                Ok(None) => return,
                Err(error) => FetchOutcome {
                    error: Some(error),
                    ..FetchOutcome::default()
                },
            },
            RankSource::Hongguo => {
                global_rank::fetch_hongguo_boards(&http, &settings.hongguo).await
            }
            RankSource::Music => global_rank::fetch_music_boards(&http, &settings.music).await,
            RankSource::Maoyan => return,
        };
        let FetchOutcome {
            mut boards, error, ..
        } = outcome;
        if boards.is_empty() {
            // 整轮一条都没抓到：保留旧快照和它的更新时间，只记录失败原因。
            let error = error.unwrap_or_else(|| "没有获取到任何榜单".to_string());
            tracing::warn!(source = source.as_str(), %error, "global rank refresh failed");
            self.global_rank
                .set_message(
                    source,
                    "本次未抓取到榜单（数据源异常或网络不可用）".to_string(),
                )
                .await;
            self.global_rank.set_error(source, error).await;
            return;
        }
        // 单个榜单失败时沿用上一次的数据，避免该榜在页面上消失。
        let stale = if error.is_some() {
            global_rank::fill_missing_boards(&mut boards, previous.as_ref(), |board| {
                board_wanted(board, &settings)
            })
        } else {
            Vec::new()
        };
        boards.sort_by_key(global_rank::board_order);
        let message = global_rank::refresh_message(&boards, &stale);
        let board_count = boards.len();
        self.publish_rank_snapshot(source, boards.clone()).await;
        self.global_rank.set_message(source, message).await;
        if let Some(error) = error {
            self.global_rank.set_error(source, error).await;
        }
        tracing::info!(
            source = source.as_str(),
            boards = board_count,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "global rank refreshed"
        );
        if source == RankSource::Netflix {
            // 榜单先发布，评分在同一轮刷新里慢慢补（刷新标志保持，界面继续轮询）。
            self.fill_netflix_ratings(&http, boards).await;
        }
    }

    async fn publish_rank_snapshot(&self, source: RankSource, boards: Vec<RankBoard>) {
        let snapshot = RankSnapshot {
            version: SNAPSHOT_VERSION,
            source,
            fetched_at: Utc::now().to_rfc3339(),
            boards,
        };
        let error = self.global_rank.error(source).await;
        match serde_json::to_string(&snapshot) {
            Ok(payload) => {
                if let Err(error) = self
                    .db
                    .save_global_rank_snapshot(
                        source.as_str().to_string(),
                        payload,
                        snapshot.fetched_at.clone(),
                    )
                    .await
                {
                    tracing::warn!(%error, "failed to persist global rank snapshot");
                }
            }
            Err(error) => tracing::warn!(%error, "failed to serialize global rank snapshot"),
        }
        self.global_rank.put(snapshot).await;
        // put 会清掉错误；补评分期间的中途发布要保留“部分榜单失败”的提示。
        if let Some(error) = error {
            self.global_rank.set_error(source, error).await;
        }
    }

    /// Netflix：全球榜文件很小，先取它判断是否发布了新一周；没有新数据时不再下载约 30MB 的地区榜。
    async fn fetch_netflix(
        &self,
        http: &reqwest::Client,
        settings: &RankSettings,
        previous: Option<&RankSnapshot>,
        force: bool,
    ) -> Result<Option<FetchOutcome>, String> {
        let regions = &settings.netflix.regions;
        if regions.is_empty() {
            return Err("没有选择任何 Netflix 榜单地区".to_string());
        }
        let global = global_rank::fetch_netflix_global(http).await?;
        let week = global.0.clone();
        if let Some(previous) = previous {
            let covered = regions.iter().all(|region| {
                previous.boards.iter().any(|board| {
                    &board.group == region
                        && !board.stale
                        && board.period.as_deref() == Some(week.as_str())
                })
            });
            if covered {
                // 同一周不重新下载地区榜；之前没匹配上 TMDB 或还没查到评分的条目再补一次。
                let now = Utc::now();
                let items = || previous.boards.iter().flat_map(|board| &board.items);
                let unmatched =
                    items().any(|item| item.tmdb.is_none()) && self.tmdb_client().await.is_ok();
                let unrated = items().any(|item| global_rank::needs_douban_check(item, now));
                if !force && !unmatched && !unrated {
                    return Ok(None);
                }
                let mut boards: Vec<RankBoard> = previous
                    .boards
                    .iter()
                    .filter(|board| board_wanted(board, settings))
                    .cloned()
                    .collect();
                self.match_netflix_titles(&mut boards).await;
                return Ok(Some(FetchOutcome {
                    boards,
                    ..FetchOutcome::default()
                }));
            }
        }
        let needs_countries = regions.iter().any(|region| region != "global");
        let (countries, error) = if needs_countries {
            match global_rank::fetch_netflix_countries(http, regions).await {
                Ok(countries) => (countries, None),
                Err(error) => (HashMap::new(), Some(format!("Netflix 地区榜：{error}"))),
            }
        } else {
            (HashMap::new(), None)
        };
        let mut boards = global_rank::netflix_boards(Some(global), &countries, regions);
        self.match_netflix_titles(&mut boards).await;
        Ok(Some(FetchOutcome { boards, error }))
    }

    /// 按英文片名匹配 TMDB，标题换成 TMDB 的本地化名称以便直接订阅；
    /// 同时带上之前查到的豆瓣评分。
    async fn match_netflix_titles(&self, boards: &mut [RankBoard]) {
        use futures::StreamExt;

        let previous = self
            .global_rank
            .previous_identities(RankSource::Netflix)
            .await;
        for item in boards.iter_mut().flat_map(|board| &mut board.items) {
            let (Some(original), Some(media_type)) = (&item.original_title, &item.media_type)
            else {
                continue;
            };
            if let Some(identity) = previous.get(&(original.clone(), media_type.as_str())) {
                if item.douban.is_none() {
                    item.douban = identity.douban.clone();
                }
                if item.douban_checked_at.is_none() {
                    item.douban_checked_at = identity.douban_checked_at.clone();
                }
                if item.tmdb.is_none()
                    && let Some(media) = &identity.tmdb
                {
                    apply_tmdb(item, media);
                }
            }
        }
        let Ok(tmdb) = self.tmdb_client().await else {
            return;
        };
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for item in boards.iter().flat_map(|board| &board.items) {
            if item.tmdb.is_none()
                && let (Some(original), Some(media_type)) = (&item.original_title, &item.media_type)
                && seen.insert((original.clone(), media_type.as_str()))
            {
                unique.push((original.clone(), media_type.clone()));
            }
        }
        let store = &self.global_rank;
        let tmdb = &tmdb;
        let found: HashMap<(String, &'static str), Option<TmdbMedia>> =
            futures::stream::iter(unique)
                .map(|(title, media_type)| async move {
                    if let Some(cached) = store.cached_match(&title, &media_type).await {
                        return ((title, media_type.as_str()), cached);
                    }
                    match tmdb.search_typed(&title, &media_type, None).await {
                        Ok(results) => {
                            let picked = maoyan::pick_tmdb_match(&title, results);
                            store
                                .store_match(title.clone(), &media_type, picked.clone())
                                .await;
                            ((title, media_type.as_str()), picked)
                        }
                        Err(error) => {
                            tracing::debug!(%error, title, "netflix tmdb lookup failed");
                            ((title, media_type.as_str()), None)
                        }
                    }
                })
                .buffer_unordered(6)
                .collect()
                .await;
        for item in boards.iter_mut().flat_map(|board| &mut board.items) {
            let (Some(original), Some(media_type)) = (&item.original_title, &item.media_type)
            else {
                continue;
            };
            if let Some(Some(media)) = found.get(&(original.clone(), media_type.as_str())) {
                apply_tmdb(item, media);
            }
        }
    }

    /// 给 Netflix 条目补豆瓣评分：按片名去重后逐个查询，结果分批写回快照。
    /// 豆瓣限流时立即停手，没查到的条目下一轮再补。
    async fn fill_netflix_ratings(&self, http: &reqwest::Client, mut boards: Vec<RankBoard>) {
        let now = Utc::now();
        let mut pending: Vec<(String, TmdbMediaType)> = Vec::new();
        let mut seen = HashSet::new();
        for item in boards.iter().flat_map(|board| &board.items) {
            if global_rank::needs_douban_check(item, now)
                && let (Some(original), Some(media_type)) = (&item.original_title, &item.media_type)
                && seen.insert((original.clone(), media_type.as_str()))
            {
                pending.push((original.clone(), media_type.clone()));
            }
        }
        if pending.is_empty() {
            return;
        }
        let total = pending.len();
        let douban = DoubanClient::new(http.clone());
        let mut checked = 0usize;
        let mut rated = 0usize;
        let mut blocked = false;
        for (original, media_type) in pending.into_iter().take(DOUBAN_ROUND_BUDGET) {
            // 同一个片名的各个榜单条目共用一次查询；优先用 TMDB 给出的中文名搜索。
            let sample = boards
                .iter()
                .flat_map(|board| &board.items)
                .find(|item| {
                    item.original_title.as_deref() == Some(original.as_str())
                        && item.media_type.as_ref() == Some(&media_type)
                })
                .cloned();
            let Some(sample) = sample else {
                continue;
            };
            let found = match lookup_douban(&douban, &sample, &media_type).await {
                Ok(found) => found,
                Err(DoubanError::RateLimited) => {
                    blocked = true;
                    break;
                }
                Err(error) => {
                    tracing::debug!(%error, title = original, "douban rating lookup failed");
                    continue;
                }
            };
            checked += 1;
            if found
                .as_ref()
                .is_some_and(|subject| subject.rating.is_some())
            {
                rated += 1;
            }
            let stamp = Utc::now().to_rfc3339();
            for item in boards.iter_mut().flat_map(|board| &mut board.items) {
                if item.original_title.as_deref() == Some(original.as_str())
                    && item.media_type.as_ref() == Some(&media_type)
                {
                    if found.is_some() {
                        item.douban = found.clone();
                    }
                    item.douban_checked_at = Some(stamp.clone());
                }
            }
            if checked % DOUBAN_PUBLISH_EVERY == 0 {
                self.publish_rank_snapshot(RankSource::Netflix, boards.clone())
                    .await;
            }
        }
        if checked > 0 {
            self.publish_rank_snapshot(RankSource::Netflix, boards.clone())
                .await;
        }
        let mut message = global_rank::refresh_message(
            &boards,
            &boards
                .iter()
                .filter(|board| board.stale)
                .map(|board| board.name.clone())
                .collect::<Vec<_>>(),
        );
        message.push_str(&format!(
            "；本轮查询豆瓣 {checked} 个片名，{rated} 个有评分"
        ));
        if blocked {
            message.push_str("，豆瓣限流，其余稍后再补");
        } else if total > checked {
            message.push_str(&format!("，其余 {} 个下一轮再补", total - checked));
        }
        self.global_rank
            .set_message(RankSource::Netflix, message)
            .await;
        tracing::info!(
            total,
            checked,
            rated,
            blocked,
            "netflix douban ratings filled"
        );
    }

    // ---- 海报代理 ----

    /// 代取榜单海报（data URL）。只允许榜单用到的图床，见 `global_rank::poster_url_allowed`。
    pub async fn rank_poster(&self, url: &str) -> Result<String, MediaServiceError> {
        if !global_rank::poster_url_allowed(url) {
            return Err(MediaServiceError::Invalid(
                "unsupported poster url".to_string(),
            ));
        }
        if let Some(cached) = self.global_rank.cached_poster(url).await {
            return Ok(cached);
        }
        let settings = self.db.get_settings().await?;
        let client = client_factory::build_client(settings.proxy.as_deref())?;
        let poster = global_rank::fetch_poster(&client, url)
            .await
            .map_err(MediaServiceError::Upstream)?;
        self.global_rank
            .store_poster(url.to_string(), poster.clone())
            .await;
        Ok(poster)
    }

    // ---- 自动订阅 ----

    pub async fn rank_subscribe_history(
        &self,
    ) -> Result<Vec<RankSubscribeRecord>, MediaServiceError> {
        let mut history = self.load_subscribe_history().await?;
        history.sort_by(|left, right| right.time.cmp(&left.time));
        Ok(history)
    }

    /// 清空自动订阅记录。清空后榜单上的条目会被视为没有订阅过，下次刷新可能再次订阅。
    pub async fn clear_rank_subscribe_history(&self) -> Result<(), MediaServiceError> {
        self.db
            .set_global_rank_state(HISTORY_KEY, "[]".to_string())
            .await?;
        Ok(())
    }

    async fn load_subscribe_history(&self) -> Result<Vec<RankSubscribeRecord>, MediaServiceError> {
        Ok(match self.db.get_global_rank_state(HISTORY_KEY).await? {
            Some(payload) => serde_json::from_str(&payload).unwrap_or_default(),
            None => Vec::new(),
        })
    }

    /// 猫眼刷新结束后的收尾：按设置自动订阅榜单前 N 条，并写入本轮的结果摘要。
    pub(super) async fn finish_maoyan_refresh(&self, settings: &RankSettings, refreshed: usize) {
        let view = self.maoyan_rank_view(settings).await;
        let stale: Vec<String> = view
            .boards
            .iter()
            .filter(|board| board.stale)
            .map(|board| board.name.clone())
            .collect();
        let mut message = if refreshed == 0 {
            "本次未抓取到榜单（数据源异常或网络不可用）".to_string()
        } else {
            global_rank::refresh_message(&view.boards, &stale)
        };
        if settings.maoyan.any_subscribe() {
            match self.auto_subscribe_maoyan(settings).await {
                Ok(0) => {}
                Ok(count) => message.push_str(&format!("；自动订阅 {count} 部")),
                Err(error) => {
                    tracing::warn!(%error, "maoyan auto subscribe failed");
                    message.push_str(&format!("；自动订阅未执行：{error}"));
                }
            }
        }
        self.global_rank
            .set_message(RankSource::Maoyan, message)
            .await;
    }

    /// 把榜单前 N 条里已匹配 TMDB 的条目加入订阅；每个条目只自动订阅一次。
    async fn auto_subscribe_maoyan(
        &self,
        settings: &RankSettings,
    ) -> Result<usize, MediaServiceError> {
        let defaults = &settings.subscribe;
        let (Some(quality_profile_id), Some(downloader_id)) =
            (defaults.quality_profile_id, defaults.downloader_id)
        else {
            return Err(MediaServiceError::Invalid(
                "没有选择订阅使用的质量配置和下载器".to_string(),
            ));
        };
        let site_ids: Vec<i64> = if defaults.site_ids.is_empty() {
            self.db
                .list_sites()
                .await?
                .into_iter()
                .map(|site| site.id)
                .collect()
        } else {
            defaults.site_ids.clone()
        };
        if site_ids.is_empty() {
            return Err(MediaServiceError::Invalid("还没有配置 PT 站点".to_string()));
        }
        let mut history = self.load_subscribe_history().await?;
        let mut done: HashSet<String> =
            history.iter().map(|record| record.unique.clone()).collect();
        let mut existing: HashSet<(i64, String, Option<u32>)> = self
            .db
            .list_subscriptions()
            .await?
            .into_iter()
            .map(|subscription| {
                (
                    subscription.tmdb_id,
                    subscription.media_type,
                    subscription.season,
                )
            })
            .collect();

        let mut entries = self.maoyan.all().await;
        entries.sort_by_key(|(entry, _)| {
            (
                MaoyanChart::ALL
                    .iter()
                    .position(|chart| *chart == entry.chart),
                MaoyanPlatform::ALL
                    .iter()
                    .position(|platform| *platform == entry.platform),
            )
        });
        let mut added = 0usize;
        for (entry, error) in entries {
            let count = settings.maoyan.subscribe_count(entry.chart, entry.platform);
            // 沿用旧数据的榜单不参与订阅，避免按过期名次订阅。
            if count == 0 || error.is_some() || !settings.maoyan.wants(entry.chart, entry.platform)
            {
                continue;
            }
            let board = global_rank::maoyan_board(entry.clone(), None);
            for item in entry.items.iter().take(count) {
                let Some(media) = &item.tmdb else {
                    continue;
                };
                let unique = format!(
                    "maoyan:{}:{}:{}",
                    media.media_type.as_str(),
                    media.tmdb_id,
                    item.title.trim()
                );
                if done.contains(&unique) {
                    continue;
                }
                let season = if media.media_type == TmdbMediaType::Tv {
                    match global_rank::explicit_season(&item.title) {
                        Some(season) => Some(season),
                        // 片名没写季号时订阅 TMDB 上的最新一季。
                        None => match self.tmdb_details(media.tmdb_id, TmdbMediaType::Tv).await {
                            Ok(details) => Some(
                                details
                                    .number_of_seasons
                                    .filter(|count| *count > 0)
                                    .unwrap_or(1),
                            ),
                            Err(error) => {
                                tracing::debug!(%error, title = item.title, "auto subscribe skipped: tmdb details unavailable");
                                continue;
                            }
                        },
                    }
                } else {
                    None
                };
                let key = (media.tmdb_id, media.media_type.as_str().to_string(), season);
                if existing.contains(&key) {
                    // 已经手动订阅过：本轮不再处理，但不写入记录。
                    done.insert(unique);
                    continue;
                }
                let request = CreateSubscriptionRequest {
                    tmdb_id: media.tmdb_id,
                    media_type: media.media_type.as_str().to_string(),
                    season,
                    start_episode: None,
                    absolute_episode: None,
                    quality_profile_id,
                    downloader_id,
                    site_ids: site_ids.clone(),
                    save_path: if media.media_type == TmdbMediaType::Tv {
                        defaults.tv_save_path.clone()
                    } else {
                        defaults.movie_save_path.clone()
                    },
                    enabled: true,
                    group_policy: Default::default(),
                };
                match self.create_subscription(&request).await {
                    Ok(subscription) => {
                        tracing::info!(
                            title = item.title,
                            tmdb_id = media.tmdb_id,
                            board = board.name,
                            "auto subscribed from maoyan chart"
                        );
                        history.push(RankSubscribeRecord {
                            unique: unique.clone(),
                            title: item.title.clone(),
                            media_type: media.media_type.clone(),
                            tmdb_id: media.tmdb_id,
                            tmdb_title: media.title.clone(),
                            year: media.year,
                            season,
                            poster_path: media.poster_path.clone(),
                            board: board.name.clone(),
                            platform: item.platform.clone(),
                            release_info: item.release_info.clone(),
                            subscription_id: Some(subscription.id),
                            time: Utc::now().to_rfc3339(),
                        });
                        done.insert(unique);
                        existing.insert(key);
                        added += 1;
                    }
                    Err(error) => {
                        // 例如新一季在 TMDB 上还没有分集信息；下一轮刷新再试。
                        tracing::debug!(%error, title = item.title, "auto subscribe failed for chart item");
                    }
                }
            }
        }
        if added > 0 {
            if history.len() > HISTORY_LIMIT {
                history.sort_by(|left, right| left.time.cmp(&right.time));
                let overflow = history.len() - HISTORY_LIMIT;
                history.drain(..overflow);
            }
            let payload = serde_json::to_string(&history)
                .map_err(|error| MediaServiceError::Serialization(error.to_string()))?;
            self.db.set_global_rank_state(HISTORY_KEY, payload).await?;
        }
        Ok(added)
    }
}

fn apply_tmdb(item: &mut RankItem, media: &TmdbMedia) {
    item.title = media.title.clone();
    item.poster = media.poster_path.clone();
    item.overview = Some(media.overview.clone()).filter(|text| !text.is_empty());
    item.year = media.year;
    item.tmdb = Some(media.clone());
}

/// 榜单是否仍在设置的抓取范围内。
fn board_wanted(board: &RankBoard, settings: &RankSettings) -> bool {
    match board.source {
        RankSource::Maoyan => true,
        RankSource::Netflix => settings.netflix.regions.contains(&board.group),
        RankSource::Hongguo => settings.hongguo.boards.contains(&board.key),
        RankSource::Music => settings.music.boards.contains(&board.key),
    }
}

/// 查一个 Netflix 条目的豆瓣条目：优先用中文名，其次英文原名。
/// 电影要求年份相近，避免同名翻拍串到旧片；剧集在豆瓣按季收录，取同名里最新的一季。
async fn lookup_douban(
    douban: &DoubanClient,
    item: &RankItem,
    media_type: &TmdbMediaType,
) -> Result<Option<DoubanSubject>, DoubanError> {
    let mut queries: Vec<String> = Vec::new();
    if item.tmdb.is_some() {
        queries.push(item.title.clone());
    }
    if let Some(original) = &item.original_title
        && !queries.contains(original)
    {
        queries.push(original.clone());
    }
    let year = item.tmdb.as_ref().and_then(|media| media.year);
    for query in queries {
        let subjects = douban.search_rated(&query).await?;
        let picked = maoyan::pick_douban(&query, media_type, subjects).filter(|subject| {
            *media_type == TmdbMediaType::Tv
                || match (subject.year, year) {
                    (Some(found), Some(wanted)) => found.abs_diff(wanted) <= 1,
                    _ => true,
                }
        });
        if picked.is_some() {
            return Ok(picked);
        }
    }
    Ok(None)
}
