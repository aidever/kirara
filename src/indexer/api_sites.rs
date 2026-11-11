//! Torrent search and download for JSON API trackers, ported from PT-depiler:
//! `rousipro.ts` (Rousi / PeerGo API v1), `zhuque.ts` (TNode) and `sunnypt.ts`.
//!
//! Signed download links are resolved right before each download and never stored.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, COOKIE, HeaderMap, HeaderValue, LOCATION};
use reqwest::{Client, Method, RequestBuilder, StatusCode, Url};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::site::json_util::{at, integer, text, time_millis};
use crate::site::{SiteAuth, SiteType, browser_request_header_map};

use super::access::OriginAccessGate;
use super::{
    IndexerAdapter, IndexerCapabilities, IndexerError, IndexerFuture, SearchRequest, SearchResult,
    endpoint_url, ensure_result_site, http_error, normalize_base_url, parse_json_or_rate_limit,
    rate_limit_error_from_body, rate_limit_error_from_json, read_torrent_response, same_origin,
};

const MAX_DOWNLOAD_REDIRECTS: usize = 5;

pub struct ApiSiteIndexer {
    site_type: SiteType,
    site_id: i64,
    site_name: String,
    /// The site's web origin; detail links point here.
    site_url: Url,
    /// Where API requests go (SunnyPT uses `api.<host>/api/v1/mp`).
    api_url: Url,
    credential: HeaderValue,
    request_headers: HeaderMap,
    client: Client,
    access_gate: Arc<OriginAccessGate>,
    /// TNode requires the CSRF token from `/api/plugins/ptppUserInfo` on API calls.
    csrf_token: Mutex<Option<HeaderValue>>,
}

impl ApiSiteIndexer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        site_type: SiteType,
        site_id: i64,
        site_name: String,
        base_url: &str,
        auth: SiteAuth,
        request_headers: HeaderMap,
        client: Client,
        access_gate: Arc<OriginAccessGate>,
    ) -> Result<Self, IndexerError> {
        let site_url = normalize_base_url(base_url)?;
        let api_url = match site_type {
            SiteType::SunnyPt => normalize_base_url(
                &crate::site::api_sites::sunnypt_api_base(site_url.as_str())
                    .map_err(IndexerError::Configuration)?,
            )?,
            SiteType::Piggo => normalize_base_url(
                &crate::site::api_sites::piggo_api_base(site_url.as_str())
                    .map_err(IndexerError::Configuration)?,
            )?,
            _ => site_url.clone(),
        };
        let raw = match (site_type, auth) {
            (SiteType::Rousi | SiteType::Piggo, SiteAuth::ApiKey { api_key }) => {
                format!("Bearer {}", api_key.trim())
            }
            (SiteType::SunnyPt, SiteAuth::ApiKey { api_key }) => api_key.trim().to_string(),
            (
                SiteType::TNode,
                SiteAuth::Cookie { cookie } | SiteAuth::CookiePasskey { cookie, .. },
            ) => cookie.trim().to_string(),
            (SiteType::TNode, _) => {
                return Err(IndexerError::Configuration(
                    "TNode search requires a cookie".to_string(),
                ));
            }
            _ => {
                return Err(IndexerError::Configuration(
                    "this tracker requires an API key".to_string(),
                ));
            }
        };
        if raw.is_empty() || raw == "Bearer" || raw == "Bearer " {
            return Err(IndexerError::Configuration(
                "tracker credential cannot be empty".to_string(),
            ));
        }
        let mut credential = HeaderValue::from_str(&raw).map_err(|_| {
            IndexerError::Configuration(
                "tracker credential contains invalid header characters".to_string(),
            )
        })?;
        credential.set_sensitive(true);
        Ok(Self {
            site_type,
            site_id,
            site_name: site_name.trim().to_string(),
            site_url,
            api_url,
            credential,
            request_headers,
            client,
            access_gate,
            csrf_token: Mutex::new(None),
        })
    }

    fn label(&self) -> &'static str {
        match self.site_type {
            SiteType::Rousi => "Rousi Pro",
            SiteType::TNode => "TNode",
            SiteType::SunnyPt => "SunnyPT",
            SiteType::Piggo => "Piggo",
            _ => "tracker",
        }
    }

    fn headers(&self) -> HeaderMap {
        let mut headers = self.request_headers.clone();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json, */*"));
        match self.site_type {
            SiteType::Rousi | SiteType::Piggo => {
                headers.insert(AUTHORIZATION, self.credential.clone());
            }
            SiteType::SunnyPt => {
                headers.insert("x-api-key", self.credential.clone());
            }
            _ => {
                headers.insert(COOKIE, self.credential.clone());
            }
        }
        headers
    }

    async fn tnode_headers(&self, refresh: bool) -> Result<HeaderMap, IndexerError> {
        let mut headers = self.headers();
        let mut cached = self.csrf_token.lock().await;
        if refresh || cached.is_none() {
            let url = endpoint_url(&self.api_url, "/api/plugins/ptppUserInfo")?;
            let json = self
                .send_json(self.client.get(url).headers(self.headers()))
                .await?;
            let token = text(at(&json, "data.csrfToken")).ok_or_else(|| {
                IndexerError::AuthenticationExpired(
                    "TNode did not return a CSRF token; the cookie may be expired".to_string(),
                )
            })?;
            let mut token = HeaderValue::from_str(&token).map_err(|_| {
                IndexerError::Parse("TNode CSRF token is not a valid header".to_string())
            })?;
            token.set_sensitive(true);
            *cached = Some(token);
        }
        if let Some(token) = cached.as_ref() {
            headers.insert("x-csrf-token", token.clone());
        }
        Ok(headers)
    }

    async fn send_json(&self, request: RequestBuilder) -> Result<Value, IndexerError> {
        let response = self
            .access_gate
            .send_with_same_origin_redirects(&self.client, request, &self.api_url)
            .await?;
        let status = response.status();
        if !same_origin(&self.api_url, response.url()) {
            return Err(IndexerError::UnsafeUrl(format!(
                "{} API redirected to another origin",
                self.label()
            )));
        }
        let body = response.text().await.map_err(http_error)?;
        if let Some(error) = rate_limit_error_from_body(&body) {
            return Err(error);
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(IndexerError::AuthenticationExpired(format!(
                "{} API returned HTTP {status}",
                self.label()
            )));
        }
        if !status.is_success() {
            return Err(IndexerError::Http(format!(
                "{} API returned HTTP {status}",
                self.label()
            )));
        }
        let json = parse_json_or_rate_limit(&body, "tracker API returned invalid JSON")?;
        ensure_api_success(&json)?;
        Ok(json)
    }

    async fn search_api(&self, request: &SearchRequest) -> Result<Vec<SearchResult>, IndexerError> {
        let page = request.page.to_string();
        let page_size = request.page_size.to_string();
        let json = match self.site_type {
            SiteType::Rousi => {
                let url = endpoint_url(&self.api_url, "/api/v1/torrents")?;
                self.send_json(self.client.get(url).headers(self.headers()).query(&[
                    ("page", page.as_str()),
                    ("page_size", page_size.as_str()),
                    ("keyword", request.query.as_str()),
                ]))
                .await?
            }
            SiteType::Piggo => {
                // 实验室 API：所有接口都是 POST + JSON 请求体。
                let url = endpoint_url(&self.api_url, "/torrents")?;
                self.send_json(self.client.post(url).headers(self.headers()).json(&json!({
                    "q": request.query,
                    "page": request.page,
                    "per_page": request.page_size,
                })))
                .await?
            }
            SiteType::SunnyPt => {
                let url = endpoint_url(&self.api_url, "/torrents")?;
                self.send_json(self.client.get(url).headers(self.headers()).query(&[
                    ("page", page.as_str()),
                    ("page_size", page_size.as_str()),
                    ("sort", "created_at"),
                    ("order", "desc"),
                    ("keyword", request.query.as_str()),
                ]))
                .await?
            }
            _ => {
                let url = endpoint_url(&self.api_url, "/api/torrent/advancedSearch")?;
                let body = json!({
                    "page": request.page,
                    "size": request.page_size,
                    "type": "title",
                    "keyword": request.query,
                });
                let first = self
                    .send_json(
                        self.client
                            .post(url.clone())
                            .headers(self.tnode_headers(false).await?)
                            .json(&body),
                    )
                    .await;
                match first {
                    // 缓存的 CSRF Token 过期时刷新一次。
                    Err(IndexerError::AuthenticationExpired(_) | IndexerError::Api(_)) => {
                        self.send_json(
                            self.client
                                .post(url)
                                .headers(self.tnode_headers(true).await?)
                                .json(&body),
                        )
                        .await?
                    }
                    other => other?,
                }
            }
        };
        parse_search_results(
            self.site_type,
            &json,
            self.site_id,
            &self.site_name,
            &self.site_url,
        )
    }

    async fn download_url(&self, torrent_id: &str) -> Result<Url, IndexerError> {
        let id = urlencoding_path(torrent_id);
        match self.site_type {
            SiteType::Rousi => {
                let url = endpoint_url(&self.api_url, &format!("/api/v1/torrents/{id}"))?;
                let json = self
                    .send_json(self.client.get(url).headers(self.headers()))
                    .await?;
                let raw = text(at(&json, "data.download_url")).ok_or_else(|| {
                    IndexerError::Api(
                        "Rousi Pro returned no download URL (paid torrent or missing torrent:download scope)"
                            .to_string(),
                    )
                })?;
                self.resolve_download_url(&raw)
            }
            SiteType::SunnyPt => {
                let url = endpoint_url(&self.api_url, &format!("/torrents/{id}/download-token"))?;
                let json = self
                    .send_json(self.client.post(url).headers(self.headers()))
                    .await?;
                let raw = text(at(&json, "data.download_url")).ok_or_else(|| {
                    IndexerError::Api("SunnyPT returned no download URL".to_string())
                })?;
                // PTD：接口返回的站点域名链接需要换成 API 域名才能下载。
                let site_origin = self.site_url.origin().ascii_serialization();
                let api_origin = self.api_url.origin().ascii_serialization();
                let raw = match raw.strip_prefix(&site_origin) {
                    Some(rest) => format!("{api_origin}{rest}"),
                    None => raw,
                };
                self.resolve_download_url(&raw)
            }
            SiteType::Piggo => {
                // 签发约 24 小时有效、不含 passkey 的下载地址。
                let url = endpoint_url(&self.api_url, &format!("/torrents/{id}/download-link"))?;
                let json = self
                    .send_json(self.client.post(url).headers(self.headers()).json(&json!({})))
                    .await?;
                let raw = text(at(&json, "data.url")).ok_or_else(|| {
                    IndexerError::Api("Piggo returned no download URL".to_string())
                })?;
                self.resolve_download_url(&raw)
            }
            _ => endpoint_url(&self.api_url, &format!("/api/torrent/download/{id}")),
        }
    }

    /// Download links may live on the API host or a subdomain of the site; nothing else.
    fn resolve_download_url(&self, raw: &str) -> Result<Url, IndexerError> {
        let url = Url::parse(raw)
            .or_else(|_| self.api_url.join(raw))
            .map_err(|_| IndexerError::UnsafeUrl("download URL is invalid".to_string()))?;
        if is_allowed_download_url(&self.site_url, &self.api_url, &url) {
            Ok(url)
        } else {
            Err(IndexerError::UnsafeUrl(
                "download URL uses an untrusted origin".to_string(),
            ))
        }
    }

    fn download_headers(&self, url: &Url) -> HeaderMap {
        // Piggo 的下载地址自带签名，文档要求用普通 GET、不带令牌。
        if self.site_type != SiteType::Piggo
            && (same_origin(&self.api_url, url) || same_origin(&self.site_url, url))
        {
            let mut headers = self.headers();
            headers.insert(
                ACCEPT,
                HeaderValue::from_static("application/x-bittorrent, */*"),
            );
            return headers;
        }
        browser_request_header_map(&self.request_headers)
    }

    async fn fetch(&self, result: &SearchResult) -> Result<Vec<u8>, IndexerError> {
        let torrent_id = ensure_result_site(result, self.site_id)?;
        let mut url = self.download_url(&torrent_id).await?;
        for redirect_count in 0..=MAX_DOWNLOAD_REDIRECTS {
            if !is_allowed_download_url(&self.site_url, &self.api_url, &url) {
                return Err(IndexerError::UnsafeUrl(
                    "download redirected to an untrusted origin".to_string(),
                ));
            }
            let mut headers = self.download_headers(&url);
            if self.site_type == SiteType::TNode {
                if let Some(token) = self.csrf_token.lock().await.clone() {
                    headers.insert("x-csrf-token", token);
                }
            }
            let response = self
                .access_gate
                .send(
                    self.client
                        .request(Method::GET, url.clone())
                        .headers(headers),
                )
                .await?;
            if !response.status().is_redirection() {
                let final_url = response.url().clone();
                return read_torrent_response(response, &final_url).await;
            }
            if redirect_count == MAX_DOWNLOAD_REDIRECTS {
                break;
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| IndexerError::Http("download redirect has no location".into()))?;
            url = response
                .url()
                .join(location)
                .map_err(|_| IndexerError::UnsafeUrl("download redirect is invalid".into()))?;
        }
        Err(IndexerError::Http(
            "download exceeded the redirect limit".to_string(),
        ))
    }
}

fn urlencoding_path(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .collect()
}

fn site_host(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    Some(host.strip_prefix("api.").unwrap_or(host).to_string())
}

fn is_allowed_download_url(site_url: &Url, api_url: &Url, candidate: &Url) -> bool {
    if same_origin(site_url, candidate) || same_origin(api_url, candidate) {
        return true;
    }
    let (Some(root), Some(host)) = (site_host(site_url), candidate.host_str()) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    candidate.scheme() == "https"
        && candidate.username().is_empty()
        && candidate.password().is_none()
        && (host == root || host.ends_with(&format!(".{root}")))
}

fn ensure_api_success(json: &Value) -> Result<(), IndexerError> {
    if let Some(error) = rate_limit_error_from_json(json) {
        return Err(error);
    }
    crate::site::json_util::ensure_success(json, "tracker").map_err(IndexerError::Api)
}

fn parse_search_results(
    site_type: SiteType,
    json: &Value,
    site_id: i64,
    site_name: &str,
    site_url: &Url,
) -> Result<Vec<SearchResult>, IndexerError> {
    let rows = match site_type {
        SiteType::SunnyPt | SiteType::Piggo => at(json, "data.items"),
        _ => at(json, "data.torrents"),
    };
    let rows = rows.as_array().ok_or_else(|| {
        IndexerError::Parse("tracker search response is missing the torrent list".to_string())
    })?;
    let mut results = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(torrent_id) = text(&row["id"]) else {
            continue;
        };
        let Some(title) = text(&row["title"]).or_else(|| text(&row["name"])) else {
            continue;
        };
        let (seeders, leechers, time, detail) = match site_type {
            SiteType::TNode => (
                &row["seeding"],
                &row["leeching"],
                &row["upload_time"],
                format!("/torrent/info/{torrent_id}"),
            ),
            SiteType::Piggo => (
                &row["seeders"],
                &row["leechers"],
                &row["added_at"],
                format!("/detail/{torrent_id}"),
            ),
            SiteType::SunnyPt => (
                &row["seeders"],
                &row["leechers"],
                &row["created_at"],
                text(&row["details_url"]).unwrap_or_else(|| format!("/torrents/{torrent_id}")),
            ),
            _ => (
                &row["seeders"],
                &row["leechers"],
                &row["created_at"],
                format!("/torrents/{torrent_id}"),
            ),
        };
        let result = SearchResult {
            site_id,
            source_site: site_name.to_string(),
            torrent_id: torrent_id.clone(),
            title,
            subtitle: text(&row["subtitle"]).or_else(|| text(&row["small_descr"])),
            detail_url: Some(detail),
            download_locator: Some(torrent_id),
            magnet: None,
            size: crate::site::json_util::size_bytes(&row["size"]).unwrap_or(0),
            seeders: integer(seeders)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0),
            leechers: integer(leechers)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0),
            publish_time: time_millis(time).and_then(DateTime::<Utc>::from_timestamp_millis),
        };
        if let Ok(result) = result.sanitized_for_base(site_url) {
            results.push(result);
        }
    }
    Ok(results)
}

impl IndexerAdapter for ApiSiteIndexer {
    fn site_id(&self) -> i64 {
        self.site_id
    }

    fn site_name(&self) -> &str {
        &self.site_name
    }

    fn capabilities(&self) -> IndexerCapabilities {
        IndexerCapabilities {
            search: true,
            fetch_torrent: true,
            api_search: true,
            html_search: false,
        }
    }

    fn search<'a>(&'a self, request: &'a SearchRequest) -> IndexerFuture<'a, Vec<SearchResult>> {
        Box::pin(async move {
            let request = request.normalized()?;
            self.search_api(&request).await
        })
    }

    fn fetch_torrent<'a>(&'a self, result: &'a SearchResult) -> IndexerFuture<'a, Vec<u8>> {
        Box::pin(async move { self.fetch(result).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn parses_rousi_search_rows() {
        let json = json!({"code": 0, "data": {"torrents": [{
            "id": 123, "uuid": "123", "title": "Show S01E01 1080p WEB-DL", "subtitle": "第1集",
            "size": 1024, "seeders": 5, "leechers": 1, "created_at": "2026-11-01T10:00:00+08:00"
        }]}});
        let results = parse_search_results(
            SiteType::Rousi,
            &json,
            7,
            "Rousi",
            &url("https://rousi.pro"),
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].torrent_id, "123");
        assert_eq!(
            results[0].detail_url.as_deref(),
            Some("https://rousi.pro/torrents/123")
        );
        assert_eq!(results[0].subtitle.as_deref(), Some("第1集"));
        assert_eq!(results[0].seeders, 5);
        assert!(results[0].publish_time.is_some());
    }

    #[test]
    fn parses_tnode_and_sunnypt_rows() {
        let tnode = json!({"status": 200, "data": {"torrents": [{
            "id": 9, "title": "Movie 2026 2160p", "size": 2048, "seeding": 3, "leeching": 0,
            "upload_time": 1_762_000_000
        }]}});
        let results = parse_search_results(
            SiteType::TNode,
            &tnode,
            1,
            "朱雀",
            &url("https://zhuque.in"),
        )
        .unwrap();
        assert_eq!(
            results[0].detail_url.as_deref(),
            Some("https://zhuque.in/torrent/info/9")
        );
        assert_eq!(results[0].seeders, 3);

        let sunny = json!({"code": 0, "data": {"items": [{
            "id": 5, "title": "Show S02 2160p", "size": 1, "seeders": 2, "leechers": 1,
            "created_at": "2026-11-01 10:00:00", "details_url": "/details.php?id=5"
        }]}});
        let results = parse_search_results(
            SiteType::SunnyPt,
            &sunny,
            1,
            "Sunny",
            &url("https://sunnypt.top"),
        )
        .unwrap();
        assert_eq!(
            results[0].detail_url.as_deref(),
            Some("https://sunnypt.top/details.php?id=5")
        );
    }

    #[test]
    fn parses_piggo_lab_api_rows() {
        let json = json!({"code": 0, "msg": "ok", "data": {"items": [{
            "id": 123, "name": "Show S01E01 2160p WEB-DL", "small_descr": "副标题",
            "size": 1073741824, "seeders": 1, "leechers": 0, "times_completed": 10,
            "added_at": "2026-11-08 12:00:00", "page_url": "https://piggo.me/detail/123"
        }], "page": 1, "pages": 1, "total": 1}});
        let results =
            parse_search_results(SiteType::Piggo, &json, 3, "猪猪", &url("https://piggo.me"))
                .unwrap();
        assert_eq!(results[0].title, "Show S01E01 2160p WEB-DL");
        assert_eq!(results[0].subtitle.as_deref(), Some("副标题"));
        assert_eq!(results[0].size, 1_073_741_824);
        assert_eq!(
            results[0].detail_url.as_deref(),
            Some("https://piggo.me/detail/123")
        );
        assert!(results[0].publish_time.is_some());
        let site = url("https://piggo.me");
        let api = url("https://api.piggo.me/v1/api");
        assert!(is_allowed_download_url(
            &site,
            &api,
            &url("https://api.piggo.me/v1/web/torrents/123/dl/abc")
        ));
    }

    #[test]
    fn rejects_api_errors_and_missing_lists() {
        let error = json!({"code": 401, "message": "invalid api key"});
        assert!(matches!(
            ensure_api_success(&error),
            Err(IndexerError::Api(_))
        ));
        assert!(
            parse_search_results(
                SiteType::Rousi,
                &json!({"code": 0, "data": {}}),
                1,
                "x",
                &url("https://rousi.pro")
            )
            .is_err()
        );
    }

    #[test]
    fn download_urls_stay_on_the_site_or_its_subdomains() {
        let site = url("https://sunnypt.top");
        let api = url("https://api.sunnypt.top/api/v1/mp");
        assert!(is_allowed_download_url(
            &site,
            &api,
            &url("https://api.sunnypt.top/dl/1")
        ));
        assert!(is_allowed_download_url(
            &site,
            &api,
            &url("https://cdn.sunnypt.top/dl/1")
        ));
        assert!(!is_allowed_download_url(
            &site,
            &api,
            &url("https://evil.example/dl/1")
        ));
        assert!(!is_allowed_download_url(
            &site,
            &api,
            &url("http://cdn.sunnypt.top/dl/1")
        ));
        assert!(!is_allowed_download_url(
            &site,
            &api,
            &url("https://notsunnypt.top/dl/1")
        ));
    }
}
