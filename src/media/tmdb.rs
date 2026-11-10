use std::sync::LazyLock;
use std::time::Duration;

use reqwest::{Client, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;

const TMDB_API_BASE: &str = "https://api.themoviedb.org/3";

/// TMDB 官方限制：单 IP 约 50 请求/秒、最多 20 个并发连接。
/// 所有 TMDB 请求共用这个队列，留一些余量，避免批量匹配时被 429 拒绝。
const TMDB_MAX_CONCURRENCY: usize = 16;
const TMDB_MIN_INTERVAL: Duration = Duration::from_millis(25);
const TMDB_MAX_ATTEMPTS: u32 = 4;

static TMDB_CONCURRENCY: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(TMDB_MAX_CONCURRENCY));
static TMDB_NEXT_SLOT: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

/// 按固定间隔发放请求时隙，使整体速率不超过上限。
async fn wait_for_tmdb_slot() {
    let wait_until = {
        let mut next = TMDB_NEXT_SLOT.lock().await;
        let now = Instant::now();
        let slot = (*next).max(now);
        *next = slot + TMDB_MIN_INTERVAL;
        slot
    };
    tokio::time::sleep_until(wait_until).await;
}

fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or_else(|| Duration::from_millis(500 * 2u64.pow(attempt)))
        .min(Duration::from_secs(10))
}

#[derive(Clone)]
pub struct TmdbClient {
    client: Client,
    auth: TmdbAuth,
    language: String,
}

#[derive(Clone)]
enum TmdbAuth {
    ApiKey(String),
    Bearer(String),
}

#[derive(Debug, thiserror::Error)]
pub enum TmdbError {
    #[error("TMDB token is not configured")]
    MissingToken,
    #[error("invalid TMDB media type: {0}")]
    InvalidMediaType(String),
    #[error("TMDB request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("TMDB returned HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("TMDB response could not be parsed: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TmdbMediaType {
    Tv,
    Movie,
}

impl TmdbMediaType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tv => "tv",
            Self::Movie => "movie",
        }
    }

    pub fn parse(value: &str) -> Result<Self, TmdbError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tv" => Ok(Self::Tv),
            "movie" => Ok(Self::Movie),
            other => Err(TmdbError::InvalidMediaType(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmdbMedia {
    pub tmdb_id: i64,
    pub media_type: TmdbMediaType,
    pub title: String,
    pub original_title: Option<String>,
    pub year: Option<u32>,
    pub overview: String,
    pub poster_path: Option<String>,
    pub is_animation: bool,
    pub genres: Vec<TmdbGenre>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TmdbGenre {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmdbDetails {
    #[serde(flatten)]
    pub media: TmdbMedia,
    pub aliases: Vec<String>,
    pub number_of_seasons: Option<u32>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmdbEpisode {
    pub id: i64,
    pub season_number: u32,
    pub episode_number: u32,
    pub name: String,
    pub overview: String,
    pub air_date: Option<String>,
    pub runtime: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmdbSeason {
    pub id: i64,
    pub tmdb_id: i64,
    pub season_number: u32,
    pub name: String,
    pub overview: String,
    pub poster_path: Option<String>,
    pub air_date: Option<String>,
    pub episodes: Vec<TmdbEpisode>,
}

impl TmdbClient {
    pub fn new(
        client: Client,
        token: impl Into<String>,
        language: impl Into<String>,
    ) -> Result<Self, TmdbError> {
        let token = token.into().trim().to_string();
        if token.is_empty() {
            return Err(TmdbError::MissingToken);
        }
        let language = language.into().trim().to_string();
        Ok(Self {
            client,
            auth: if looks_like_read_token(&token) {
                TmdbAuth::Bearer(token)
            } else {
                TmdbAuth::ApiKey(token)
            },
            language: if language.is_empty() {
                "zh-CN".to_string()
            } else {
                language
            },
        })
    }

    pub async fn search(
        &self,
        query: &str,
        media_type: Option<&str>,
    ) -> Result<Vec<TmdbMedia>, TmdbError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let kind = match media_type
            .unwrap_or("multi")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "multi" | "tv" | "movie" => media_type.unwrap_or("multi").trim().to_ascii_lowercase(),
            other => return Err(TmdbError::InvalidMediaType(other.to_string())),
        };
        let request = self.client.get(format!("{TMDB_API_BASE}/search/{kind}"));
        let request = self.authorize(request).query(&[
            ("query", query),
            ("language", self.language.as_str()),
            ("include_adult", "false"),
        ]);
        let raw: RawSearchResponse = self.send_json(request).await?;
        Ok(map_search_results(raw, &kind))
    }

    /// 按年份限定搜索单一类型（tv / movie），用于把豆瓣条目对应到 TMDB。
    pub async fn search_typed(
        &self,
        query: &str,
        media_type: &TmdbMediaType,
        year: Option<u32>,
    ) -> Result<Vec<TmdbMedia>, TmdbError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let kind = media_type.as_str();
        let year_key = match media_type {
            TmdbMediaType::Tv => "first_air_date_year",
            TmdbMediaType::Movie => "primary_release_year",
        };
        let year = year.map(|year| year.to_string());
        let request = self.client.get(format!("{TMDB_API_BASE}/search/{kind}"));
        let mut request = self.authorize(request).query(&[
            ("query", query),
            ("language", self.language.as_str()),
            ("include_adult", "false"),
        ]);
        if let Some(year) = &year {
            request = request.query(&[(year_key, year.as_str())]);
        }
        let raw: RawSearchResponse = self.send_json(request).await?;
        Ok(map_search_results(raw, kind))
    }

    /// 通过 IMDb 编号精确查找 TMDB 条目。
    pub async fn find_by_imdb(
        &self,
        imdb_id: &str,
        media_type: &TmdbMediaType,
    ) -> Result<Option<TmdbMedia>, TmdbError> {
        let imdb_id = imdb_id.trim();
        if !imdb_id.starts_with("tt") || !imdb_id[2..].bytes().all(|byte| byte.is_ascii_digit()) {
            return Ok(None);
        }
        let request = self.client.get(format!("{TMDB_API_BASE}/find/{imdb_id}"));
        let request = self.authorize(request).query(&[
            ("external_source", "imdb_id"),
            ("language", self.language.as_str()),
        ]);
        let raw: RawFindResponse = self.send_json(request).await?;
        let results = match media_type {
            TmdbMediaType::Tv => raw.tv_results,
            TmdbMediaType::Movie => raw.movie_results,
        };
        Ok(map_search_results(RawSearchResponse { results }, media_type.as_str())
            .into_iter()
            .next())
    }

    pub async fn details(
        &self,
        tmdb_id: i64,
        media_type: TmdbMediaType,
    ) -> Result<TmdbDetails, TmdbError> {
        let request = self
            .client
            .get(format!("{TMDB_API_BASE}/{}/{tmdb_id}", media_type.as_str()));
        let request = self.authorize(request).query(&[
            ("language", self.language.as_str()),
            ("append_to_response", "alternative_titles,translations"),
        ]);
        let raw: RawDetails = self.send_json(request).await?;
        map_details(raw, media_type)
    }

    /// 评分与主演：一次详情请求附带演职员表（剧集用汇总演职员表，覆盖所有季）。
    pub async fn extras(
        &self,
        tmdb_id: i64,
        media_type: TmdbMediaType,
        cast_limit: usize,
    ) -> Result<TmdbExtras, TmdbError> {
        let credits = match media_type {
            TmdbMediaType::Tv => "aggregate_credits",
            TmdbMediaType::Movie => "credits",
        };
        let request = self
            .client
            .get(format!("{TMDB_API_BASE}/{}/{tmdb_id}", media_type.as_str()));
        let request = self.authorize(request).query(&[
            ("language", self.language.as_str()),
            ("append_to_response", credits),
        ]);
        let raw: RawExtras = self.send_json(request).await?;
        Ok(map_extras(raw, cast_limit))
    }

    pub async fn season(&self, tmdb_id: i64, season_number: u32) -> Result<TmdbSeason, TmdbError> {
        let request = self.client.get(format!(
            "{TMDB_API_BASE}/tv/{tmdb_id}/season/{season_number}"
        ));
        let request = self
            .authorize(request)
            .query(&[("language", self.language.as_str())]);
        let raw: RawSeason = self.send_json(request).await?;
        Ok(map_season(raw, tmdb_id, season_number))
    }

    fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.auth {
            TmdbAuth::ApiKey(key) => request.query(&[("api_key", key)]),
            TmdbAuth::Bearer(token) => request.bearer_auth(token),
        }
    }

    /// 经全局队列发送请求；网络错误、429 和 5xx 会按退避重试。
    async fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        request: RequestBuilder,
    ) -> Result<T, TmdbError> {
        let mut attempt = 0;
        loop {
            let current = request
                .try_clone()
                .expect("TMDB requests have no streaming body");
            let (result, retry_after) = {
                let _permit = TMDB_CONCURRENCY
                    .acquire()
                    .await
                    .expect("TMDB semaphore is never closed");
                wait_for_tmdb_slot().await;
                Self::send_once(current).await
            };
            attempt += 1;
            match result {
                Err(error) if attempt < TMDB_MAX_ATTEMPTS && is_retryable(&error) => {
                    let delay = retry_delay(attempt - 1, retry_after);
                    tracing::debug!(%error, attempt, delay_ms = delay.as_millis() as u64, "retrying TMDB request");
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error),
                Ok(body) => {
                    return serde_json::from_str(&body)
                        .map_err(|error| TmdbError::Parse(error.to_string()));
                }
            }
        }
    }

    async fn send_once(request: RequestBuilder) -> (Result<String, TmdbError>, Option<Duration>) {
        let response = match request.timeout(Duration::from_secs(20)).send().await {
            Ok(response) => response,
            Err(error) => return (Err(TmdbError::Transport(error.without_url())), None),
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => return (Err(TmdbError::Transport(error.without_url())), None),
        };
        if !status.is_success() {
            let message = serde_json::from_str::<RawError>(&body)
                .ok()
                .and_then(|error| error.status_message)
                .unwrap_or_else(|| sanitize_error_body(&body));
            return (Err(TmdbError::Http { status, message }), retry_after);
        }
        (Ok(body), None)
    }
}

fn is_retryable(error: &TmdbError) -> bool {
    match error {
        TmdbError::Transport(_) => true,
        TmdbError::Http { status, .. } => {
            *status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
        }
        _ => false,
    }
}

#[derive(Deserialize)]
struct RawError {
    status_message: Option<String>,
}

#[derive(Deserialize)]
struct RawSearchResponse {
    #[serde(default)]
    results: Vec<RawMedia>,
}

#[derive(Deserialize)]
struct RawFindResponse {
    #[serde(default)]
    movie_results: Vec<RawMedia>,
    #[serde(default)]
    tv_results: Vec<RawMedia>,
}

#[derive(Deserialize)]
struct RawMedia {
    id: i64,
    media_type: Option<String>,
    title: Option<String>,
    name: Option<String>,
    original_title: Option<String>,
    original_name: Option<String>,
    release_date: Option<String>,
    first_air_date: Option<String>,
    overview: Option<String>,
    poster_path: Option<String>,
    #[serde(default)]
    genre_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct RawCredits {
    #[serde(default)]
    cast: Vec<RawCastMember>,
}

#[derive(Deserialize)]
struct RawExtras {
    #[serde(default)]
    vote_average: Option<f64>,
    #[serde(default)]
    vote_count: Option<u64>,
    #[serde(default)]
    credits: Option<RawCredits>,
    #[serde(default)]
    aggregate_credits: Option<RawCredits>,
}

/// TMDB 评分与主演。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TmdbExtras {
    /// 评分（0–10）；评分人数太少时为空，避免一两个人打的分误导。
    pub rating: Option<f64>,
    pub cast: Vec<String>,
}

/// 至少这么多人评分才展示 TMDB 评分。
const MIN_TMDB_VOTES: u64 = 5;

fn map_extras(raw: RawExtras, cast_limit: usize) -> TmdbExtras {
    let rating = raw
        .vote_average
        .filter(|value| *value > 0.0 && raw.vote_count.unwrap_or(0) >= MIN_TMDB_VOTES)
        .map(|value| (value * 10.0).round() / 10.0);
    let cast = raw
        .aggregate_credits
        .or(raw.credits)
        .map(|credits| map_cast(credits, cast_limit))
        .unwrap_or_default();
    TmdbExtras { rating, cast }
}

#[derive(Deserialize)]
struct RawCastMember {
    #[serde(default)]
    name: String,
    #[serde(default)]
    order: Option<i64>,
}

fn map_cast(raw: RawCredits, limit: usize) -> Vec<String> {
    let mut cast = raw.cast;
    cast.sort_by_key(|member| member.order.unwrap_or(i64::MAX));
    let mut names: Vec<String> = Vec::new();
    for member in cast {
        let name = member.name.trim();
        if !name.is_empty() && !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
        if names.len() >= limit {
            break;
        }
    }
    names
}

#[derive(Deserialize)]
struct RawDetails {
    id: i64,
    title: Option<String>,
    name: Option<String>,
    original_title: Option<String>,
    original_name: Option<String>,
    release_date: Option<String>,
    first_air_date: Option<String>,
    overview: Option<String>,
    poster_path: Option<String>,
    number_of_seasons: Option<u32>,
    status: Option<String>,
    alternative_titles: Option<RawAlternativeTitles>,
    translations: Option<RawTranslations>,
    #[serde(default)]
    genres: Vec<RawGenre>,
}

#[derive(Deserialize)]
struct RawGenre {
    id: i64,
    #[serde(rename = "name")]
    _name: String,
}

#[derive(Default, Deserialize)]
struct RawAlternativeTitles {
    #[serde(default)]
    results: Vec<RawAlias>,
    #[serde(default)]
    titles: Vec<RawAlias>,
}

#[derive(Deserialize)]
struct RawAlias {
    title: String,
}

#[derive(Default, Deserialize)]
struct RawTranslations {
    #[serde(default)]
    translations: Vec<RawTranslation>,
}

#[derive(Deserialize)]
struct RawTranslation {
    data: RawTranslationData,
}

#[derive(Deserialize)]
struct RawTranslationData {
    title: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct RawSeason {
    id: i64,
    name: Option<String>,
    overview: Option<String>,
    poster_path: Option<String>,
    air_date: Option<String>,
    #[serde(default)]
    episodes: Vec<RawEpisode>,
}

#[derive(Deserialize)]
struct RawEpisode {
    id: i64,
    season_number: Option<u32>,
    episode_number: Option<u32>,
    name: Option<String>,
    overview: Option<String>,
    air_date: Option<String>,
    runtime: Option<u32>,
}

fn map_search_results(raw: RawSearchResponse, requested_kind: &str) -> Vec<TmdbMedia> {
    raw.results
        .into_iter()
        .filter_map(|item| {
            let kind = item.media_type.as_deref().unwrap_or(requested_kind);
            let media_type = match kind {
                "tv" => TmdbMediaType::Tv,
                "movie" => TmdbMediaType::Movie,
                _ => return None,
            };
            Some(map_media(item, media_type))
        })
        .filter(|item| !item.title.is_empty())
        .collect()
}

fn map_media(raw: RawMedia, media_type: TmdbMediaType) -> TmdbMedia {
    let (title, original_title, date) = match media_type {
        TmdbMediaType::Tv => (raw.name, raw.original_name, raw.first_air_date),
        TmdbMediaType::Movie => (raw.title, raw.original_title, raw.release_date),
    };
    let title = title.unwrap_or_default().trim().to_string();
    let original_title = clean_distinct(original_title, Some(&title));
    TmdbMedia {
        tmdb_id: raw.id,
        media_type: media_type.clone(),
        title,
        original_title,
        year: date.as_deref().and_then(parse_year),
        overview: raw.overview.unwrap_or_default(),
        poster_path: clean_optional(raw.poster_path),
        is_animation: raw.genre_ids.contains(&16),
        genres: raw
            .genre_ids
            .into_iter()
            .map(|id| TmdbGenre {
                id,
                name: genre_name_zh(&media_type, id).to_string(),
            })
            .collect(),
    }
}

fn map_details(raw: RawDetails, media_type: TmdbMediaType) -> Result<TmdbDetails, TmdbError> {
    let (title, original_title, date) = match media_type {
        TmdbMediaType::Tv => (raw.name, raw.original_name, raw.first_air_date),
        TmdbMediaType::Movie => (raw.title, raw.original_title, raw.release_date),
    };
    let title = title.unwrap_or_default().trim().to_string();
    if title.is_empty() {
        return Err(TmdbError::Parse(
            "details response has no title".to_string(),
        ));
    }
    let original_title = clean_distinct(original_title, Some(&title));
    let mut aliases = Vec::new();
    if let Some(value) = &original_title {
        push_unique(&mut aliases, value);
    }
    let alternatives = raw.alternative_titles.unwrap_or_default();
    for alias in alternatives.results.into_iter().chain(alternatives.titles) {
        if !alias.title.eq_ignore_ascii_case(&title) {
            push_unique(&mut aliases, &alias.title);
        }
    }
    for translation in raw.translations.unwrap_or_default().translations {
        if let Some(translated_title) = translation.data.name.or(translation.data.title)
            && !translated_title.eq_ignore_ascii_case(&title)
        {
            push_unique(&mut aliases, &translated_title);
        }
    }
    Ok(TmdbDetails {
        media: TmdbMedia {
            tmdb_id: raw.id,
            media_type: media_type.clone(),
            title,
            original_title,
            year: date.as_deref().and_then(parse_year),
            overview: raw.overview.unwrap_or_default(),
            poster_path: clean_optional(raw.poster_path),
            is_animation: raw.genres.iter().any(|genre| genre.id == 16),
            genres: raw
                .genres
                .into_iter()
                .map(|genre| TmdbGenre {
                    id: genre.id,
                    name: genre_name_zh(&media_type, genre.id).to_string(),
                })
                .collect(),
        },
        aliases,
        number_of_seasons: raw.number_of_seasons,
        status: clean_optional(raw.status),
    })
}

fn genre_name_zh(media_type: &TmdbMediaType, id: i64) -> &'static str {
    match (media_type, id) {
        (_, 16) => "动画",
        (_, 18) => "剧情",
        (_, 35) => "喜剧",
        (_, 80) => "犯罪",
        (_, 99) => "纪录",
        (_, 10751) => "家庭",
        (_, 10752) => "战争",
        (_, 10749) => "爱情",
        (_, 9648) => "悬疑",
        (_, 10759) => "动作冒险",
        (_, 10762) => "儿童",
        (_, 10763) => "新闻",
        (_, 10764) => "真人秀",
        (_, 10765) => "科幻奇幻",
        (_, 10766) => "肥皂剧",
        (_, 10767) => "脱口秀",
        (_, 10768) => "战争政治",
        (TmdbMediaType::Movie, 28) => "动作",
        (TmdbMediaType::Movie, 12) => "冒险",
        (TmdbMediaType::Movie, 14) => "奇幻",
        (TmdbMediaType::Movie, 27) => "恐怖",
        (TmdbMediaType::Movie, 36) => "历史",
        (TmdbMediaType::Movie, 53) => "惊悚",
        (TmdbMediaType::Movie, 878) => "科幻",
        (TmdbMediaType::Movie, 10402) => "音乐",
        (TmdbMediaType::Movie, 10770) => "电视电影",
        (TmdbMediaType::Movie, 37) => "西部",
        _ => "其他",
    }
}

fn map_season(raw: RawSeason, tmdb_id: i64, fallback_season: u32) -> TmdbSeason {
    let episodes = raw
        .episodes
        .into_iter()
        .filter_map(|episode| {
            let episode_number = episode.episode_number?;
            Some(TmdbEpisode {
                id: episode.id,
                season_number: episode.season_number.unwrap_or(fallback_season),
                episode_number,
                name: episode.name.unwrap_or_default(),
                overview: episode.overview.unwrap_or_default(),
                air_date: clean_optional(episode.air_date),
                runtime: episode.runtime,
            })
        })
        .collect();
    TmdbSeason {
        id: raw.id,
        tmdb_id,
        season_number: fallback_season,
        name: raw
            .name
            .unwrap_or_else(|| format!("Season {fallback_season}")),
        overview: raw.overview.unwrap_or_default(),
        poster_path: clean_optional(raw.poster_path),
        air_date: clean_optional(raw.air_date),
        episodes,
    }
}

fn looks_like_read_token(token: &str) -> bool {
    token.starts_with("eyJ") && token.matches('.').count() == 2
}

fn parse_year(date: &str) -> Option<u32> {
    date.get(..4)?
        .parse()
        .ok()
        .filter(|year| (1870..=2200).contains(year))
}

fn clean_distinct(value: Option<String>, primary: Option<&str>) -> Option<String> {
    clean_optional(value)
        .filter(|value| primary.is_none_or(|primary| !value.eq_ignore_ascii_case(primary.trim())))
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if value.is_empty()
        || values
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(value))
    {
        return;
    }
    values.push(value.to_string());
}

fn sanitize_error_body(body: &str) -> String {
    body.chars()
        .take(300)
        .collect::<String>()
        .replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_multi_search_and_ignores_people() {
        let raw: RawSearchResponse = serde_json::from_str(
            r#"{"results":[
                {"id":1,"media_type":"tv","name":"百日成王","original_name":"The King","first_air_date":"2025-01-02","genre_ids":[16]},
                {"id":2,"media_type":"movie","title":"沙丘2","original_title":"Dune: Part Two","release_date":"2024-03-01"},
                {"id":3,"media_type":"person","name":"Somebody"}
            ]}"#,
        )
        .unwrap();
        let mapped = map_search_results(raw, "multi");
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0].year, Some(2025));
        assert!(mapped[0].is_animation);
        assert_eq!(mapped[1].media_type, TmdbMediaType::Movie);
    }

    #[test]
    fn details_deduplicates_original_and_alternative_titles() {
        let raw: RawDetails = serde_json::from_str(
            r#"{"id":7,"name":"Example","original_name":"Original","first_air_date":"2020-05-01",
                "alternative_titles":{"results":[{"title":"Original"},{"title":"Alias"},{"title":"alias"}]}}"#,
        )
        .unwrap();
        let details = map_details(raw, TmdbMediaType::Tv).unwrap();
        assert_eq!(details.aliases, vec!["Original", "Alias"]);
    }

    #[test]
    fn details_identifies_animation_from_tmdb_genre_id() {
        let raw: RawDetails = serde_json::from_str(
            r#"{"id":7,"name":"百日成王","genres":[{"id":16,"name":"Animation"}]}"#,
        )
        .unwrap();
        assert!(
            map_details(raw, TmdbMediaType::Tv)
                .unwrap()
                .media
                .is_animation
        );
    }

    #[test]
    fn details_include_translated_tv_and_movie_titles_as_aliases() {
        let tv: RawDetails = serde_json::from_str(
            r#"{"id":7,"name":"百日成王","original_name":"Bai Ri Cheng Wang",
                "translations":{"translations":[
                    {"data":{"name":"Crowned in a Hundred Days"}},
                    {"data":{"name":"百日成王"}}
                ]}}"#,
        )
        .unwrap();
        let movie: RawDetails = serde_json::from_str(
            r#"{"id":8,"title":"沙丘2","original_title":"Dune: Part Two",
                "translations":{"translations":[{"data":{"title":"Dune Part Two"}}]}}"#,
        )
        .unwrap();

        assert_eq!(
            map_details(tv, TmdbMediaType::Tv).unwrap().aliases,
            vec!["Bai Ri Cheng Wang", "Crowned in a Hundred Days"]
        );
        assert_eq!(
            map_details(movie, TmdbMediaType::Movie).unwrap().aliases,
            vec!["Dune: Part Two", "Dune Part Two"]
        );
    }

    #[test]
    fn season_skips_entries_without_episode_number() {
        let raw: RawSeason = serde_json::from_str(
            r#"{"id":9,"episodes":[
                {"id":1,"season_number":1,"episode_number":3,"name":"Third"},
                {"id":2,"name":"Malformed"}
            ]}"#,
        )
        .unwrap();
        let season = map_season(raw, 10, 1);
        assert_eq!(season.episodes.len(), 1);
        assert_eq!(season.episodes[0].episode_number, 3);
    }

    #[test]
    fn detects_v3_keys_and_v4_read_tokens_without_echoing_them() {
        assert!(!looks_like_read_token("0123456789abcdef"));
        assert!(looks_like_read_token("eyJheader.payload.signature"));
        assert_eq!(
            TmdbError::MissingToken.to_string(),
            "TMDB token is not configured"
        );
    }

    #[test]
    fn cast_follows_billing_order_and_skips_blank_or_duplicate_names() {
        let raw: RawCredits = serde_json::from_value(serde_json::json!({
            "cast": [
                {"name": "孙艺洲", "order": 1},
                {"name": "井柏然", "order": 0},
                {"name": " ", "order": 2},
                {"name": "井柏然", "order": 3},
                {"name": "张颂文", "order": 4},
                {"name": "王鸥"}
            ]
        }))
        .unwrap();
        assert_eq!(map_cast(raw, 3), vec!["井柏然", "孙艺洲", "张颂文"]);
    }

    #[test]
    fn extras_hide_ratings_with_too_few_votes() {
        let raw: RawExtras = serde_json::from_value(serde_json::json!({
            "vote_average": 7.846, "vote_count": 120,
            "aggregate_credits": {"cast": [{"name": "井柏然", "order": 0}]}
        }))
        .unwrap();
        assert_eq!(map_extras(raw, 4), TmdbExtras { rating: Some(7.8), cast: vec!["井柏然".into()] });

        let raw: RawExtras =
            serde_json::from_value(serde_json::json!({"vote_average": 10.0, "vote_count": 1})).unwrap();
        assert_eq!(map_extras(raw, 4).rating, None);
    }
}
