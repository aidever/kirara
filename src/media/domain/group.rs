//! 制作组优先的追更策略：按用户排好的制作组顺序挑选每集资源。
//!
//! 第一个制作组是首选，其余是备选。首选组没有发布时，从其他组第一次出现该集资源开始
//! 等待 `wait_hours`，仍然没有首选组版本才退而求其次。列表外的制作组（包括无法识别组名的
//! 资源）按 `outside` 处理：按质量配置兜底或不下载。

use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

pub const MAX_GROUPS: usize = 16;
pub const MAX_RULES: usize = 32;
pub const MAX_WAIT_HOURS: u32 = 24 * 14;
const MAX_GROUP_LEN: usize = 80;
const MAX_RULE_LEN: usize = 300;

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
    pub groups: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroupPolicyError {
    #[error("最多设置 {MAX_GROUPS} 个制作组")]
    TooManyGroups,
    #[error("制作组名称过长：{0}")]
    GroupTooLong(String),
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
        let mut groups: Vec<String> = Vec::new();
        for group in self.groups.drain(..) {
            let group = group.trim().to_string();
            if group.is_empty() {
                continue;
            }
            if group.chars().count() > MAX_GROUP_LEN {
                return Err(GroupPolicyError::GroupTooLong(group));
            }
            let key = normalize_group(&group);
            if key.is_empty()
                || groups
                    .iter()
                    .any(|existing| normalize_group(existing) == key)
            {
                continue;
            }
            groups.push(group);
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

    /// 制作组在列表中的位置，0 为首选；不在列表内或无法识别时为 `None`。
    pub fn rank_of(&self, release_group: Option<&str>) -> Option<usize> {
        let release_group = release_group?;
        let whole = normalize_group(release_group);
        if whole.is_empty() {
            return None;
        }
        let parts: Vec<String> = release_group
            .split(['&', '＆', '×', '/', '+', '、'])
            .map(normalize_group)
            .filter(|part| !part.is_empty())
            .collect();
        self.groups.iter().position(|group| {
            let group = normalize_group(group);
            group == whole || parts.iter().any(|part| *part == group)
        })
    }

    /// 包含/排除规则的拒绝原因；规则在保存时已校验，这里忽略无法编译的旧规则。
    pub fn filter_reason(&self, title: &str, release_group: Option<&str>) -> Option<String> {
        let applies = |rule: &Rule| match (&rule.group, release_group) {
            (None, _) => true,
            (Some(group), Some(release_group)) => {
                let group = normalize_group(group);
                normalize_group(release_group) == group
                    || release_group
                        .split(['&', '＆', '×', '/', '+', '、'])
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
            group_rank: policy.rank_of(candidate.release_group),
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
                .rank_of(candidate.release_group)
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
            groups: groups.iter().map(|group| group.to_string()).collect(),
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
        }
    }

    #[test]
    fn group_names_match_case_and_punctuation_insensitively_and_in_collaborations() {
        let policy = policy(&["CHDWEB", "Nekomoe kissaten", "LoliHouse"]);
        assert_eq!(policy.rank_of(Some("chdweb")), Some(0));
        assert_eq!(policy.rank_of(Some("Nekomoe Kissaten")), Some(1));
        assert_eq!(policy.rank_of(Some("Nekomoe kissaten&LoliHouse")), Some(1));
        assert_eq!(policy.rank_of(Some("HHWEB")), None);
        assert_eq!(policy.rank_of(None), None);
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
        assert_eq!(policy.groups, vec!["CHDWEB", "HHWEB"]);
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
}
