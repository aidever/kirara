use std::sync::OnceLock;

use chrono::{Datelike, Utc};
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub raw_title: String,
    pub title: String,
    pub alternate_titles: Vec<String>,
    pub year: Option<u32>,
    pub season: Option<u32>,
    pub episodes: Vec<u32>,
    pub absolute_episodes: Vec<u32>,
    pub full_season: bool,
    pub resolution: Option<String>,
    pub codec: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub hdr_formats: Vec<String>,
    #[serde(default)]
    pub bit_depth: Option<u8>,
    /// 高帧率标记：`60fps`、`50fps`、`120fps`，只写 `HFR` 时为 `HFR`；常规帧率为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<String>,
    /// 平台的增强画质档：`HQ`（高码率）、`EDR`、`MAXPLUS`；普通版本为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_tier: Option<String>,
    /// 流媒体片源平台，如 `Netflix`、`Baha`、`friDay`；同一集在不同平台的码率差别很大。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub revision: Option<String>,
    pub release_group: Option<String>,
    pub matched_rule: String,
    /// 整季资源标注的总集数（如「全30集」），未标注时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_episodes: Option<u32>,
    /// 副标题里单独写出的年份（如「宿敌 | 2024 | 中国大陆」），用于排除同名的旧剧。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle_year: Option<u32>,
    /// 种子在站点上的发布年份。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_year: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ReleaseParseError {
    #[error("release title is empty")]
    EmptyTitle,
    #[error("episode range is reversed: {start}-{end}")]
    ReversedEpisodeRange { start: u32, end: u32 },
    #[error("episode range {start}-{end} exceeds maximum expansion of {maximum}")]
    EpisodeRangeTooLarge { start: u32, end: u32, maximum: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseParser {
    pub max_episode_range: u32,
    pub latest_year: u32,
}

impl Default for ReleaseParser {
    fn default() -> Self {
        Self {
            max_episode_range: 300,
            latest_year: (Utc::now().year() + 1) as u32,
        }
    }
}

impl ReleaseParser {
    #[allow(dead_code)]
    pub fn new(max_episode_range: u32) -> Self {
        Self {
            max_episode_range,
            ..Self::default()
        }
    }

    #[allow(dead_code)]
    pub fn with_limits(max_episode_range: u32, latest_year: u32) -> Self {
        Self {
            max_episode_range,
            latest_year,
        }
    }

    pub fn parse(&self, raw_title: &str) -> Result<ReleaseInfo, ReleaseParseError> {
        let raw_title = raw_title.trim();
        if raw_title.is_empty() {
            return Err(ReleaseParseError::EmptyTitle);
        }

        let resolution = parse_resolution(raw_title);
        let codec = parse_codec(raw_title);
        let source = parse_source(raw_title);
        let hdr_formats = parse_hdr_formats(raw_title);
        let bit_depth = parse_bit_depth(raw_title);
        let frame_rate = parse_frame_rate(raw_title);
        let quality_tier = parse_quality_tier(raw_title);
        let platform = parse_platform(raw_title);
        let mut revision = parse_revision(raw_title);
        let mut release_group = parse_release_group(raw_title);
        let parsed_year = self.parse_movie_year(raw_title);

        let mut season = None;
        let mut episodes = Vec::new();
        let mut absolute_episodes = Vec::new();
        let mut full_season = false;
        let year = parsed_year.map(|parsed| parsed.year);
        let matched_rule;
        let title;

        if let Some(parsed) = self.parse_s_episode(raw_title)? {
            season = Some(parsed.season);
            episodes = parsed.episodes;
            title = title_before(raw_title, parsed.marker_start);
            matched_rule = "standard_episode";
        } else if let Some(parsed) = self.parse_x_episode(raw_title)? {
            season = Some(parsed.season);
            episodes = parsed.episodes;
            title = title_before(raw_title, parsed.marker_start);
            matched_rule = "x_episode";
        } else if let Some(parsed) = self.parse_bare_episode(raw_title)? {
            season = parsed.season;
            episodes = parsed.episodes;
            title = title_before(raw_title, parsed.marker_start);
            matched_rule = "bare_episode";
        } else if let Some(parsed) = self.parse_chinese_episode(raw_title)? {
            season = parsed.season;
            episodes = parsed.episodes;
            title = title_before(raw_title, parsed.marker_start);
            matched_rule = "chinese_episode";
        } else if let Some(parsed) = self.parse_anime_absolute(raw_title)? {
            absolute_episodes = parsed.episodes;
            title = clean_title(&parsed.title);
            if parsed.release_group.is_some() {
                release_group = parsed.release_group;
            }
            if parsed.revision.is_some() {
                revision = parsed.revision;
            }
            matched_rule = "anime_absolute";
        } else if let Some(parsed) = parse_season_pack(raw_title) {
            season = Some(parsed.season);
            full_season = true;
            title = title_before(raw_title, parsed.marker_start);
            matched_rule = "season_pack";
        } else if let Some(parsed) = parsed_year {
            title = title_before(raw_title, parsed.marker_start);
            matched_rule = "movie";
        } else {
            let quality_start = first_quality_marker(raw_title).unwrap_or(raw_title.len());
            title = title_before(raw_title, quality_start);
            matched_rule = if resolution.is_some()
                || codec.is_some()
                || source.is_some()
                || !hdr_formats.is_empty()
                || bit_depth.is_some()
            {
                "quality_only"
            } else {
                "unknown"
            };
        }

        let title = if title.is_empty() {
            clean_title(raw_title)
        } else {
            title
        };
        let mut alternate_titles = Vec::new();
        let title = if matches!(
            matched_rule,
            "standard_episode" | "x_episode" | "bare_episode" | "chinese_episode" | "season_pack"
        ) {
            // 剧集常写成「Show 2026 S01E01」，年份不属于剧名。
            match self.strip_trailing_year(&title) {
                Some(stripped) => {
                    alternate_titles.push(title);
                    stripped
                }
                None => title,
            }
        } else {
            title
        };
        let total_episodes = if full_season {
            parse_total_episodes(raw_title)
        } else {
            None
        };

        Ok(ReleaseInfo {
            raw_title: raw_title.to_owned(),
            title,
            alternate_titles,
            year,
            season,
            episodes,
            absolute_episodes,
            full_season,
            resolution,
            codec,
            source,
            hdr_formats,
            bit_depth,
            frame_rate,
            quality_tier,
            platform,
            revision,
            release_group,
            matched_rule: matched_rule.to_owned(),
            total_episodes,
            subtitle_year: None,
            published_year: None,
        })
    }

    /// 解析种子标题，并在标题缺少集数时参考副标题。
    ///
    /// 国内 PT 站常把集数只写在副标题里，例如标题「Show 2026 S01 2160p WEB-DL」配副标题
    /// 「第1-8集」「更新至第8集」「全30集」。标题本身写明的集数始终优先。
    pub fn parse_with_subtitle(
        &self,
        raw_title: &str,
        subtitle: Option<&str>,
    ) -> Result<ReleaseInfo, ReleaseParseError> {
        let mut release = self.parse(raw_title)?;
        let Some(subtitle) = subtitle.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(release);
        };
        release.subtitle_year = self.subtitle_year(subtitle);
        if !release.episodes.is_empty() || !release.absolute_episodes.is_empty() {
            return Ok(release);
        }
        let episode_like = matches!(
            release.matched_rule.as_str(),
            "season_pack" | "movie" | "quality_only" | "unknown"
        );
        if !episode_like {
            return Ok(release);
        }
        let subtitle_season = chinese_season_regex()
            .captures(subtitle)
            .and_then(|captures| parse_chinese_number(&captures["number"]));
        let explicit = match self.parse_chinese_episode(subtitle) {
            Ok(Some(parsed)) => Some(parsed.episodes),
            _ => self.parse_updated_to(subtitle),
        };
        if let Some(episodes) = explicit {
            if release.matched_rule != "season_pack" {
                strip_movie_year(&mut release);
            }
            release.season = release.season.or(subtitle_season);
            release.episodes = episodes;
            release.full_season = false;
            release.total_episodes = None;
            release.matched_rule = "subtitle_episode".to_owned();
            return Ok(release);
        }
        if let Some(total) = parse_total_episodes(subtitle) {
            if release.matched_rule != "season_pack" && subtitle_season.is_none() {
                // 没有季标记也没有「第X季」，单凭「全X集」无法区分剧集与电影合集。
                if release.matched_rule == "movie" {
                    return Ok(release);
                }
            }
            if release.matched_rule != "season_pack" {
                strip_movie_year(&mut release);
                release.matched_rule = "subtitle_season_pack".to_owned();
            }
            release.season = release.season.or(subtitle_season);
            release.full_season = true;
            release.total_episodes = Some(total);
        } else if release.full_season {
            release.total_episodes = release.total_episodes.or(parse_total_episodes(subtitle));
        }
        Ok(release)
    }

    /// 「EP01」「E05」「EP01-EP10」「Episode 5」这类没有季号的写法。
    fn parse_bare_episode(&self, title: &str) -> Result<Option<ChineseEpisode>, ReleaseParseError> {
        for captures in bare_episode_regex().captures_iter(title) {
            let start_match = captures.name("start").expect("start capture");
            let marker = captures.name("marker").expect("marker capture");
            // 单字母「E」需要两位数字，避免把「E3」之类的片名当成集数。
            // 单字母「E」必须紧贴两到三位数字（E05），避免把「E3」「WALL·E 2008」当成集数。
            if marker.as_str().eq_ignore_ascii_case("e")
                && (marker.end() != start_match.start()
                    || !(2..=3).contains(&start_match.as_str().len()))
            {
                continue;
            }
            if clean_title(&title[..marker.start()]).is_empty() {
                continue;
            }
            let start = capture_number(&captures, "start");
            let episodes = match captures.name("end") {
                Some(end) => self.expand_range(start, end.as_str().parse().expect("digits"))?,
                None => vec![start],
            };
            let season = chinese_season_regex()
                .captures(title)
                .and_then(|captures| parse_chinese_number(&captures["number"]))
                .or_else(|| {
                    english_season_regex()
                        .captures(title)
                        .map(|captures| capture_number(&captures, "season"))
                });
            return Ok(Some(ChineseEpisode {
                season,
                episodes,
                marker_start: marker.start(),
            }));
        }
        Ok(None)
    }

    /// 「更新至第8集」「更至08集」：连载中的合集，视为第 1 集到该集。
    fn parse_updated_to(&self, text: &str) -> Option<Vec<u32>> {
        let captures = updated_to_regex().captures(text)?;
        let end = parse_chinese_number(&captures["number"])?;
        if end == 0 {
            return None;
        }
        self.expand_range(1, end).ok()
    }

    /// 副标题中以分隔符隔开的独立年份，例如「宿敌 | 2024 | 中国大陆」。
    fn subtitle_year(&self, subtitle: &str) -> Option<u32> {
        subtitle
            .split(|c: char| matches!(c, '|' | '｜' | '/' | '／' | '[' | ']' | '【' | '】') || c.is_whitespace())
            .map(str::trim)
            .filter(|token| token.len() == 4 && token.bytes().all(|byte| byte.is_ascii_digit()))
            .filter_map(|token| token.parse().ok())
            .find(|year| self.is_plausible_year(*year))
    }

    fn strip_trailing_year(&self, title: &str) -> Option<String> {
        let (head, last) = title.rsplit_once(' ')?;
        let year: u32 = last.parse().ok().filter(|_| last.len() == 4)?;
        if !self.is_plausible_year(year) {
            return None;
        }
        let head =
            head.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '(' | '['));
        let head = head.trim();
        (!head.is_empty()).then(|| head.to_owned())
    }

    fn parse_s_episode(&self, title: &str) -> Result<Option<NumberedEpisode>, ReleaseParseError> {
        if let Some(captures) = s_range_regex().captures(title) {
            return self.numbered_range(&captures, "season", "start", "end");
        }
        if let Some(captures) = s_multi_regex().captures(title) {
            let season = capture_number(&captures, "season");
            let mut episodes = vec![capture_number(&captures, "start")];
            let rest = captures.name("rest").expect("rest capture").as_str();
            episodes.extend(e_number_regex().captures_iter(rest).map(|capture| {
                capture[1]
                    .parse::<u32>()
                    .expect("regex only captures digits")
            }));
            episodes.sort_unstable();
            episodes.dedup();
            return Ok(Some(NumberedEpisode {
                season,
                episodes,
                marker_start: season_marker_start(&captures),
            }));
        }
        Ok(s_single_regex()
            .captures(title)
            .map(|captures| NumberedEpisode {
                season: capture_number(&captures, "season"),
                episodes: vec![capture_number(&captures, "start")],
                marker_start: season_marker_start(&captures),
            }))
    }

    fn parse_x_episode(&self, title: &str) -> Result<Option<NumberedEpisode>, ReleaseParseError> {
        if let Some(captures) = x_range_regex().captures(title) {
            return self.numbered_range(&captures, "season", "start", "end");
        }
        Ok(x_single_regex()
            .captures(title)
            .map(|captures| NumberedEpisode {
                season: capture_number(&captures, "season"),
                episodes: vec![capture_number(&captures, "start")],
                marker_start: captures.name("season").expect("season capture").start(),
            }))
    }

    fn numbered_range(
        &self,
        captures: &Captures<'_>,
        season_name: &str,
        start_name: &str,
        end_name: &str,
    ) -> Result<Option<NumberedEpisode>, ReleaseParseError> {
        let start = capture_number(captures, start_name);
        let end = capture_number(captures, end_name);
        let episodes = self.expand_range(start, end)?;
        Ok(Some(NumberedEpisode {
            season: capture_number(captures, season_name),
            episodes,
            marker_start: captures
                .name(season_name)
                .expect("season capture")
                .start()
                .saturating_sub(1),
        }))
    }

    fn parse_chinese_episode(
        &self,
        title: &str,
    ) -> Result<Option<ChineseEpisode>, ReleaseParseError> {
        let season_capture = chinese_season_regex().captures(title);
        let season = season_capture
            .as_ref()
            .and_then(|captures| parse_chinese_number(&captures["number"]));

        if let Some(captures) = chinese_episode_range_regex().captures(title) {
            let Some(start) = parse_chinese_number(&captures["start"]) else {
                return Ok(None);
            };
            let Some(end) = parse_chinese_number(&captures["end"]) else {
                return Ok(None);
            };
            let marker_start = season_capture
                .as_ref()
                .map(|capture| capture.get(0).expect("whole capture").start())
                .unwrap_or_else(|| captures.get(0).expect("whole capture").start());
            return Ok(Some(ChineseEpisode {
                season,
                episodes: self.expand_range(start, end)?,
                marker_start,
            }));
        }

        if let Some(captures) = updated_to_regex().captures(title)
            && let Some(episodes) = self.parse_updated_to(title)
        {
            let marker_start = season_capture
                .as_ref()
                .map(|capture| capture.get(0).expect("whole capture").start())
                .unwrap_or_else(|| captures.get(0).expect("whole capture").start());
            return Ok(Some(ChineseEpisode {
                season,
                episodes,
                marker_start,
            }));
        }

        Ok(chinese_episode_single_regex()
            .captures(title)
            .and_then(|captures| {
                let episode = parse_chinese_number(&captures["number"])?;
                let marker_start = season_capture
                    .as_ref()
                    .map(|capture| capture.get(0).expect("whole capture").start())
                    .unwrap_or_else(|| captures.get(0).expect("whole capture").start());
                Some(ChineseEpisode {
                    season,
                    episodes: vec![episode],
                    marker_start,
                })
            }))
    }

    fn parse_anime_absolute(&self, title: &str) -> Result<Option<AnimeEpisode>, ReleaseParseError> {
        let captures = anime_dash_regex()
            .captures(title)
            .or_else(|| anime_bracket_regex().captures(title));
        let Some(captures) = captures else {
            return Ok(None);
        };

        let start = capture_number(&captures, "start");
        let end = captures
            .name("end")
            .map(|value| value.as_str().parse().expect("regex only captures digits"));

        // A bare current-year-shaped number after a release group is more likely a movie year.
        if end.is_none() && captures.name("revision").is_none() && self.is_plausible_year(start) {
            return Ok(None);
        }

        let episodes = match end {
            Some(end) => self.expand_range(start, end)?,
            None => vec![start],
        };
        Ok(Some(AnimeEpisode {
            title: captures["title"].to_owned(),
            episodes,
            release_group: Some(captures["group"].trim().to_owned()),
            revision: captures
                .name("revision")
                .map(|revision| format!("v{}", revision.as_str())),
        }))
    }

    fn parse_movie_year(&self, title: &str) -> Option<MovieYear> {
        year_regex()
            .captures_iter(title)
            .filter_map(|captures| {
                let value = captures.name("year")?;
                let year = value.as_str().parse().ok()?;
                if !self.is_plausible_year(year) {
                    return None;
                }
                // Numeric movie names such as "1917" and "2001" are not release years
                // when the number is the first meaningful title token.
                if clean_title(&title[..value.start()]).is_empty() {
                    return None;
                }
                Some(MovieYear {
                    year,
                    marker_start: value.start(),
                })
            })
            .last()
    }

    fn is_plausible_year(&self, value: u32) -> bool {
        (1900..=self.latest_year).contains(&value)
    }

    fn expand_range(&self, start: u32, end: u32) -> Result<Vec<u32>, ReleaseParseError> {
        if end < start {
            return Err(ReleaseParseError::ReversedEpisodeRange { start, end });
        }
        let count = end - start + 1;
        if count > self.max_episode_range {
            return Err(ReleaseParseError::EpisodeRangeTooLarge {
                start,
                end,
                maximum: self.max_episode_range,
            });
        }
        Ok((start..=end).collect())
    }
}

struct NumberedEpisode {
    season: u32,
    episodes: Vec<u32>,
    marker_start: usize,
}

struct ChineseEpisode {
    season: Option<u32>,
    episodes: Vec<u32>,
    marker_start: usize,
}

struct AnimeEpisode {
    title: String,
    episodes: Vec<u32>,
    release_group: Option<String>,
    revision: Option<String>,
}

struct SeasonPack {
    season: u32,
    marker_start: usize,
}

#[derive(Clone, Copy)]
struct MovieYear {
    year: u32,
    marker_start: usize,
}

fn parse_season_pack(title: &str) -> Option<SeasonPack> {
    if let Some(captures) = s_season_regex().captures(title) {
        return Some(SeasonPack {
            season: capture_number(&captures, "season"),
            marker_start: season_marker_start(&captures),
        });
    }
    if let Some(captures) = english_season_regex().captures(title) {
        return Some(SeasonPack {
            season: capture_number(&captures, "season"),
            marker_start: captures.get(0).expect("whole capture").start(),
        });
    }
    if let Some(pack) = chinese_season_regex().captures(title).and_then(|captures| {
        Some(SeasonPack {
            season: parse_chinese_number(&captures["number"])?,
            marker_start: captures.get(0).expect("whole capture").start(),
        })
    }) {
        return Some(pack);
    }
    // 「Show 全30集」「Show 全集」没有季号时按第一季整季处理。
    complete_episodes_regex().find(title).and_then(|found| {
        (!clean_title(&title[..found.start()]).is_empty()).then_some(SeasonPack {
            season: 1,
            marker_start: found.start(),
        })
    })
}

fn parse_total_episodes(text: &str) -> Option<u32> {
    if let Some(captures) = total_episodes_regex().captures(text) {
        return parse_chinese_number(&captures["number"]).filter(|total| *total > 0);
    }
    english_total_regex()
        .captures(text)
        .and_then(|captures| captures["number"].parse().ok())
        .filter(|total| *total > 0)
}

/// 副标题证明这是剧集后，把误当成电影年份的数字从片名里去掉。
fn strip_movie_year(release: &mut ReleaseInfo) {
    if let Some(year) = release.year {
        let suffix = format!(" {year}");
        if let Some(stripped) = release.title.strip_suffix(&suffix)
            && !stripped.trim().is_empty()
        {
            let stripped = stripped.trim().to_owned();
            release
                .alternate_titles
                .push(std::mem::replace(&mut release.title, stripped));
        }
    }
}

fn capture_number(captures: &Captures<'_>, name: &str) -> u32 {
    captures[name]
        .parse()
        .expect("numeric regex capture must parse")
}

fn season_marker_start(captures: &Captures<'_>) -> usize {
    captures
        .name("season")
        .expect("season capture")
        .start()
        .saturating_sub(1)
}

fn title_before(title: &str, marker_start: usize) -> String {
    clean_title(&title[..marker_start])
}

fn clean_title(title: &str) -> String {
    let mut title = title.trim();
    while let Some(captures) = leading_group_regex().captures(title) {
        let whole = captures.get(0).expect("whole capture");
        if whole.start() != 0 {
            break;
        }
        title = title[whole.end()..].trim_start();
    }

    let replaced: String = title
        .chars()
        .map(|character| match character {
            '.' | '_' => ' ',
            _ => character,
        })
        .collect();
    replaced
        .trim_matches(|character: char| {
            character.is_whitespace() || matches!(character, '-' | '[' | ']' | '(' | ')')
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_chinese_number(value: &str) -> Option<u32> {
    if value.chars().all(|character| character.is_ascii_digit()) {
        return value.parse().ok();
    }

    let mut result = 0_u32;
    let mut digit = 0_u32;
    for character in value.chars() {
        match character {
            '零' | '〇' => digit = 0,
            '一' => digit = 1,
            '二' | '两' => digit = 2,
            '三' => digit = 3,
            '四' => digit = 4,
            '五' => digit = 5,
            '六' => digit = 6,
            '七' => digit = 7,
            '八' => digit = 8,
            '九' => digit = 9,
            '十' => {
                result += digit.max(1) * 10;
                digit = 0;
            }
            '百' => {
                result += digit.max(1) * 100;
                digit = 0;
            }
            '千' => {
                result += digit.max(1) * 1_000;
                digit = 0;
            }
            _ => return None,
        }
    }
    Some(result + digit)
}

fn parse_resolution(title: &str) -> Option<String> {
    // An explicit scan height ("1080p") is authoritative. Marketing tokens such as "4K"
    // or "UHD" also appear in titles of 1080p re-encodes ("4K修复版 1080p"), so they are
    // only used when no explicit height or frame size is present.
    if let Some(captures) = explicit_resolution_regex().captures(title) {
        return Some(normalize_resolution_token(&captures["value"]));
    }
    if let Some(captures) = frame_size_regex().captures(title) {
        let height = &captures["height"];
        return Some(
            match height {
                "4320" => "4320p",
                "2160" => "2160p",
                "1440" => "1440p",
                "1080" => "1080p",
                "720" => "720p",
                "576" => "576p",
                _ => "480p",
            }
            .to_owned(),
        );
    }
    resolution_regex()
        .captures(title)
        .map(|captures| normalize_resolution_token(&captures["value"]))
}

fn normalize_resolution_token(value: &str) -> String {
    {
        let compact = value.to_ascii_lowercase();
        match compact.as_str() {
            "8k" | "4320p" => "4320p",
            "4k" | "uhd" | "2160" | "2160p" | "2160i" => "2160p",
            "2k" | "1440p" => "1440p",
            "1080p" | "1080i" => "1080p",
            "720p" => "720p",
            "576p" | "576i" => "576p",
            "480p" | "480i" => "480p",
            _ => unreachable!("resolution regex and mapping must stay aligned"),
        }
        .to_owned()
    }
}

fn parse_codec(title: &str) -> Option<String> {
    if av1_regex().is_match(title) {
        Some("AV1".into())
    } else if h265_regex().is_match(title) {
        Some("H265".into())
    } else if h264_regex().is_match(title) {
        Some("H264".into())
    } else if xvid_regex().is_match(title) {
        Some("XviD".into())
    } else if mpeg2_regex().is_match(title) {
        Some("MPEG2".into())
    } else {
        None
    }
}

fn parse_source(title: &str) -> Option<String> {
    if remux_regex().is_match(title) {
        Some("REMUX".into())
    } else if web_dl_regex().is_match(title) {
        Some("WEB-DL".into())
    } else if web_rip_regex().is_match(title) {
        Some("WEBRip".into())
    } else if bluray_regex().is_match(title) {
        Some("BluRay".into())
    } else if hdtv_regex().is_match(title) {
        Some("HDTV".into())
    } else if dvd_regex().is_match(title) {
        Some("DVD".into())
    } else if web_bare_regex().is_match(title) {
        // Scene and P2P releases commonly label web downloads as plain "WEB".
        Some("WEB-DL".into())
    } else if cam_regex().is_match(title) {
        Some("CAM".into())
    } else {
        None
    }
}

fn parse_hdr_formats(title: &str) -> Vec<String> {
    let mut formats = Vec::new();
    if dolby_vision_regex().is_match(title) {
        formats.push("Dolby Vision".to_owned());
    }
    if hdr10_plus_regex().is_match(title) {
        formats.push("HDR10+".to_owned());
    } else if hdr10_regex().is_match(title) {
        formats.push("HDR10".to_owned());
    }
    if hlg_regex().is_match(title) {
        formats.push("HLG".to_owned());
    }
    if hdr_vivid_regex().is_match(title) {
        formats.push("HDR Vivid".to_owned());
    } else if hdr_regex().is_match(title) {
        formats.push("HDR".to_owned());
    }
    formats
}

/// 高帧率：明确写出 48 帧及以上时用数字，只写 HFR 时记为 `HFR`。
fn parse_frame_rate(title: &str) -> Option<String> {
    let explicit = fps_regex()
        .captures_iter(title)
        .filter_map(|captures| captures["value"].parse::<u32>().ok())
        .find(|value| (48..=240).contains(value));
    match explicit {
        Some(value) => Some(format!("{value}fps")),
        None => hfr_regex().is_match(title).then(|| "HFR".to_owned()),
    }
}

/// 片源平台。只在第一个画质标记（分辨率、编码等）之后查找，避免把剧名里的
/// 「Max」「Hulu」之类当成平台；简写（NF、TX、IQ、CR、iT）同样只在这一段里认。
fn parse_platform(title: &str) -> Option<String> {
    let start = first_quality_marker(title)?;
    let tail = &title[start..];
    platform_regex().captures(tail).map(|captures| {
        let token = captures["value"].to_ascii_lowercase();
        let compact: String = token.chars().filter(char::is_ascii_alphanumeric).collect();
        match compact.as_str() {
            "nf" | "netflix" => "Netflix",
            "amzn" | "amazon" => "Amazon",
            "dsnp" | "disney" => "Disney+",
            "atvp" => "Apple TV+",
            "hmax" | "max" => "Max",
            "it" | "itunes" => "iTunes",
            "hulu" => "Hulu",
            "pcok" | "peacock" => "Peacock",
            "pmtp" => "Paramount+",
            "cr" | "crunchyroll" => "Crunchyroll",
            "baha" => "Baha",
            "friday" => "friDay",
            "linetv" => "LINE TV",
            "hamivideo" => "Hami Video",
            "myvideo" => "myVideo",
            "nowplayer" => "Now",
            "mytvsuper" => "myTV SUPER",
            "kktv" => "KKTV",
            "viu" => "Viu",
            "catchplay" => "CATCHPLAY",
            "youku" => "Youku",
            "iq" | "iqiyi" => "iQIYI",
            "tx" | "tencent" | "wetv" => "Tencent",
            "mgtv" => "MGTV",
            "bilibili" | "bglobal" => "Bilibili",
            _ => unreachable!("platform regex and mapping must stay aligned"),
        }
        .to_owned()
    })
}

/// 平台增强画质档：爱奇艺/优酷的 HQ（高码率）、优酷 EDR、腾讯 MAXPLUS。
fn parse_quality_tier(title: &str) -> Option<String> {
    if max_plus_regex().is_match(title) {
        Some("MAXPLUS".to_owned())
    } else if hq_regex().is_match(title) {
        Some("HQ".to_owned())
    } else if edr_regex().is_match(title) {
        Some("EDR".to_owned())
    } else {
        None
    }
}

fn parse_bit_depth(title: &str) -> Option<u8> {
    if hi10p_regex().is_match(title) {
        return Some(10);
    }
    bit_depth_regex()
        .captures(title)
        .and_then(|captures| captures["value"].parse().ok())
}

fn parse_revision(title: &str) -> Option<String> {
    if let Some(captures) = attached_revision_regex().captures(title) {
        return Some(captures["value"].to_ascii_lowercase());
    }
    revision_regex()
        .captures_iter(title)
        .find(|captures| {
            let start = captures.name("value").expect("value capture").start();
            !clean_title(&title[..start]).is_empty()
        })
        .map(|captures| {
            let value = captures["value"].to_owned();
            if value.to_ascii_lowercase().starts_with('v') {
                value.to_ascii_lowercase()
            } else {
                value.to_ascii_uppercase()
            }
        })
}

fn parse_release_group(title: &str) -> Option<String> {
    if let Some(captures) = leading_group_regex().captures(title) {
        return Some(captures["group"].trim().to_owned());
    }
    let captures = trailing_group_regex().captures(title)?;
    let group = captures["group"].to_owned();
    let key = group.to_ascii_lowercase();
    if matches!(
        key.as_str(),
        "dl" | "rip" | "remux" | "proper" | "repack" | "h264" | "h265" | "x264" | "x265"
    ) {
        None
    } else {
        Some(group)
    }
}

fn first_quality_marker(title: &str) -> Option<usize> {
    let quality_marker = [
        resolution_regex(),
        av1_regex(),
        h265_regex(),
        h264_regex(),
        xvid_regex(),
        mpeg2_regex(),
        remux_regex(),
        web_dl_regex(),
        web_rip_regex(),
        bluray_regex(),
        hdtv_regex(),
        dvd_regex(),
        cam_regex(),
        dolby_vision_regex(),
        hdr10_plus_regex(),
        hdr10_regex(),
        hlg_regex(),
        hdr_regex(),
        bit_depth_regex(),
        hi10p_regex(),
        attached_revision_regex(),
    ]
    .into_iter()
    .filter_map(|regex| {
        regex
            .find(title)
            .map(|found| marker_start(title, found.start()))
    })
    .min();
    let revision_marker = revision_regex().captures_iter(title).find_map(|captures| {
        let start = captures.name("value").expect("value capture").start();
        (!clean_title(&title[..start]).is_empty()).then_some(start)
    });

    quality_marker.into_iter().chain(revision_marker).min()
}

fn marker_start(title: &str, start: usize) -> usize {
    let Some(character) = title[start..].chars().next() else {
        return start;
    };
    if character.is_alphanumeric() {
        start
    } else {
        start + character.len_utf8()
    }
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("release parser regex must compile"))
}

macro_rules! regex_fn {
    ($name:ident, $pattern:literal) => {
        fn $name() -> &'static Regex {
            static CELL: OnceLock<Regex> = OnceLock::new();
            regex(&CELL, $pattern)
        }
    };
}

regex_fn!(
    s_range_regex,
    r"(?i)(?:^|[^a-z0-9])s(?P<season>\d{1,2})[ ._-]*(?:episode|ep?)[ ._]?(?P<start>\d{1,4})[ ._]*(?:-|~|to)[ ._]*(?:s\d{1,2}[ ._-]*)?(?:(?:episode|ep?)[ ._]?)?(?P<end>\d{1,4})(?:v[2-9]\d*)?(?:[^a-z0-9]|$)"
);
regex_fn!(
    s_multi_regex,
    r"(?i)(?:^|[^a-z0-9])s(?P<season>\d{1,2})[ ._-]*(?:episode|ep?)[ ._]?(?P<start>\d{1,4})(?P<rest>(?:[ ._+&-]*ep?\d{1,4})+)(?:v[2-9]\d*)?(?:[^a-z0-9]|$)"
);
regex_fn!(
    s_single_regex,
    r"(?i)(?:^|[^a-z0-9])s(?P<season>\d{1,2})[ ._-]*(?:episode|ep?)[ ._]?(?P<start>\d{1,4})(?:v[2-9]\d*)?(?:[^a-z0-9]|$)"
);
regex_fn!(e_number_regex, r"(?i)ep?(\d{1,4})");
regex_fn!(
    bare_episode_regex,
    r"(?i)(?:^|[^a-z0-9])(?P<marker>episode|ep|e)[ ._]?(?P<start>\d{1,4})(?:[ ._]*(?:-|~|to)[ ._]*(?:(?:episode|ep|e)[ ._]?)?(?P<end>\d{1,4}))?(?:v[2-9]\d*)?(?:[^a-z0-9]|$)"
);
regex_fn!(
    updated_to_regex,
    r"更新?\s*(?:至|到)\s*第?\s*(?P<number>[0-9零〇一二两三四五六七八九十百千]+)\s*[集话期]?"
);
regex_fn!(
    total_episodes_regex,
    r"全\s*(?P<number>[0-9零〇一二两三四五六七八九十百千]+)\s*[集话期]"
);
regex_fn!(
    english_total_regex,
    r"(?i)(?:^|[^a-z0-9])(?P<number>\d{1,4})[ ._-]*episodes(?:[^a-z0-9]|$)"
);
regex_fn!(
    complete_episodes_regex,
    r"全\s*[0-9零〇一二两三四五六七八九十百千]*\s*[集话]"
);
regex_fn!(
    x_range_regex,
    r"(?i)(?:^|[^a-z0-9])(?P<season>\d{1,2})x(?P<start>\d{2,4})[ ._]*(?:-|~|to)[ ._]*(?P<end>\d{2,4})(?:v[2-9]\d*)?(?:[^a-z0-9]|$)"
);
regex_fn!(
    x_single_regex,
    r"(?i)(?:^|[^a-z0-9])(?P<season>\d{1,2})x(?P<start>\d{2,4})(?:v[2-9]\d*)?(?:[^a-z0-9]|$)"
);
regex_fn!(
    chinese_season_regex,
    r"第\s*(?P<number>[0-9零〇一二两三四五六七八九十百千]+)\s*季"
);
regex_fn!(
    chinese_episode_range_regex,
    r"第\s*(?P<start>[0-9零〇一二两三四五六七八九十百千]+)\s*(?:[集话]\s*)?(?:-|~|至|到)\s*(?:第\s*)?(?P<end>[0-9零〇一二两三四五六七八九十百千]+)\s*[集话]"
);
regex_fn!(
    chinese_episode_single_regex,
    r"第\s*(?P<number>[0-9零〇一二两三四五六七八九十百千]+)\s*[集话]"
);
regex_fn!(
    anime_dash_regex,
    r"(?i)^\s*\[(?P<group>[^\]\r\n]{1,80})\]\s*(?P<title>.+?)\s+-\s+(?P<start>\d{1,4})(?:\s*(?:-|~)\s*(?P<end>\d{1,4}))?(?:v(?P<revision>\d+))?(?:\s|[\[(.]|$)"
);
regex_fn!(
    anime_bracket_regex,
    r"(?i)^\s*\[(?P<group>[^\]\r\n]{1,80})\]\s*(?P<title>.+?)\s+\[(?P<start>\d{1,4})(?:\s*(?:-|~)\s*(?P<end>\d{1,4}))?(?:v(?P<revision>\d+))?\](?:\s|$)"
);
regex_fn!(
    s_season_regex,
    r"(?i)(?:^|[^a-z0-9])s(?P<season>\d{1,2})(?:[^a-z0-9]|$)"
);
regex_fn!(
    english_season_regex,
    r"(?i)(?:^|[^a-z0-9])season[ ._-]*(?P<season>\d{1,2})(?:[^a-z0-9]|$)"
);
regex_fn!(
    year_regex,
    r"(?:^|[^0-9])(?P<year>(?:19|20|21)\d{2})(?:[^0-9]|$)"
);
regex_fn!(
    resolution_regex,
    r"(?i)(?:^|[^a-z0-9])(?P<value>4320p|8k|2160[pi]|4k|uhd|1440p|2k|1080[pi]|720p|576[pi]|480[pi])(?:[^a-z0-9]|$)"
);
regex_fn!(
    explicit_resolution_regex,
    r"(?i)(?:^|[^a-z0-9])(?P<value>4320p|2160[pi]|1440p|1080[pi]|720p|576[pi]|480[pi])(?:[^a-z0-9]|$)"
);
regex_fn!(
    frame_size_regex,
    r"(?i)(?:^|[^0-9])(?:7680|4096|3840|2560|1920|1440|1280|1024|720|640)\s*[x×]\s*(?P<height>4320|2160|1440|1080|720|576|480)(?:[^0-9]|$)"
);
regex_fn!(web_bare_regex, r"(?:^|[^A-Za-z0-9])WEB(?:[^A-Za-z0-9]|$)");
regex_fn!(av1_regex, r"(?i)(?:^|[^a-z0-9])(?:av1|av01)(?:[^a-z0-9]|$)");
regex_fn!(
    h265_regex,
    r"(?i)(?:^|[^a-z0-9])(?:h[ ._-]*265|x265|hevc)(?:[^a-z0-9]|$)"
);
regex_fn!(
    h264_regex,
    r"(?i)(?:^|[^a-z0-9])(?:h[ ._-]*264|x264|avc)(?:[^a-z0-9]|$)"
);
regex_fn!(xvid_regex, r"(?i)(?:^|[^a-z0-9])xvid(?:[^a-z0-9]|$)");
regex_fn!(
    mpeg2_regex,
    r"(?i)(?:^|[^a-z0-9])mpeg[ ._-]*2(?:[^a-z0-9]|$)"
);
regex_fn!(remux_regex, r"(?i)(?:^|[^a-z0-9])remux(?:[^a-z0-9]|$)");
regex_fn!(
    web_dl_regex,
    r"(?i)(?:^|[^a-z0-9])web[ ._-]*dl(?:[^a-z0-9]|$)"
);
regex_fn!(
    web_rip_regex,
    r"(?i)(?:^|[^a-z0-9])web[ ._-]*rip(?:[^a-z0-9]|$)"
);
regex_fn!(
    bluray_regex,
    r"(?i)(?:^|[^a-z0-9])(?:blu[ ._-]*ray|bd[ ._-]*(?:rip|remux)|brrip|bdmv)(?:[^a-z0-9]|$)"
);
regex_fn!(hdtv_regex, r"(?i)(?:^|[^a-z0-9])hdtv(?:[^a-z0-9]|$)");
regex_fn!(
    dvd_regex,
    r"(?i)(?:^|[^a-z0-9])(?:dvd(?:rip)?|r5)(?:[^a-z0-9]|$)"
);
regex_fn!(
    cam_regex,
    r"(?i)(?:^|[^a-z0-9])(?:hdcam|cam|telesync|ts)(?:[^a-z0-9]|$)"
);
regex_fn!(
    dolby_vision_regex,
    r"(?i)(?:^|[^a-z0-9])(?:dv|dovi|dolby[ ._-]*vision)(?:[^a-z0-9]|$)"
);
regex_fn!(
    hdr10_plus_regex,
    r"(?i)(?:^|[^a-z0-9])hdr[ ._-]*10(?:[ ._-]*plus|\+)(?:[^a-z0-9]|$)"
);
regex_fn!(
    hdr10_regex,
    r"(?i)(?:^|[^a-z0-9])hdr[ ._-]*10(?:[^a-z0-9+]|$)"
);
regex_fn!(hlg_regex, r"(?i)(?:^|[^a-z0-9])hlg(?:[^a-z0-9]|$)");
regex_fn!(hdr_regex, r"(?i)(?:^|[^a-z0-9])hdr(?:[^a-z0-9]|$)");
regex_fn!(
    hdr_vivid_regex,
    r"(?i)(?:^|[^a-z0-9])hdr[ ._-]*vivid(?:[^a-z0-9]|$)"
);
regex_fn!(
    fps_regex,
    // 数字前只要求不是数字，以兼容「1080p60fps」这类连写。
    r"(?i)(?:^|[^0-9])(?P<value>\d{2,3})[ ._-]*(?:fps|帧率?)(?:[^a-z0-9]|$)"
);
regex_fn!(hfr_regex, r"(?i)(?:^|[^a-z0-9])hfr(?:[^a-z0-9]|$)");
regex_fn!(
    platform_regex,
    r"(?i)(?:^|[^a-z0-9+])(?P<value>nf|netflix|amzn|amazon|dsnp|disney\+?|atvp|hmax|max|it|itunes|hulu|pcok|peacock|pmtp|cr|crunchyroll|baha|friday|line[ ._-]?tv|hami[ ._-]?video|myvideo|nowplayer|mytvsuper|kktv|viu|catchplay|youku|iq|iqiyi|tx|tencent|wetv|mgtv|bilibili|b[ ._-]?global)(?:[^a-z0-9+]|$)"
);
regex_fn!(hq_regex, r"(?i)(?:^|[^a-z0-9])hq(?:[^a-z0-9]|$)");
regex_fn!(edr_regex, r"(?i)(?:^|[^a-z0-9])edr(?:[^a-z0-9]|$)");
regex_fn!(
    max_plus_regex,
    r"(?i)(?:^|[^a-z0-9])max[ ._-]*plus(?:[^a-z0-9]|$)"
);
regex_fn!(
    bit_depth_regex,
    r"(?i)(?:^|[^a-z0-9])(?P<value>8|10|12)[ ._-]*bit(?:[^a-z0-9]|$)"
);
regex_fn!(hi10p_regex, r"(?i)(?:^|[^a-z0-9])hi10p(?:[^a-z0-9]|$)");
regex_fn!(
    revision_regex,
    r"(?i)(?:^|[^a-z0-9])(?P<value>repack\d*|proper|real|v[2-9]\d*)(?:[^a-z0-9]|$)"
);
regex_fn!(
    attached_revision_regex,
    r"(?i)(?:s\d{1,2}e\d{1,4}|\d{1,2}x\d{2,4}|\d{1,4})(?P<value>v[2-9]\d*)(?:[^a-z0-9]|$)"
);
regex_fn!(
    leading_group_regex,
    r"^\s*\[(?P<group>[^\]\r\n]{1,80})\]\s*"
);
regex_fn!(
    trailing_group_regex,
    r"-(?P<group>[A-Za-z][A-Za-z0-9]{1,20})$"
);

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> ReleaseParser {
        ReleaseParser::with_limits(20, 2027)
    }

    #[test]
    fn parses_standard_episode_single_multi_and_ranges() {
        let single = parser()
            .parse("Example.Show.S01E02.1080p.WEB-DL.x265-GROUP")
            .unwrap();
        assert_eq!(single.title, "Example Show");
        assert_eq!(single.season, Some(1));
        assert_eq!(single.episodes, vec![2]);
        assert_eq!(single.release_group.as_deref(), Some("GROUP"));

        let multi = parser().parse("Example Show S01E02E03.E05").unwrap();
        assert_eq!(multi.episodes, vec![2, 3, 5]);

        let range = parser().parse("Example Show S01E02-E04").unwrap();
        assert_eq!(range.episodes, vec![2, 3, 4]);
    }

    #[test]
    fn parses_x_episode_but_not_3x3_eyes() {
        let episode = parser().parse("Example.Show.1x02.720p").unwrap();
        assert_eq!(episode.matched_rule, "x_episode");
        assert_eq!(episode.season, Some(1));
        assert_eq!(episode.episodes, vec![2]);

        let numeric_title = parser().parse("3x3 Eyes 1080p BluRay").unwrap();
        assert_eq!(numeric_title.matched_rule, "quality_only");
        assert_eq!(numeric_title.title, "3x3 Eyes");
        assert!(numeric_title.episodes.is_empty());
    }

    #[test]
    fn rejects_reversed_and_oversized_ranges_before_allocation() {
        assert!(matches!(
            parser().parse("Show S01E05-E03"),
            Err(ReleaseParseError::ReversedEpisodeRange { start: 5, end: 3 })
        ));
        assert!(matches!(
            parser().parse("Show S01E01-E99"),
            Err(ReleaseParseError::EpisodeRangeTooLarge { maximum: 20, .. })
        ));
    }

    #[test]
    fn parses_chinese_season_episode_and_spelled_numbers() {
        let release = parser().parse("庆余年 第2季 第3-5集 2160p WEB-DL").unwrap();
        assert_eq!(release.title, "庆余年");
        assert_eq!(release.season, Some(2));
        assert_eq!(release.episodes, vec![3, 4, 5]);

        let spelled = parser().parse("动画 第十二话 1080p").unwrap();
        assert_eq!(spelled.episodes, vec![12]);

        let repeated_markers = parser().parse("动画 第三话-第五话 1080p").unwrap();
        assert_eq!(repeated_markers.episodes, vec![3, 4, 5]);
    }

    #[test]
    fn parses_anime_absolute_episode_and_revision() {
        let release = parser()
            .parse("[Lilith-Raws] One Piece - 1122-1124v2 [Baha][WEB-DL][1080p][AVC]")
            .unwrap();

        assert_eq!(release.title, "One Piece");
        assert_eq!(release.absolute_episodes, vec![1122, 1123, 1124]);
        assert_eq!(release.release_group.as_deref(), Some("Lilith-Raws"));
        assert_eq!(release.revision.as_deref(), Some("v2"));
        assert_eq!(release.matched_rule, "anime_absolute");
    }

    #[test]
    fn parses_season_packs() {
        let western = parser().parse("Example Show S02 Complete 1080p").unwrap();
        assert_eq!(western.season, Some(2));
        assert!(western.full_season);

        let chinese = parser().parse("庆余年 第二季 全集 2160p").unwrap();
        assert_eq!(chinese.season, Some(2));
        assert!(chinese.full_season);
    }

    #[test]
    fn parses_movie_and_all_quality_fields() {
        let release = parser()
            .parse("Dune.Part.Two.2024.2160p.UHD.BluRay.REMUX.DV.HDR10+.HEVC.10bit.PROPER-GROUP")
            .unwrap();

        assert_eq!(release.title, "Dune Part Two");
        assert_eq!(release.year, Some(2024));
        assert_eq!(release.resolution.as_deref(), Some("2160p"));
        assert_eq!(release.codec.as_deref(), Some("H265"));
        assert_eq!(release.source.as_deref(), Some("REMUX"));
        assert_eq!(release.hdr_formats, vec!["Dolby Vision", "HDR10+"]);
        assert_eq!(release.bit_depth, Some(10));
        assert_eq!(release.revision.as_deref(), Some("PROPER"));
    }

    #[test]
    fn parses_frame_rate_quality_tier_and_hdr_vivid_from_web_titles() {
        let cases: [(&str, Option<&str>, Option<&str>, &[&str]); 10] = [
            (
                "The Silent Storm 2026 S01 E05-07 2160p Youku WEB-DL HQ HFR H265 AAC-Momoda",
                Some("HFR"),
                Some("HQ"),
                &[],
            ),
            (
                "The Silent Storm 2026 S01 E09-10 2160p Youku WEB-DL HFR H265 AAC-Momoda",
                Some("HFR"),
                None,
                &[],
            ),
            (
                "The Silent Storm 2026 S01E4 2160p WEB-DL 60fps H265 HDR10 AAC-AilMWeb",
                Some("60fps"),
                None,
                &["HDR10"],
            ),
            (
                "The Silent Storm 2026 S01E5 2160p HQ WEB-DL H265 HDR10 AAC-AilMWeb",
                None,
                Some("HQ"),
                &["HDR10"],
            ),
            (
                "Lady Liberty 2026 S01E13-S01E14 2160p WEB-DL 60Fps MAXPLUS HDRVivid H265 10bit AAC-UBWEB",
                Some("60fps"),
                Some("MAXPLUS"),
                &["HDR Vivid"],
            ),
            (
                "The Epoch of Miyu 2026 S01E01-E04 2160p 60fps WEB-DL H.265 10bit HDR Vivid DDP5.1 4Audios-QHstudIo",
                Some("60fps"),
                None,
                &["HDR Vivid"],
            ),
            (
                "Chang An 2023 IMAX 2160p 60fps HQ WEB-DL H.265 10bit HDR10 DTS-X5.1.4 5Audios-QHstudIo",
                Some("60fps"),
                Some("HQ"),
                &["HDR10"],
            ),
            (
                "The Sword Immortal Is Here S02 2022 2160p WEB-DL H.265 EDR AAC 2.0-HHWEB",
                None,
                Some("EDR"),
                &[],
            ),
            (
                "FIFA World Cup 2026 Spain vs Saudi Arabia 20260622 1080p60fps ES Telemundo",
                Some("60fps"),
                None,
                &[],
            ),
            (
                "Show.S01E01.2160p.WEB-DL.DDP5.1.H265-GROUP",
                None,
                None,
                &[],
            ),
        ];
        for (title, frame_rate, tier, hdr) in cases {
            let release = parser().parse(title).unwrap();
            assert_eq!(release.frame_rate.as_deref(), frame_rate, "{title}");
            assert_eq!(release.quality_tier.as_deref(), tier, "{title}");
            assert_eq!(release.hdr_formats, hdr, "{title}");
        }
    }

    #[test]
    fn parses_streaming_platform_only_after_quality_markers() {
        let cases = [
            ("Bungo Stray Dogs Wan S02E02 1080p Baha WEB-DL AAC2.0 H.264-MWeb", Some("Baha")),
            ("Hana-Kimi S02E03 1080p friDay WEB-DL AAC2.0 H.264-MWeb", Some("friDay")),
            ("Hana-Kimi S02E03 1080p LINETV WEB-DL AAC2.0 H.264-MWeb", Some("LINE TV")),
            ("Keep Running S10E13 2160p TX WEB-DL AAC2.0 HFR H.265-MWeb", Some("Tencent")),
            ("Born with Luck 2026 S01E01-S01E05 2160p IQ WEB-DL H265 DDP5.1-UBWEB", Some("iQIYI")),
            ("Pressure 2026 2160p iT WEB-DL DDP 5.1 Atmos HDR10 H.265-DepWeb", Some("iTunes")),
            ("Show.S01E01.2160p.NF.WEB-DL.DDP5.1.H.265-GROUP", Some("Netflix")),
            ("Filing for Love S01 1080p MAX WEB-DL H.265 DDP2.0-ADWeb", Some("Max")),
            ("Mad Max Fury Road 2015 2160p UHD BluRay REMUX HEVC-GROUP", None),
            ("Lady Liberty 2026 S01E13 2160p WEB-DL 60Fps MAXPLUS H265-UBWEB", None),
        ];
        for (title, platform) in cases {
            let release = parser().parse(title).unwrap();
            assert_eq!(release.platform.as_deref(), platform, "{title}");
        }
    }

    #[test]
    fn parses_hdr_variants_without_confusing_dolby_audio() {
        let hdr = parser()
            .parse("Example.Show.S01E01.2160p.WEB-DL.HDR10.HLG.HEVC.12-bit")
            .unwrap();
        assert_eq!(hdr.hdr_formats, vec!["HDR10", "HLG"]);
        assert_eq!(hdr.bit_depth, Some(12));

        let atmos = parser()
            .parse("Example.Show.S01E01.1080p.WEB-DL.DDP5.1.Atmos.H264")
            .unwrap();
        assert!(atmos.hdr_formats.is_empty());
        assert_eq!(atmos.bit_depth, None);

        let anime = parser()
            .parse("[Group] Example - 01 [1080p][Hi10P][HDR]")
            .unwrap();
        assert_eq!(anime.hdr_formats, vec!["HDR"]);
        assert_eq!(anime.bit_depth, Some(10));
    }

    #[test]
    fn parses_revision_attached_to_episode_number() {
        let standard = parser()
            .parse("Example Show S01E02v2 1080p WEB-DL")
            .unwrap();
        assert_eq!(standard.episodes, vec![2]);
        assert_eq!(standard.revision.as_deref(), Some("v2"));

        let x_style = parser().parse("Example Show 1x02v3 720p").unwrap();
        assert_eq!(x_style.episodes, vec![2]);
        assert_eq!(x_style.revision.as_deref(), Some("v3"));
    }

    #[test]
    fn numeric_titles_are_not_episode_or_year_tokens() {
        let the_100 = parser().parse("The 100 1080p WEB-DL H264").unwrap();
        assert_eq!(the_100.title, "The 100");
        assert_eq!(the_100.year, None);
        assert!(the_100.episodes.is_empty());

        let blade_runner = parser()
            .parse("Blade.Runner.2049.1080p.BluRay.x265")
            .unwrap();
        assert_eq!(blade_runner.title, "Blade Runner 2049");
        assert_eq!(blade_runner.year, None);
        assert!(blade_runner.absolute_episodes.is_empty());

        let real_steel = parser().parse("Real Steel 1080p BluRay").unwrap();
        assert_eq!(real_steel.title, "Real Steel");
        assert_eq!(real_steel.revision, None);
    }

    #[test]
    fn a_numeric_movie_name_at_the_start_is_not_assumed_to_be_a_year() {
        let release = parser().parse("2001 A Space Odyssey 1080p BluRay").unwrap();
        assert_eq!(release.title, "2001 A Space Odyssey");
        assert_eq!(release.year, None);

        let with_year = parser()
            .parse("2001 A Space Odyssey 1968 1080p BluRay")
            .unwrap();
        assert_eq!(with_year.title, "2001 A Space Odyssey");
        assert_eq!(with_year.year, Some(1968));
    }

    #[test]
    fn release_info_round_trips_through_json() {
        let release = parser().parse("Show S01E01 1080p WEB-DL").unwrap();
        let json = serde_json::to_string(&release).unwrap();
        let restored: ReleaseInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, release);
    }

    #[test]
    fn old_release_snapshots_default_new_video_fields() {
        let value = serde_json::json!({
            "raw_title": "Show S01E01 1080p WEB-DL",
            "title": "Show",
            "alternate_titles": [],
            "year": null,
            "season": 1,
            "episodes": [1],
            "absolute_episodes": [],
            "full_season": false,
            "resolution": "1080p",
            "codec": null,
            "source": "WEB-DL",
            "revision": null,
            "release_group": null,
            "matched_rule": "standard_episode"
        });
        let restored: ReleaseInfo = serde_json::from_value(value).unwrap();
        assert!(restored.hdr_formats.is_empty());
        assert_eq!(restored.bit_depth, None);
    }

    #[test]
    fn explicit_resolution_wins_over_marketing_tokens() {
        let release = parser()
            .parse("Example Show 4K修复版 S01E01 1080p WEB-DL H264")
            .unwrap();
        assert_eq!(release.resolution.as_deref(), Some("1080p"));

        let uhd_source = parser()
            .parse("Example.Movie.2020.UHD.BluRay.1080p.x264-GROUP")
            .unwrap();
        assert_eq!(uhd_source.resolution.as_deref(), Some("1080p"));

        let only_4k = parser().parse("Example Show S01E01 4K WEB-DL").unwrap();
        assert_eq!(only_4k.resolution.as_deref(), Some("2160p"));
    }

    #[test]
    fn frame_sizes_map_to_resolutions() {
        let release = parser()
            .parse("Example Show S01E01 3840x2160 WEB-DL HEVC")
            .unwrap();
        assert_eq!(release.resolution.as_deref(), Some("2160p"));
        let release = parser().parse("Example Show 第3集 1920×1080 HDTV").unwrap();
        assert_eq!(release.resolution.as_deref(), Some("1080p"));
    }

    #[test]
    fn bare_uppercase_web_is_a_web_download() {
        let release = parser()
            .parse("Example.Show.S01E01.1080p.WEB.h264-GROUP")
            .unwrap();
        assert_eq!(release.source.as_deref(), Some("WEB-DL"));
        assert_eq!(release.title, "Example Show");

        let titled = parser().parse("The.Web.S01E01.1080p.HDTV.x264").unwrap();
        assert_eq!(titled.source.as_deref(), Some("HDTV"));
    }

    #[test]
    fn parses_common_pt_episode_spellings() {
        let cases: &[(&str, Option<u32>, &[u32])] = &[
            (
                "The Silent Storm 2026 S01 E09-10 2160p Youku WEB-DL HFR H265 AAC-Momoda",
                Some(1),
                &[9, 10],
            ),
            (
                "The Silent Storm 2026 S01 E01-08 2160p Youku WEB-DL HFR H265 AAC-Momoda",
                Some(1),
                &[1, 2, 3, 4, 5, 6, 7, 8],
            ),
            (
                "Gu Yu Jie Hou Xiang S01E05 1080p TX WEB-DL AAC2.0 H.264-MWeb",
                Some(1),
                &[5],
            ),
            ("Show.S01E01-E03.2160p.WEB-DL", Some(1), &[1, 2, 3]),
            ("Show S01EP01-EP03 2160p WEB-DL", Some(1), &[1, 2, 3]),
            ("Show S01E01-S01E03 1080p WEB-DL", Some(1), &[1, 2, 3]),
            ("Show S02 Ep05 1080p WEB-DL", Some(2), &[5]),
            ("Show.S01.E07.1080p.WEB-DL", Some(1), &[7]),
            ("Show EP01-EP04 2160p WEB-DL", None, &[1, 2, 3, 4]),
            ("Show E05 2160p WEB-DL", None, &[5]),
            ("Show Ep.12 1080p WEB-DL", None, &[12]),
            ("Show 第2季 第05集 1080p", Some(2), &[5]),
            (
                "Show 更新至第8集 2160p WEB-DL",
                None,
                &[1, 2, 3, 4, 5, 6, 7, 8],
            ),
        ];
        for (title, season, episodes) in cases {
            let release = parser().parse(title).unwrap();
            assert_eq!(release.season, *season, "{title}");
            assert_eq!(release.episodes, episodes.to_vec(), "{title}");
            assert!(!release.full_season, "{title}");
        }
    }

    #[test]
    fn episode_titles_drop_the_release_year() {
        let release = parser()
            .parse("The Silent Storm 2026 S01 E09-10 2160p Youku WEB-DL HFR H265 AAC-Momoda")
            .unwrap();
        assert_eq!(release.title, "The Silent Storm");
        assert_eq!(release.year, Some(2026));
        assert_eq!(release.alternate_titles, vec!["The Silent Storm 2026"]);
        assert_eq!(release.release_group.as_deref(), Some("Momoda"));

        let numeric = parser().parse("1923 S02E01 2160p WEB-DL").unwrap();
        assert_eq!(numeric.title, "1923");
    }

    #[test]
    fn single_letter_e_needs_attached_digits() {
        let movie = parser().parse("WALL·E 2008 1080p BluRay x264").unwrap();
        assert!(movie.episodes.is_empty());
        assert_eq!(movie.year, Some(2008));

        let codec = parser()
            .parse("Movie 2020 1080p WEB-DL E-AC3 H264")
            .unwrap();
        assert!(codec.episodes.is_empty());
    }

    #[test]
    fn complete_markers_record_total_episodes() {
        let release = parser().parse("庆余年 全46集 2160p WEB-DL").unwrap();
        assert!(release.full_season);
        assert_eq!(release.total_episodes, Some(46));
        assert_eq!(release.title, "庆余年");

        let english = parser().parse("Show S01 Complete 2160p WEB-DL").unwrap();
        assert!(english.full_season);
        assert_eq!(english.total_episodes, None);
    }

    #[test]
    fn subtitles_fill_in_missing_episode_numbers() {
        let in_progress = parser()
            .parse_with_subtitle(
                "Show 2026 S01 2160p WEB-DL H265-GROUP",
                Some("谷雨 | 第1-8集 | 国语"),
            )
            .unwrap();
        assert_eq!(in_progress.season, Some(1));
        assert_eq!(in_progress.episodes, (1..=8).collect::<Vec<_>>());
        assert!(!in_progress.full_season);
        assert_eq!(in_progress.title, "Show");

        let updated = parser()
            .parse_with_subtitle("Show 2026 S01 2160p WEB-DL", Some("更新至第12集"))
            .unwrap();
        assert_eq!(updated.episodes.len(), 12);

        let complete = parser()
            .parse_with_subtitle("Show 2026 S01 2160p WEB-DL", Some("全30集 | 国语中字"))
            .unwrap();
        assert!(complete.full_season);
        assert_eq!(complete.total_episodes, Some(30));

        let no_season_marker = parser()
            .parse_with_subtitle("Show 2026 2160p WEB-DL H265-GROUP", Some("谷雨 第9-10集"))
            .unwrap();
        assert_eq!(no_season_marker.episodes, vec![9, 10]);
        assert_eq!(no_season_marker.title, "Show");
        assert_eq!(no_season_marker.matched_rule, "subtitle_episode");

        let explicit_title_wins = parser()
            .parse_with_subtitle("Show S01E05 2160p WEB-DL", Some("第1-8集"))
            .unwrap();
        assert_eq!(explicit_title_wins.episodes, vec![5]);

        let movie = parser()
            .parse_with_subtitle("Movie 2024 2160p WEB-DL", Some("全3集 合集"))
            .unwrap();
        assert_eq!(movie.matched_rule, "movie");
        assert!(!movie.full_season);
    }
}
