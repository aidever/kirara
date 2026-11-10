//! 识别预处理：站点上的季/集编号与 TMDB 不一致时，把解析出的编号换算回 TMDB 编号。
//!
//! 只改解析结果，不改写种子名：标题照常解析出 `S03E02`，再按规则换算成 `S01E42`，
//! 原始标题保留用于展示，匹配说明里写明换算过程。

use serde::{Deserialize, Serialize};

use super::release::ReleaseInfo;

/// 一条编号映射：标题（可选关键词）里季号为 `source_season` 的资源，
/// 视为 TMDB 第 `target_season` 季，集号加上 `episode_offset`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NumberingRule {
    /// 标题或副标题包含该关键词（不区分大小写）时才生效；为空时对所有资源生效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyword: Option<String>,
    pub source_season: u32,
    pub target_season: u32,
    #[serde(default)]
    pub episode_offset: i32,
}

pub const MAX_NUMBERING_RULES: usize = 8;
const MAX_KEYWORD_CHARS: usize = 100;
const MAX_SEASON: u32 = 1000;
const MAX_OFFSET: i32 = 10_000;

impl NumberingRule {
    pub fn validate(&self) -> Result<(), String> {
        if self.source_season > MAX_SEASON || self.target_season > MAX_SEASON {
            return Err("numbering season is out of range".to_string());
        }
        if self.episode_offset.abs() > MAX_OFFSET {
            return Err("numbering episode_offset is out of range".to_string());
        }
        if self
            .keyword
            .as_deref()
            .is_some_and(|keyword| keyword.chars().count() > MAX_KEYWORD_CHARS)
        {
            return Err("numbering keyword is too long".to_string());
        }
        if self.source_season == self.target_season && self.episode_offset == 0 {
            return Err("numbering rule does not change anything".to_string());
        }
        Ok(())
    }

    fn keyword_matches(&self, release: &ReleaseInfo, subtitle: Option<&str>) -> bool {
        let Some(keyword) = self
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|keyword| !keyword.is_empty())
        else {
            return true;
        };
        let keyword = keyword.to_lowercase();
        release.raw_title.to_lowercase().contains(&keyword)
            || subtitle.is_some_and(|subtitle| subtitle.to_lowercase().contains(&keyword))
    }

    /// TMDB 的季/集反推回站点上的编号，用于生成搜索词。
    pub fn site_numbering(&self, season: u32, episode: u32) -> Option<(u32, u32)> {
        if season != self.target_season {
            return None;
        }
        let episode = i64::from(episode) - i64::from(self.episode_offset);
        u32::try_from(episode)
            .ok()
            .filter(|episode| *episode > 0)
            .map(|episode| (self.source_season, episode))
    }
}

/// 去掉空关键词、非法和重复的规则，最多保留 [`MAX_NUMBERING_RULES`] 条。
pub fn normalize_rules(rules: Vec<NumberingRule>) -> Result<Vec<NumberingRule>, String> {
    let mut normalized: Vec<NumberingRule> = Vec::new();
    for mut rule in rules {
        rule.keyword = rule
            .keyword
            .map(|keyword| keyword.trim().to_string())
            .filter(|keyword| !keyword.is_empty());
        rule.validate()?;
        if !normalized.contains(&rule) {
            normalized.push(rule);
        }
    }
    if normalized.len() > MAX_NUMBERING_RULES {
        return Err(format!(
            "at most {MAX_NUMBERING_RULES} numbering rules are allowed"
        ));
    }
    Ok(normalized)
}

/// 按第一条命中的规则换算编号；命中时记录换算说明，返回 `true`。
pub fn apply(rules: &[NumberingRule], release: &mut ReleaseInfo, subtitle: Option<&str>) -> bool {
    let Some(season) = release.season else {
        return false;
    };
    for rule in rules {
        if rule.source_season != season || !rule.keyword_matches(release, subtitle) {
            continue;
        }
        let mapped: Option<Vec<u32>> = release
            .episodes
            .iter()
            .map(|episode| {
                u32::try_from(i64::from(*episode) + i64::from(rule.episode_offset))
                    .ok()
                    .filter(|episode| *episode > 0)
            })
            .collect();
        // 换算后出现第 0 集或负数，说明这条规则不适用于该资源。
        let Some(mapped) = mapped else {
            continue;
        };
        let before = numbering_label(Some(season), &release.episodes);
        release.season = Some(rule.target_season);
        release.episodes = mapped;
        let after = numbering_label(release.season, &release.episodes);
        release.numbering_note = Some(format!("{before} → {after}"));
        return true;
    }
    false
}

fn numbering_label(season: Option<u32>, episodes: &[u32]) -> String {
    let season = season.map(|season| format!("S{season:02}")).unwrap_or_default();
    match episodes {
        [] => season,
        [episode] => format!("{season}E{episode:02}"),
        [first, .., last] => format!("{season}E{first:02}-E{last:02}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::domain::release::ReleaseParser;

    fn rule(keyword: Option<&str>, source: u32, target: u32, offset: i32) -> NumberingRule {
        NumberingRule {
            keyword: keyword.map(str::to_string),
            source_season: source,
            target_season: target,
            episode_offset: offset,
        }
    }

    #[test]
    fn maps_site_season_and_episode_to_tmdb_numbering() {
        let mut release = ReleaseParser::default()
            .parse("The Legend of LUOXIAOHEI 2011 S03E02 4K Bilibili WEB-DL H.264 AAC")
            .unwrap();
        assert!(apply(&[rule(None, 3, 1, 40)], &mut release, None));
        assert_eq!(release.season, Some(1));
        assert_eq!(release.episodes, vec![42]);
        assert_eq!(release.numbering_note.as_deref(), Some("S03E02 → S01E42"));
    }

    #[test]
    fn keyword_limits_the_rule_and_other_seasons_are_untouched() {
        let parser = ReleaseParser::default();
        let mut other = parser.parse("Example S03E02 1080p WEB-DL-ABC").unwrap();
        assert!(!apply(&[rule(Some("luoxiaohei"), 3, 1, 40)], &mut other, None));
        assert_eq!(other.season, Some(3));

        let mut by_subtitle = parser.parse("Example S03E02 1080p WEB-DL-ABC").unwrap();
        assert!(apply(
            &[rule(Some("罗小黑"), 3, 1, 40)],
            &mut by_subtitle,
            Some("罗小黑战记 第42集")
        ));

        let mut season_two = parser.parse("Example S02E02 1080p WEB-DL-ABC").unwrap();
        assert!(!apply(&[rule(None, 3, 1, 40)], &mut season_two, None));
        assert_eq!(season_two.episodes, vec![2]);
    }

    #[test]
    fn negative_offset_skips_episodes_that_would_fall_below_one() {
        let parser = ReleaseParser::default();
        let rules = [rule(None, 2, 1, -10)];
        let mut early = parser.parse("Example S02E03 1080p WEB-DL-ABC").unwrap();
        assert!(!apply(&rules, &mut early, None));
        let mut later = parser.parse("Example S02E13 1080p WEB-DL-ABC").unwrap();
        assert!(apply(&rules, &mut later, None));
        assert_eq!(later.episodes, vec![3]);
    }

    #[test]
    fn reverse_mapping_produces_site_numbering_for_queries() {
        let mapping = rule(None, 3, 1, 40);
        assert_eq!(mapping.site_numbering(1, 42), Some((3, 2)));
        assert_eq!(mapping.site_numbering(1, 40), None);
        assert_eq!(mapping.site_numbering(2, 42), None);
    }

    #[test]
    fn normalize_rejects_noop_and_trims_keywords() {
        assert!(normalize_rules(vec![rule(None, 1, 1, 0)]).is_err());
        let rules = normalize_rules(vec![rule(Some("  "), 3, 1, 40), rule(None, 3, 1, 40)]).unwrap();
        assert_eq!(rules, vec![rule(None, 3, 1, 40)]);
    }
}
