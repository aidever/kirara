//! 制作组优先的追更策略：按用户排好的制作组顺序挑选每集资源。
//!
//! 第一个制作组是首选，其余是备选。首选组没有发布时，从其他组第一次出现该集资源开始
//! 等待 `wait_hours`，仍然没有首选组版本才退而求其次。列表外的制作组（包括无法识别组名的
//! 资源）按 `outside` 处理：按质量配置兜底或不下载。
//!
//! 每个条目还可以限定分辨率、编码、动态范围（杜比视界 / HDR / SDR）、帧率、画质档（HQ 等）和片源平台，例如把
//! 「CHDWEB · 2160p · H265 · Dolby Vision」排在「CHDWEB · 1080p」前面。没有限定的维度不参与匹配。

use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

pub const MAX_GROUPS: usize = 24;
pub const MAX_RULES: usize = 32;
pub const MAX_WAIT_HOURS: u32 = 24 * 14;
const MAX_GROUP_LEN: usize = 80;
const MAX_RULE_LEN: usize = 300;
const MAX_TRAIT_LEN: usize = 40;
/// 没有任何 HDR 标记的资源在动态范围维度上的取值。
pub const SDR_LABEL: &str = "SDR";
/// 没有高帧率标记的资源在帧率维度上的取值。
pub const STANDARD_FRAME_RATE: &str = "常规帧率";
/// 没有 HQ / EDR / MAXPLUS 等标记的资源在画质档维度上的取值。
pub const STANDARD_TIER: &str = "普通档";
/// 标题没写平台的资源在平台维度上的取值。
pub const UNKNOWN_PLATFORM: &str = "未标平台";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutsideGroups {
    /// 列表内都没有时，按质量配置从其他制作组中挑选。
    #[default]
    Quality,
    /// 只下载列表内的制作组。
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupPolicy {
    /// 按优先级排列，第一个为首选。为空时不按制作组挑选（旧行为）。
    pub groups: Vec<GroupEntry>,
    /// 首选组缺席时，从其他组首次发布该集算起的等待小时数。
    pub wait_hours: u32,
    /// 用备选或列表外版本下载后，首选（或更靠前的）组发布时自动洗版。
    pub upgrade: bool,
    /// 洗版完成后删除旧版本的种子和文件；否则保留做种并打标签。
    pub delete_replaced: bool,
    pub outside: OutsideGroups,
    /// 每行一个正则；`{{组名}}:正则` 只对该组生效。适用的规则必须全部匹配。
    pub include: Vec<String>,
    /// 每行一个正则；`{{组名}}:正则` 只对该组生效。任意一条匹配即排除。
    pub exclude: Vec<String>,
}

impl Default for GroupPolicy {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            wait_hours: 6,
            upgrade: true,
            delete_replaced: false,
            outside: OutsideGroups::Quality,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }
}

/// 追更顺序中的一项：制作组，加上可选的分辨率、编码、动态范围限定。
///
/// 旧数据里条目是纯字符串（只有组名），反序列化时兼容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "GroupEntryRepr")]
pub struct GroupEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// 动态范围标签，取值与 [`hdr_label`] 一致，如 `Dolby Vision`、`HDR10`、`SDR`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr: Option<String>,
    /// 帧率标签，取值与 [`frame_rate_label`] 一致，如 `60fps`、`常规帧率`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<String>,
    /// 画质档标签，取值与 [`tier_label`] 一致，如 `HQ`、`普通档`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// 片源平台标签，取值与 [`platform_label`] 一致，如 `Netflix`、`未标平台`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GroupEntryRepr {
    Name(String),
    Entry {
        name: String,
        #[serde(default)]
        resolution: Option<String>,
        #[serde(default)]
        codec: Option<String>,
        #[serde(default)]
        hdr: Option<String>,
        #[serde(default)]
        frame_rate: Option<String>,
        #[serde(default)]
        tier: Option<String>,
        #[serde(default)]
        platform: Option<String>,
    },
}

impl From<GroupEntryRepr> for GroupEntry {
    fn from(value: GroupEntryRepr) -> Self {
        match value {
            GroupEntryRepr::Name(name) => GroupEntry::named(name),
            GroupEntryRepr::Entry {
                name,
                resolution,
                codec,
                hdr,
                frame_rate,
                tier,
                platform,
            } => GroupEntry {
                name,
                resolution,
                codec,
                hdr,
                frame_rate,
                tier,
                platform,
            },
        }
    }
}

impl From<&str> for GroupEntry {
    fn from(value: &str) -> Self {
        GroupEntry::named(value)
    }
}

impl GroupEntry {
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            resolution: None,
            codec: None,
            hdr: None,
            frame_rate: None,
            tier: None,
            platform: None,
        }
    }

    /// 展示用名称，如 `CHDWEB · 2160p · H265 · Dolby Vision`。
    pub fn label(&self) -> String {
        std::iter::once(self.name.as_str())
            .chain(self.resolution.as_deref())
            .chain(self.codec.as_deref())
            .chain(self.hdr.as_deref())
            .chain(self.frame_rate.as_deref())
            .chain(self.tier.as_deref())
            .chain(self.platform.as_deref())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    fn key(&self) -> (String, [Option<String>; 6]) {
        let lower = |value: &Option<String>| value.as_deref().map(str::to_ascii_lowercase);
        (
            normalize_group(&self.name),
            [
                lower(&self.resolution),
                lower(&self.codec),
                lower(&self.hdr),
                lower(&self.frame_rate),
                lower(&self.tier),
                lower(&self.platform),
            ],
        )
    }

    fn matches(&self, release_group: &str, traits: &ReleaseTraits<'_>) -> bool {
        let group = normalize_group(&self.name);
        let name_matches = normalize_group(release_group) == group
            || release_group
                .split(GROUP_SEPARATORS)
                .any(|part| normalize_group(part) == group);
        let same = |wanted: &Option<String>, actual: Option<&str>| match wanted {
            None => true,
            Some(wanted) => actual.is_some_and(|actual| actual.eq_ignore_ascii_case(wanted)),
        };
        name_matches
            && same(&self.resolution, traits.resolution)
            && same(&self.codec, traits.codec)
            && same(&self.hdr, Some(&hdr_label(traits.hdr_formats)))
            && same(&self.frame_rate, Some(frame_rate_label(traits.frame_rate)))
            && same(&self.tier, Some(tier_label(traits.quality_tier)))
            && same(&self.platform, Some(platform_label(traits.platform)))
    }
}

/// 资源的画质特征，用于匹配带限定的条目。
#[derive(Debug, Clone, Copy, Default)]
pub struct ReleaseTraits<'a> {
    pub resolution: Option<&'a str>,
    pub codec: Option<&'a str>,
    pub hdr_formats: &'a [String],
    pub frame_rate: Option<&'a str>,
    pub quality_tier: Option<&'a str>,
    pub platform: Option<&'a str>,
}

impl<'a> ReleaseTraits<'a> {
    pub fn of(release: &'a super::ReleaseInfo) -> Self {
        Self {
            resolution: release.resolution.as_deref(),
            codec: release.codec.as_deref(),
            hdr_formats: &release.hdr_formats,
            frame_rate: release.frame_rate.as_deref(),
            quality_tier: release.quality_tier.as_deref(),
            platform: release.platform.as_deref(),
        }
    }
}

/// 帧率标签：没有高帧率标记时为 `常规帧率`。
pub fn frame_rate_label(frame_rate: Option<&str>) -> &str {
    frame_rate.unwrap_or(STANDARD_FRAME_RATE)
}

/// 平台标签：标题没写平台时为 `未标平台`。
pub fn platform_label(platform: Option<&str>) -> &str {
    platform.unwrap_or(UNKNOWN_PLATFORM)
}

/// 画质档标签：没有 HQ / EDR / MAXPLUS 等标记时为 `普通档`。
pub fn tier_label(tier: Option<&str>) -> &str {
    tier.unwrap_or(STANDARD_TIER)
}

/// 动态范围标签：没有 HDR 标记时为 `SDR`，多种格式按解析顺序用 ` + ` 连接。
pub fn hdr_label(formats: &[String]) -> String {
    if formats.is_empty() {
        SDR_LABEL.to_string()
    } else {
        formats.join(" + ")
    }
}

const GROUP_SEPARATORS: [char; 6] = ['&', '＆', '×', '/', '+', '、'];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroupPolicyError {
    #[error("最多设置 {MAX_GROUPS} 个制作组")]
    TooManyGroups,
    #[error("制作组名称过长：{0}")]
    GroupTooLong(String),
    #[error("画质限定过长：{0}")]
    TraitTooLong(String),
    #[error("最多设置 {MAX_RULES} 条包含或排除规则")]
    TooManyRules,
    #[error("规则过长：{0}")]
    RuleTooLong(String),
    #[error("规则不是有效的正则表达式：{0}")]
    InvalidRule(String),
    #[error("备选等待时间不能超过 {MAX_WAIT_HOURS} 小时")]
    WaitTooLong,
}

struct Rule {
    group: Option<String>,
    regex: Regex,
}

impl GroupPolicy {
    pub fn is_active(&self) -> bool {
        !self.groups.is_empty()
    }

    /// 去掉空白和重复项并校验规则，返回可直接保存的策略。
    pub fn normalized(mut self) -> Result<Self, GroupPolicyError> {
        let mut groups: Vec<GroupEntry> = Vec::new();
        for entry in self.groups.drain(..) {
            let name = entry.name.trim().to_string();
            if name.is_empty() {
                continue;
            }
            if name.chars().count() > MAX_GROUP_LEN {
                return Err(GroupPolicyError::GroupTooLong(name));
            }
            let clean = |value: Option<String>| -> Result<Option<String>, GroupPolicyError> {
                let Some(value) = value.map(|value| value.trim().to_string()) else {
                    return Ok(None);
                };
                if value.chars().count() > MAX_TRAIT_LEN {
                    return Err(GroupPolicyError::TraitTooLong(value));
                }
                Ok(Some(value).filter(|value| !value.is_empty()))
            };
            let entry = GroupEntry {
                name,
                resolution: clean(entry.resolution)?,
                codec: clean(entry.codec)?,
                hdr: clean(entry.hdr)?,
                frame_rate: clean(entry.frame_rate)?,
                tier: clean(entry.tier)?,
                platform: clean(entry.platform)?,
            };
            let key = entry.key();
            if key.0.is_empty() || groups.iter().any(|existing| existing.key() == key) {
                continue;
            }
            groups.push(entry);
        }
        if groups.len() > MAX_GROUPS {
            return Err(GroupPolicyError::TooManyGroups);
        }
        self.groups = groups;
        if self.wait_hours > MAX_WAIT_HOURS {
            return Err(GroupPolicyError::WaitTooLong);
        }
        self.include = normalize_rules(std::mem::take(&mut self.include))?;
        self.exclude = normalize_rules(std::mem::take(&mut self.exclude))?;
        Ok(self)
    }

    /// 资源在列表中的位置（第一个匹配的条目），0 为首选；不在列表内或无法识别时为 `None`。
    pub fn rank_of(
        &self,
        release_group: Option<&str>,
        traits: &ReleaseTraits<'_>,
    ) -> Option<usize> {
        let release_group = release_group?;
        if normalize_group(release_group).is_empty() {
            return None;
        }
        self.groups
            .iter()
            .position(|entry| entry.matches(release_group, traits))
    }

    /// 包含/排除规则的拒绝原因；规则在保存时已校验，这里忽略无法编译的旧规则。
    pub fn filter_reason(&self, title: &str, release_group: Option<&str>) -> Option<String> {
        let applies = |rule: &Rule| match (&rule.group, release_group) {
            (None, _) => true,
            (Some(group), Some(release_group)) => {
                let group = normalize_group(group);
                normalize_group(release_group) == group
                    || release_group
                        .split(GROUP_SEPARATORS)
                        .any(|part| normalize_group(part) == group)
            }
            (Some(_), None) => false,
        };
        for (source, rule) in self.include.iter().zip(compile_rules(&self.include)) {
            if let Some(rule) = rule
                && applies(&rule)
                && !rule.regex.is_match(title)
            {
                return Some(format!("不符合包含规则 {source}"));
            }
        }
        for (source, rule) in self.exclude.iter().zip(compile_rules(&self.exclude)) {
            if let Some(rule) = rule
                && applies(&rule)
                && rule.regex.is_match(title)
            {
                return Some(format!("命中排除规则 {source}"));
            }
        }
        None
    }

    /// 洗版时使用的位置编号：列表外记为列表长度，数字越小越优先。
    pub fn upgrade_rank(&self, rank: Option<usize>) -> u32 {
        rank.unwrap_or(self.groups.len()) as u32
    }
}

/// 参与挑选的候选信息；调用方需按质量从好到差排列。
#[derive(Debug, Clone, Copy)]
pub struct GroupCandidate<'a> {
    pub accepted: bool,
    pub release_group: Option<&'a str>,
    pub title: &'a str,
    pub publish_time: Option<DateTime<Utc>>,
    pub traits: ReleaseTraits<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CandidateGroupInfo {
    /// 在制作组列表中的位置，0 为首选。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_rank: Option<usize>,
    /// 被包含/排除规则拒绝的原因。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_filtered: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupSelection {
    /// 下载该候选。`rank` 为 `None` 表示列表外制作组。
    Pick {
        index: usize,
        rank: Option<usize>,
    },
    /// 首选组尚未发布，`until` 之后才会使用 `index` 指向的候选。
    Wait {
        index: usize,
        rank: Option<usize>,
        until: DateTime<Utc>,
    },
    Nothing,
}

pub fn select(
    policy: &GroupPolicy,
    candidates: &[GroupCandidate<'_>],
    now: DateTime<Utc>,
) -> (GroupSelection, Vec<CandidateGroupInfo>) {
    let info: Vec<CandidateGroupInfo> = candidates
        .iter()
        .map(|candidate| CandidateGroupInfo {
            group_rank: policy.rank_of(candidate.release_group, &candidate.traits),
            group_filtered: policy.filter_reason(candidate.title, candidate.release_group),
        })
        .collect();
    let usable: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(index, candidate)| candidate.accepted && info[*index].group_filtered.is_none())
        .map(|(index, _)| index)
        .collect();
    if usable.is_empty() {
        return (GroupSelection::Nothing, info);
    }
    if !policy.is_active() {
        return (
            GroupSelection::Pick {
                index: usable[0],
                rank: None,
            },
            info,
        );
    }
    // 质量顺序已由调用方保证，同一位置内取第一个即最佳。
    let best_in_list = usable
        .iter()
        .filter_map(|index| info[*index].group_rank.map(|rank| (rank, *index)))
        .min_by_key(|(rank, index)| (*rank, *index));
    if let Some((0, index)) = best_in_list {
        return (
            GroupSelection::Pick {
                index,
                rank: Some(0),
            },
            info,
        );
    }
    let fallback = best_in_list
        .map(|(rank, index)| (Some(rank), index))
        .or_else(|| {
            (policy.outside == OutsideGroups::Quality)
                .then(|| {
                    usable
                        .iter()
                        .find(|index| info[**index].group_rank.is_none())
                })
                .flatten()
                .map(|index| (None, *index))
        });
    let Some((rank, index)) = fallback else {
        return (GroupSelection::Nothing, info);
    };
    if policy.wait_hours == 0 {
        return (GroupSelection::Pick { index, rank }, info);
    }
    // 从其他组首次发布该集算起；发布时间未知时视为已经等够。
    let first_seen = usable
        .iter()
        .filter(|index| info[**index].group_rank != Some(0))
        .map(|index| candidates[*index].publish_time)
        .try_fold(None::<DateTime<Utc>>, |earliest, time| {
            time.map(|time| Some(earliest.map_or(time, |earliest| earliest.min(time))))
        })
        .flatten();
    match first_seen {
        Some(first_seen) => {
            let until = first_seen + Duration::hours(policy.wait_hours as i64);
            if until > now {
                (GroupSelection::Wait { index, rank, until }, info)
            } else {
                (GroupSelection::Pick { index, rank }, info)
            }
        }
        None => (GroupSelection::Pick { index, rank }, info),
    }
}

/// 洗版检查：在已接受且未被规则排除的候选中，找出比 `current_rank` 更靠前的最佳版本。
pub fn select_upgrade(
    policy: &GroupPolicy,
    candidates: &[GroupCandidate<'_>],
    current_rank: u32,
) -> Option<(usize, usize)> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            candidate.accepted
                && policy
                    .filter_reason(candidate.title, candidate.release_group)
                    .is_none()
        })
        .filter_map(|(index, candidate)| {
            policy
                .rank_of(candidate.release_group, &candidate.traits)
                .filter(|rank| (*rank as u32) < current_rank)
                .map(|rank| (rank, index))
        })
        .min_by_key(|(rank, index)| (*rank, *index))
        .map(|(rank, index)| (index, rank))
}

pub fn normalize_group(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalize_rules(rules: Vec<String>) -> Result<Vec<String>, GroupPolicyError> {
    let mut normalized: Vec<String> = Vec::new();
    for rule in rules
        .iter()
        .flat_map(|rule| rule.lines())
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
    {
        if rule.chars().count() > MAX_RULE_LEN {
            return Err(GroupPolicyError::RuleTooLong(rule.to_string()));
        }
        let (_, pattern) = split_rule(rule);
        if pattern.trim().is_empty() || build_regex(pattern).is_err() {
            return Err(GroupPolicyError::InvalidRule(rule.to_string()));
        }
        if !normalized.iter().any(|existing| existing == rule) {
            normalized.push(rule.to_string());
        }
    }
    if normalized.len() > MAX_RULES {
        return Err(GroupPolicyError::TooManyRules);
    }
    Ok(normalized)
}

fn compile_rules(rules: &[String]) -> Vec<Option<Rule>> {
    rules
        .iter()
        .map(|rule| {
            let (group, pattern) = split_rule(rule);
            build_regex(pattern).ok().map(|regex| Rule {
                group: group.map(str::to_string),
                regex,
            })
        })
        .collect()
}

/// `{{组名}}:正则` 拆成组名和正则；其他写法整行都是正则。
fn split_rule(rule: &str) -> (Option<&str>, &str) {
    if let Some(rest) = rule.strip_prefix("{{")
        && let Some((group, pattern)) = rest.split_once("}}:")
        && !group.trim().is_empty()
    {
        return (Some(group.trim()), pattern);
    }
    (None, rule)
}

fn build_regex(pattern: &str) -> Result<Regex, regex::Error> {
    regex::RegexBuilder::new(pattern.trim())
        .case_insensitive(true)
        .size_limit(1 << 20)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(groups: &[&str]) -> GroupPolicy {
        GroupPolicy {
            groups: groups
                .iter()
                .map(|group| GroupEntry::named(*group))
                .collect(),
            ..GroupPolicy::default()
        }
    }

    fn at(hours_ago: i64) -> Option<DateTime<Utc>> {
        Some(now() - Duration::hours(hours_ago))
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn cand<'a>(
        group: Option<&'a str>,
        title: &'a str,
        published: Option<DateTime<Utc>>,
    ) -> GroupCandidate<'a> {
        GroupCandidate {
            accepted: true,
            release_group: group,
            title,
            publish_time: published,
            traits: ReleaseTraits::default(),
        }
    }

    const NONE: ReleaseTraits<'static> = ReleaseTraits {
        resolution: None,
        codec: None,
        hdr_formats: &[],
        frame_rate: None,
        quality_tier: None,
        platform: None,
    };

    #[test]
    fn group_names_match_case_and_punctuation_insensitively_and_in_collaborations() {
        let policy = policy(&["CHDWEB", "Nekomoe kissaten", "LoliHouse"]);
        assert_eq!(policy.rank_of(Some("chdweb"), &NONE), Some(0));
        assert_eq!(policy.rank_of(Some("Nekomoe Kissaten"), &NONE), Some(1));
        assert_eq!(
            policy.rank_of(Some("Nekomoe kissaten&LoliHouse"), &NONE),
            Some(1)
        );
        assert_eq!(policy.rank_of(Some("HHWEB"), &NONE), None);
        assert_eq!(policy.rank_of(None, &NONE), None);
    }

    #[test]
    fn primary_group_wins_even_when_a_backup_has_better_quality() {
        let policy = policy(&["CHDWEB", "HHWEB"]);
        let candidates = [
            cand(Some("HHWEB"), "Show.S01E01.2160p-HHWEB", at(5)),
            cand(Some("CHDWEB"), "Show.S01E01.1080p-CHDWEB", at(1)),
        ];
        let (selection, info) = select(&policy, &candidates, now());
        assert_eq!(
            selection,
            GroupSelection::Pick {
                index: 1,
                rank: Some(0)
            }
        );
        assert_eq!(info[0].group_rank, Some(1));
    }

    #[test]
    fn backup_waits_from_the_first_non_primary_release() {
        let policy = GroupPolicy {
            wait_hours: 6,
            ..policy(&["CHDWEB", "HHWEB", "ADWeb"])
        };
        let candidates = [
            cand(Some("HHWEB"), "Show.S01E01-HHWEB", at(1)),
            cand(Some("ADWeb"), "Show.S01E01-ADWeb", at(2)),
        ];
        let (selection, _) = select(&policy, &candidates, now());
        assert_eq!(
            selection,
            GroupSelection::Wait {
                index: 0,
                rank: Some(1),
                until: now() + Duration::hours(4)
            }
        );

        let candidates = [
            cand(Some("ADWeb"), "Show.S01E01-ADWeb", at(7)),
            cand(Some("HHWEB"), "Show.S01E01-HHWEB", at(1)),
        ];
        let (selection, _) = select(&policy, &candidates, now());
        assert_eq!(
            selection,
            GroupSelection::Pick {
                index: 1,
                rank: Some(1)
            }
        );
    }

    #[test]
    fn outside_groups_follow_the_outside_policy() {
        let candidates = [
            cand(Some("Other"), "Show.S01E01-Other", at(10)),
            cand(None, "Show 01", at(10)),
        ];
        let quality = GroupPolicy {
            wait_hours: 0,
            ..policy(&["CHDWEB"])
        };
        assert_eq!(
            select(&quality, &candidates, now()).0,
            GroupSelection::Pick {
                index: 0,
                rank: None
            }
        );
        let skip = GroupPolicy {
            outside: OutsideGroups::Skip,
            ..quality
        };
        assert_eq!(select(&skip, &candidates, now()).0, GroupSelection::Nothing);
    }

    #[test]
    fn unknown_publish_time_does_not_wait_forever() {
        let policy = policy(&["CHDWEB", "HHWEB"]);
        let candidates = [cand(Some("HHWEB"), "Show.S01E01-HHWEB", None)];
        assert_eq!(
            select(&policy, &candidates, now()).0,
            GroupSelection::Pick {
                index: 0,
                rank: Some(1)
            }
        );
    }

    #[test]
    fn inactive_policy_keeps_quality_order_but_applies_rules() {
        let policy = GroupPolicy {
            exclude: vec!["60fps".into()],
            ..GroupPolicy::default()
        }
        .normalized()
        .unwrap();
        let candidates = [
            cand(Some("HHWEB"), "Show.S01E01.2160p.60fps-HHWEB", at(1)),
            cand(Some("ADWeb"), "Show.S01E01.1080p-ADWeb", at(1)),
        ];
        let (selection, info) = select(&policy, &candidates, now());
        assert_eq!(
            selection,
            GroupSelection::Pick {
                index: 1,
                rank: None
            }
        );
        assert_eq!(
            info[0].group_filtered.as_deref(),
            Some("命中排除规则 60fps")
        );
    }

    #[test]
    fn group_scoped_rules_only_apply_to_that_group() {
        let policy = GroupPolicy {
            include: vec!["{{ANi}}:1080[Pp]".into()],
            exclude: vec!["{{HHWEB}}:60fps".into()],
            ..policy(&["ANi", "HHWEB"])
        }
        .normalized()
        .unwrap();
        assert!(
            policy
                .filter_reason("[ANi] Show - 01 [720P]", Some("ANi"))
                .is_some()
        );
        assert!(
            policy
                .filter_reason("[ANi] Show - 01 [1080P]", Some("ANi"))
                .is_none()
        );
        assert!(
            policy
                .filter_reason("Show.S01E01.60fps-ADWeb", Some("ADWeb"))
                .is_none()
        );
        assert!(
            policy
                .filter_reason("Show.S01E01.60fps-HHWEB", Some("HHWEB"))
                .is_some()
        );
    }

    #[test]
    fn normalization_dedupes_groups_and_rejects_invalid_rules() {
        let policy = GroupPolicy {
            groups: vec![
                " CHDWEB ".into(),
                "chdweb".into(),
                "".into(),
                "HHWEB".into(),
            ],
            include: vec!["HDR\nDV".into()],
            ..GroupPolicy::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(
            policy.groups,
            vec![GroupEntry::named("CHDWEB"), GroupEntry::named("HHWEB")]
        );
        assert_eq!(policy.include, vec!["HDR", "DV"]);
        assert!(
            GroupPolicy {
                exclude: vec!["(".into()],
                ..GroupPolicy::default()
            }
            .normalized()
            .is_err()
        );
    }

    #[test]
    fn upgrades_only_accept_strictly_better_listed_groups() {
        let policy = policy(&["CHDWEB", "HHWEB", "ADWeb"]);
        let candidates = [
            cand(Some("ADWeb"), "Show.S01E01-ADWeb", at(1)),
            cand(Some("HHWEB"), "Show.S01E01-HHWEB", at(1)),
            cand(Some("Other"), "Show.S01E01-Other", at(1)),
        ];
        assert_eq!(select_upgrade(&policy, &candidates, 2), Some((1, 1)));
        assert_eq!(select_upgrade(&policy, &candidates, 1), None);
        assert_eq!(select_upgrade(&policy, &candidates, 3), Some((1, 1)));
    }

    #[test]
    fn variant_entries_only_match_their_resolution_codec_and_dynamic_range() {
        let dv = vec!["Dolby Vision".to_string()];
        let policy = GroupPolicy {
            groups: vec![
                GroupEntry {
                    resolution: Some("2160p".into()),
                    codec: Some("H265".into()),
                    hdr: Some("Dolby Vision".into()),
                    ..GroupEntry::named("CHDWEB")
                },
                GroupEntry {
                    resolution: Some("1080p".into()),
                    hdr: Some(SDR_LABEL.into()),
                    ..GroupEntry::named("CHDWEB")
                },
                GroupEntry::named("HHWEB"),
            ],
            ..GroupPolicy::default()
        }
        .normalized()
        .unwrap();
        let uhd_dv = ReleaseTraits {
            resolution: Some("2160p"),
            codec: Some("H265"),
            hdr_formats: &dv,
            ..NONE
        };
        let uhd_sdr = ReleaseTraits {
            resolution: Some("2160p"),
            codec: Some("H265"),
            hdr_formats: &[],
            ..NONE
        };
        let fhd = ReleaseTraits {
            resolution: Some("1080p"),
            codec: Some("H264"),
            hdr_formats: &[],
            ..NONE
        };
        assert_eq!(policy.rank_of(Some("CHDWEB"), &uhd_dv), Some(0));
        assert_eq!(policy.rank_of(Some("CHDWEB"), &fhd), Some(1));
        assert_eq!(policy.rank_of(Some("CHDWEB"), &uhd_sdr), None);
        assert_eq!(policy.rank_of(Some("HHWEB"), &uhd_sdr), Some(2));

        let mut primary = cand(Some("CHDWEB"), "Show.S01E01.2160p.DV.H265-CHDWEB", at(1));
        primary.traits = uhd_dv;
        let mut backup = cand(Some("CHDWEB"), "Show.S01E01.1080p.H264-CHDWEB", at(1));
        backup.traits = fhd;
        assert_eq!(
            select(&policy, &[backup, primary], now()).0,
            GroupSelection::Pick {
                index: 1,
                rank: Some(0)
            }
        );
    }

    #[test]
    fn entries_accept_legacy_strings_and_dedupe_by_variant() {
        let policy: GroupPolicy = serde_json::from_value(serde_json::json!({
            "groups": [
                "CHDWEB",
                {"name": "chdweb", "resolution": "2160p"},
                {"name": "CHDWEB", "resolution": " 2160P "},
                {"name": "CHDWEB", "codec": ""}
            ]
        }))
        .unwrap();
        let policy = policy.normalized().unwrap();
        assert_eq!(policy.groups.len(), 2);
        assert_eq!(policy.groups[1].label(), "chdweb · 2160p");
        assert_eq!(
            serde_json::to_value(&policy.groups[0]).unwrap(),
            serde_json::json!({"name": "CHDWEB"})
        );
    }

    #[test]
    fn frame_rate_and_tier_split_otherwise_identical_versions() {
        let policy = GroupPolicy {
            groups: vec![
                GroupEntry {
                    frame_rate: Some(STANDARD_FRAME_RATE.into()),
                    tier: Some("HQ".into()),
                    ..GroupEntry::named("Momoda")
                },
                GroupEntry {
                    tier: Some(STANDARD_TIER.into()),
                    ..GroupEntry::named("Momoda")
                },
            ],
            ..GroupPolicy::default()
        }
        .normalized()
        .unwrap();
        let hq = ReleaseTraits {
            quality_tier: Some("HQ"),
            ..NONE
        };
        let hq_hfr = ReleaseTraits {
            frame_rate: Some("HFR"),
            quality_tier: Some("HQ"),
            ..NONE
        };
        let hfr = ReleaseTraits {
            frame_rate: Some("HFR"),
            ..NONE
        };
        assert_eq!(policy.rank_of(Some("Momoda"), &hq), Some(0));
        assert_eq!(policy.rank_of(Some("Momoda"), &hq_hfr), None);
        assert_eq!(policy.rank_of(Some("Momoda"), &hfr), Some(1));
        assert_eq!(policy.groups[0].label(), "Momoda · 常规帧率 · HQ");
    }
}
