//! 内置豆瓣接口（豆瓣 App 使用的 frodo API，与 MoviePilot 等开源项目同源）。
//!
//! 中文影视在豆瓣上的收录和命名通常比 TMDB 更准，因此榜单匹配时先查豆瓣，
//! 再借助豆瓣给出的年份、原名、又名和 IMDb 编号定位 TMDB 条目。
//! 豆瓣对频繁请求会封禁 IP，所有请求共用一个低速队列。

use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine;
use chrono::{FixedOffset, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;

use super::tmdb::TmdbMediaType;

const FRODO_BASE: &str = "https://frodo.douban.com";
const FRODO_PATH_PREFIX: &str = "/api/v2";
const API_KEY: &str = "0dad551ec0f84ed02907ff5c42e8ec70";
const API_SECRET: &[u8] = b"bf7dddc7c9cfe6f7";
const USER_AGENT: &str = "api-client/1 com.douban.frodo/7.22.0.beta9(231) Android/23 product/Mate 40 vendor/HUAWEI model/Mate 40 brand/HUAWEI  rom/android  network/wifi  platform/AndroidPad";

const WEB_SUGGEST: &str = "https://movie.douban.com/j/subject_suggest";
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36";

// 豆瓣搜索频率稍高就会返回 need_login（403），因此串行、慢速请求；
// 一旦被限制，frodo 接口冷却一段时间，期间搜索改走网页版联想接口。
const DOUBAN_MAX_CONCURRENCY: usize = 1;
const DOUBAN_MIN_INTERVAL: Duration = Duration::from_millis(1200);
const DOUBAN_MAX_ATTEMPTS: u32 = 3;
const FRODO_COOLDOWN: Duration = Duration::from_secs(60 * 60);

static DOUBAN_CONCURRENCY: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(DOUBAN_MAX_CONCURRENCY));
static DOUBAN_NEXT_SLOT: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
static FRODO_BLOCKED_UNTIL: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

async fn frodo_blocked() -> bool {
    FRODO_BLOCKED_UNTIL
        .lock()
        .await
        .is_some_and(|until| until > Instant::now())
}

async fn block_frodo() {
    *FRODO_BLOCKED_UNTIL.lock().await = Some(Instant::now() + FRODO_COOLDOWN);
    tracing::warn!("douban frodo API is rate limited; falling back to web suggest for an hour");
}

#[derive(Debug, thiserror::Error)]
pub enum DoubanError {
    #[error("豆瓣请求失败：{0}")]
    Transport(String),
    #[error("豆瓣返回 HTTP {0}")]
    Http(u16),
    #[error("豆瓣暂时限制了访问频率")]
    RateLimited,
    #[error("豆瓣返回的数据无法解析：{0}")]
    Parse(String),
}

/// 榜单条目展示用的豆瓣信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DoubanSubject {
    pub id: String,
    pub title: String,
    pub year: Option<u32>,
    pub rating: Option<f64>,
    /// 豆瓣海报图片编号（如 `p2936709341`）；搜索结果里的图片地址带时效签名，只保存编号。
    pub poster_id: Option<String>,
    pub url: String,
    pub media_type: TmdbMediaType,
}

/// 详情页里用于定位 TMDB 的信息。
#[derive(Debug, Clone, Default)]
pub struct DoubanDetail {
    pub original_title: Option<String>,
    pub aka: Vec<String>,
    pub imdb: Option<String>,
}

#[derive(Clone)]
pub struct DoubanClient {
    client: Client,
}

fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha1::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha1::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// frodo 签名：HMAC-SHA1(secret, "GET&<urlencoded path>&<ts>") 的 Base64。
fn sign(path: &str, ts: &str) -> String {
    let raw = format!("GET&{}&{ts}", urlencoding::encode(path));
    base64::engine::general_purpose::STANDARD.encode(hmac_sha1(API_SECRET, raw.as_bytes()))
}

fn today() -> String {
    let shanghai = FixedOffset::east_opt(8 * 3600).expect("UTC+8 is a valid offset");
    Utc::now().with_timezone(&shanghai).format("%Y%m%d").to_string()
}

async fn wait_for_slot() {
    let wait_until = {
        let mut next = DOUBAN_NEXT_SLOT.lock().await;
        let slot = (*next).max(Instant::now());
        *next = slot + DOUBAN_MIN_INTERVAL;
        slot
    };
    tokio::time::sleep_until(wait_until).await;
}

fn parse_year(value: Option<&Value>) -> Option<u32> {
    let text = match value? {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    text.get(..4)?
        .parse()
        .ok()
        .filter(|year| (1870..=2200).contains(year))
}

/// 去掉首尾空白和豆瓣标题里偶尔夹带的方向控制符（如 U+200E）。
fn string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|value| {
            value
                .chars()
                .filter(|character| !matches!(character, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{feff}'))
                .collect::<String>()
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn media_type_of(kind: &str) -> Option<TmdbMediaType> {
    match kind {
        "tv" => Some(TmdbMediaType::Tv),
        "movie" => Some(TmdbMediaType::Movie),
        _ => None,
    }
}

/// 从图片地址中提取海报编号：`.../public/p2936709341.jpg?...` → `p2936709341`。
fn poster_id(url: &str) -> Option<String> {
    let name = url.split('?').next()?.rsplit('/').next()?;
    let id = name.split('.').next()?;
    is_poster_id(id).then(|| id.to_string())
}

pub fn is_poster_id(id: &str) -> bool {
    id.len() > 1
        && id.len() <= 20
        && id.starts_with('p')
        && id[1..].bytes().all(|byte| byte.is_ascii_digit())
}

/// 豆瓣图片有防盗链，浏览器直接引用会被拒绝；由后端带豆瓣来源下载后以 data URL 返回。
pub async fn fetch_poster(client: &Client, id: &str) -> Result<String, DoubanError> {
    if !is_poster_id(id) {
        return Err(DoubanError::Parse("invalid poster id".to_string()));
    }
    let response = client
        .get(format!("https://img2.doubanio.com/view/photo/s_ratio_poster/public/{id}.jpg"))
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::REFERER, "https://movie.douban.com/")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|error| DoubanError::Transport(error.without_url().to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(DoubanError::Http(status.as_u16()));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.starts_with("image/"))
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| DoubanError::Transport(error.without_url().to_string()))?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(DoubanError::Parse("poster too large".to_string()));
    }
    Ok(format!(
        "data:{content_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

pub fn parse_search(payload: &Value) -> Vec<DoubanSubject> {
    let Some(items) = payload.get("items").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let target = item.get("target")?;
            let kind = item
                .get("target_type")
                .and_then(Value::as_str)
                .or_else(|| target.get("type").and_then(Value::as_str))?;
            let media_type = media_type_of(kind)?;
            let id = string(target.get("id")).or_else(|| string(item.get("target_id")))?;
            let title = string(target.get("title"))?;
            Some(DoubanSubject {
                url: format!("https://movie.douban.com/subject/{id}/"),
                id,
                title,
                year: parse_year(target.get("year")),
                rating: target
                    .pointer("/rating/value")
                    .and_then(Value::as_f64)
                    .filter(|value| *value > 0.0),
                poster_id: string(target.get("cover_url")).and_then(|url| poster_id(&url)),
                media_type,
            })
        })
        .collect()
}

/// 网页版联想接口：`episode` 非空的是剧集；没有评分。
pub fn parse_suggest(payload: &Value) -> Vec<DoubanSubject> {
    let Some(items) = payload.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let id = string(item.get("id"))?;
            if !id.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let title = string(item.get("title"))?;
            let media_type = if string(item.get("episode")).is_some() {
                TmdbMediaType::Tv
            } else {
                TmdbMediaType::Movie
            };
            Some(DoubanSubject {
                url: format!("https://movie.douban.com/subject/{id}/"),
                id,
                title,
                year: parse_year(item.get("year")),
                rating: None,
                poster_id: string(item.get("img")).and_then(|url| poster_id(&url)),
                media_type,
            })
        })
        .collect()
}

pub fn parse_detail(payload: &Value) -> DoubanDetail {
    DoubanDetail {
        original_title: string(payload.get("original_title")),
        aka: payload
            .get("aka")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| string(Some(value)))
                    .collect()
            })
            .unwrap_or_default(),
        imdb: string(payload.get("imdb")),
    }
}

impl DoubanClient {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, DoubanError> {
        if frodo_blocked().await {
            return Err(DoubanError::RateLimited);
        }
        let full_path = format!("{FRODO_PATH_PREFIX}{path}");
        let mut attempt = 0;
        loop {
            attempt += 1;
            let ts = today();
            let sig = sign(&full_path, &ts);
            let result = {
                let _permit = DOUBAN_CONCURRENCY
                    .acquire()
                    .await
                    .expect("douban semaphore is never closed");
                wait_for_slot().await;
                self.client
                    .get(format!("{FRODO_BASE}{full_path}"))
                    .query(query)
                    .query(&[
                        ("apiKey", API_KEY),
                        ("os_rom", "android"),
                        ("_ts", ts.as_str()),
                        ("_sig", sig.as_str()),
                    ])
                    .header(reqwest::header::USER_AGENT, USER_AGENT)
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await
            };
            let retryable = match result {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json::<Value>()
                        .await
                        .map_err(|error| DoubanError::Parse(error.to_string()));
                }
                Ok(response) => {
                    let status = response.status();
                    if matches!(status.as_u16(), 403 | 429) {
                        // need_login / 频率限制：重试只会延长封禁。
                        block_frodo().await;
                        return Err(DoubanError::RateLimited);
                    }
                    if !status.is_server_error() || attempt >= DOUBAN_MAX_ATTEMPTS {
                        return Err(DoubanError::Http(status.as_u16()));
                    }
                    true
                }
                Err(error) => {
                    if attempt >= DOUBAN_MAX_ATTEMPTS {
                        return Err(DoubanError::Transport(error.without_url().to_string()));
                    }
                    true
                }
            };
            if retryable {
                tokio::time::sleep(Duration::from_millis(800 * 2u64.pow(attempt - 1))).await;
            }
        }
    }

    /// 搜索影视：优先 frodo 接口（带评分），受限或失败时改用网页版联想接口。
    pub async fn search(&self, query: &str) -> Result<Vec<DoubanSubject>, DoubanError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        match self
            .get("/search/movie", &[("q", query), ("count", "10")])
            .await
        {
            Ok(payload) => Ok(parse_search(&payload)),
            Err(error) => {
                tracing::debug!(%error, query, "douban frodo search failed, trying web suggest");
                self.suggest(query).await
            }
        }
    }

    async fn suggest(&self, query: &str) -> Result<Vec<DoubanSubject>, DoubanError> {
        let response = {
            let _permit = DOUBAN_CONCURRENCY
                .acquire()
                .await
                .expect("douban semaphore is never closed");
            wait_for_slot().await;
            self.client
                .get(WEB_SUGGEST)
                .query(&[("q", query)])
                .header(reqwest::header::USER_AGENT, WEB_USER_AGENT)
                .header(reqwest::header::REFERER, "https://movie.douban.com/")
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .map_err(|error| DoubanError::Transport(error.without_url().to_string()))?
        };
        let status = response.status();
        if matches!(status.as_u16(), 403 | 429) {
            return Err(DoubanError::RateLimited);
        }
        if !status.is_success() {
            return Err(DoubanError::Http(status.as_u16()));
        }
        let payload = response
            .json::<Value>()
            .await
            .map_err(|error| DoubanError::Parse(error.to_string()))?;
        Ok(parse_suggest(&payload))
    }

    pub async fn detail(&self, subject: &DoubanSubject) -> Result<DoubanDetail, DoubanError> {
        let payload = self
            .get(&format!("/{}/{}", subject.media_type.as_str(), subject.id), &[])
            .await?;
        Ok(parse_detail(&payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hmac_sha1_matches_rfc2202() {
        let digest = hmac_sha1(b"Jefe", b"what do ya want for nothing?");
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
    }

    #[test]
    fn signature_encodes_the_path() {
        // 与开源实现一致：路径整体 URL 编码（斜杠也编码）。
        assert_eq!(
            format!("GET&{}&20261105", urlencoding::encode("/api/v2/search/movie")),
            "GET&%2Fapi%2Fv2%2Fsearch%2Fmovie&20261105"
        );
        assert!(!sign("/api/v2/search/movie", "20261105").is_empty());
    }

    #[test]
    fn parses_search_and_detail_payloads() {
        let payload = json!({"items": [
            {"target_type": "tv", "target_id": "36675942", "target": {
                "id": "36675942", "title": "小城良方", "year": "2026",
                "rating": {"value": 8.0}, "cover_url": "https://img/view/photo/large/public/p2936709341.jpg?sa_cv=1&x/format/webp"}},
            {"target_type": "doulist", "target": {"id": "1", "title": "片单"}},
            {"target_type": "movie", "target": {"id": "2", "title": "无评分", "year": "", "rating": {"value": 0}}}
        ]});
        let subjects = parse_search(&payload);
        assert_eq!(subjects.len(), 2);
        assert_eq!(subjects[0].media_type, TmdbMediaType::Tv);
        assert_eq!(subjects[0].year, Some(2026));
        assert_eq!(subjects[0].rating, Some(8.0));
        assert_eq!(subjects[1].rating, None);
        assert_eq!(subjects[1].year, None);
        assert_eq!(subjects[0].poster_id.as_deref(), Some("p2936709341"));
        assert!(!is_poster_id("p12/../x"));

        let suggest = parse_suggest(&json!([
            {"episode": "29", "img": "https://img2.doubanio.com/view/photo/s_ratio_poster/public/p2936709341.jpg", "title": "小城良方", "type": "movie", "year": "2026", "id": "36675942"},
            {"episode": "", "img": "", "title": "空枪", "type": "movie", "year": "2026", "id": "37508847"},
            {"title": "坏数据", "id": "abc"}
        ]));
        assert_eq!(suggest.len(), 2);
        assert_eq!(suggest[0].media_type, TmdbMediaType::Tv);
        assert_eq!(suggest[0].poster_id.as_deref(), Some("p2936709341"));
        assert_eq!(suggest[1].media_type, TmdbMediaType::Movie);
        assert_eq!(suggest[1].poster_id, None);

        let detail = parse_detail(&json!({"original_title": "", "aka": ["刘医生返乡记", " "], "imdb": "tt1234567"}));
        assert_eq!(detail.original_title, None);
        assert_eq!(detail.aka, vec!["刘医生返乡记".to_string()]);
        assert_eq!(detail.imdb.as_deref(), Some("tt1234567"));
    }

    /// 访问真实豆瓣接口，验证签名与解析：`cargo test --lib douban::tests::live -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn live_search_and_detail() {
        let client = DoubanClient::new(Client::new());
        let subjects = client.search("小城良方").await.expect("search");
        let subject = crate::media::maoyan::pick_douban("小城良方", &TmdbMediaType::Tv, subjects)
            .expect("matched");
        assert_eq!(subject.year, Some(2026));
        match client.detail(&subject).await {
            Ok(detail) => assert!(detail.aka.iter().any(|name| name == "刘医生返乡记"), "{detail:?}"),
            Err(DoubanError::RateLimited) => eprintln!("frodo is rate limited; detail skipped"),
            Err(error) => panic!("detail: {error}"),
        }
        let suggested = client.suggest("空枪").await.expect("suggest");
        assert!(crate::media::maoyan::pick_douban("空枪", &TmdbMediaType::Movie, suggested).is_some());
        let movie = client.search("生化危机：爆发夜").await.expect("movie search");
        assert!(crate::media::maoyan::pick_douban("生化危机：爆发夜", &TmdbMediaType::Movie, movie).is_some());
        let poster = fetch_poster(&Client::new(), subject.poster_id.as_deref().expect("poster id"))
            .await
            .expect("poster");
        assert!(poster.starts_with("data:image/"));
    }
}
