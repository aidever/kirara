import { useCallback, useEffect, useId, useMemo, useRef, useState, type ReactNode } from "react";
import { ArrowUp, ChevronDown, ExternalLink, LoaderCircle, Plus, RefreshCw, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  NumberingRulesEditor,
  SeasonMismatchList,
  cleanNumbering,
  type NumberingRule,
  type SeasonMismatchSummary,
} from "@/components/media/numbering-rules";
import { api, ApiError } from "@/lib/api";
import { cn } from "@/lib/utils";

/**
 * 追更顺序中的一项，与后端 `GroupEntry` 对应。只有组名时匹配该组的任意版本；
 * 填了分辨率、编码、动态范围、帧率、画质档或平台时，只匹配该组里这些特征都相同的版本。
 */
export type GroupEntry = {
  name: string;
  resolution?: string | null;
  codec?: string | null;
  hdr?: string | null;
  frame_rate?: string | null;
  tier?: string | null;
  platform?: string | null;
};

/** 与后端一致：没有对应标记的资源在这些维度上的取值。 */
const STANDARD_FRAME_RATE = "常规帧率";
const STANDARD_TIER = "普通档";
const UNKNOWN_PLATFORM = "未标平台";

/** 与后端 `GroupPolicy` 对应：第一项为首选，其余为备选。 */
export type GroupPolicy = {
  groups: GroupEntry[];
  wait_hours: number;
  upgrade: boolean;
  delete_replaced: boolean;
  outside: "quality" | "skip";
  include: string[];
  exclude: string[];
};

export const DEFAULT_GROUP_POLICY: GroupPolicy = {
  groups: [],
  wait_hours: 6,
  upgrade: true,
  delete_replaced: false,
  outside: "quality",
  include: [],
  exclude: [],
};

export type ProbeTorrent = {
  result: {
    site_id: number;
    source_site: string;
    torrent_id: string;
    title: string;
    subtitle?: string | null;
    detail_url?: string | null;
    size: number;
    seeders: number;
    leechers: number;
    publish_time?: string | null;
  };
  episodes: number[];
  absolute_episodes: number[];
  full_season: boolean;
  total_episodes?: number | null;
  source?: string | null;
  /** 经编号映射换算时的说明，如 `S03E02 → S01E42`。 */
  numbering_note?: string | null;
};

export type ReleaseVariantSummary = {
  resolution: string | null;
  codec: string | null;
  hdr: string;
  frame_rate?: string;
  tier?: string;
  platform?: string;
  sources: string[];
  count: number;
  sites: string[];
  episodes: number[];
  absolute_episodes: number[];
  has_pack: boolean;
  latest_publish: string | null;
  size_per_episode: number | null;
  min_size_per_episode?: number | null;
  max_size_per_episode?: number | null;
  max_seeders: number;
  torrents: ProbeTorrent[];
};

export type ReleaseGroupSummary = {
  name: string | null;
  count: number;
  sites: string[];
  episodes: number[];
  absolute_episodes: number[];
  has_pack: boolean;
  resolutions: string[];
  sources: string[];
  codecs: string[];
  hdr_formats: string[];
  latest_publish: string | null;
  size_per_episode: number | null;
  max_seeders: number;
  median_seeders: number;
  sample_title: string;
  variants?: ReleaseVariantSummary[];
};

type ProbeResponse = {
  probe_id?: string | null;
  queries: string[];
  matched: number;
  mapped?: number;
  groups: ReleaseGroupSummary[];
  season_mismatches?: SeasonMismatchSummary<ProbeTorrent>[];
  errors: Array<{ source_site: string; message: string }>;
  total_sites: number;
  successful_sites: number;
  probed_at: string;
};

export type GroupUpgradeRecord = {
  id: number;
  target_key: string;
  season: number | null;
  episode: number | null;
  absolute_episode: number | null;
  current_group: string | null;
  current_rank: number;
  status: string;
  upgrade_group: string | null;
  replaced_count: number;
  last_checked_at: string | null;
  last_error: string | null;
  updated_at: string;
};

const WAIT_CHOICES = [0, 2, 6, 12, 24];

function optionalText(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function normalizeEntries(value: unknown): GroupEntry[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item): GroupEntry[] => {
    if (typeof item === "string") return [{ name: item }];
    if (item && typeof item === "object" && typeof (item as { name?: unknown }).name === "string") {
      const record = item as Record<string, unknown>;
      return [{
        name: record.name as string,
        resolution: optionalText(record.resolution),
        codec: optionalText(record.codec),
        hdr: optionalText(record.hdr),
        frame_rate: optionalText(record.frame_rate),
        tier: optionalText(record.tier),
        platform: optionalText(record.platform),
      }];
    }
    return [];
  });
}

export function normalizeGroupPolicy(value: unknown): GroupPolicy {
  if (!value || typeof value !== "object") return { ...DEFAULT_GROUP_POLICY };
  const record = value as Record<string, unknown>;
  const strings = (input: unknown) => (Array.isArray(input) ? input.filter((item): item is string => typeof item === "string") : []);
  return {
    groups: normalizeEntries(record.groups),
    wait_hours: typeof record.wait_hours === "number" ? record.wait_hours : DEFAULT_GROUP_POLICY.wait_hours,
    upgrade: typeof record.upgrade === "boolean" ? record.upgrade : DEFAULT_GROUP_POLICY.upgrade,
    delete_replaced: record.delete_replaced === true,
    outside: record.outside === "skip" ? "skip" : "quality",
    include: strings(record.include),
    exclude: strings(record.exclude),
  };
}

function sameGroup(left: string, right: string) {
  const normalize = (value: string) => value.toLowerCase().replace(/[^\p{L}\p{N}]/gu, "");
  return normalize(left) === normalize(right);
}

/** 与后端一致：组名相同，或合作发布（A&B）中的任意一方相同。 */
function groupNameMatches(entryName: string, releaseGroup: string) {
  return sameGroup(entryName, releaseGroup) || releaseGroup.split(/[&＆×/+、]/).some((part) => part.trim() && sameGroup(entryName, part));
}

function sameText(left: string | null | undefined, right: string | null | undefined) {
  return (left ?? "").toLowerCase() === (right ?? "").toLowerCase();
}

const TRAIT_KEYS = ["resolution", "codec", "hdr", "frame_rate", "tier", "platform"] as const;

/** 条目的限定特征，如 ["2160p", "H265", "Dolby Vision", "HQ"]；只按组名时为空。 */
export function entryTraits(entry: GroupEntry): string[] {
  return TRAIT_KEYS.map((key) => entry[key]).filter((value): value is string => Boolean(value));
}

export function entryLabel(entry: GroupEntry): string {
  return [entry.name, ...entryTraits(entry)].join(" · ");
}

function sameEntry(left: GroupEntry, right: GroupEntry) {
  return sameGroup(left.name, right.name) && TRAIT_KEYS.every((key) => sameText(left[key], right[key]));
}

/** 版本在各维度上的标签，取值与后端 `GroupEntry` 匹配时使用的一致。 */
type VariantTraits = { resolution: string | null; codec: string | null; hdr: string; frame_rate: string; tier: string; platform: string };

function variantTraits(variant: ReleaseVariantSummary): VariantTraits {
  return {
    resolution: variant.resolution,
    codec: variant.codec,
    hdr: variant.hdr,
    frame_rate: variant.frame_rate ?? STANDARD_FRAME_RATE,
    tier: variant.tier ?? STANDARD_TIER,
    platform: variant.platform ?? UNKNOWN_PLATFORM,
  };
}

function entryMatchesVariant(entry: GroupEntry, groupName: string, variant: ReleaseVariantSummary) {
  const traits = variantTraits(variant);
  return groupNameMatches(entry.name, groupName) && TRAIT_KEYS.every((key) => !entry[key] || sameText(entry[key], traits[key]));
}

/** 版本在追更顺序中的实际位置（第一个匹配的条目），0 为首选，-1 为列表外。 */
function variantRank(policy: GroupPolicy, groupName: string, variant: ReleaseVariantSummary) {
  return policy.groups.findIndex((entry) => entryMatchesVariant(entry, groupName, variant));
}

function variantEntry(groupName: string, variant: ReleaseVariantSummary): GroupEntry {
  return { name: groupName, ...variantTraits(variant) };
}

function variantKey(variant: ReleaseVariantSummary) {
  return TRAIT_KEYS.map((key) => variantTraits(variant)[key] ?? "?").join("|");
}

/** 同一版本内单集体积相差的倍数；达到 2 倍时提示可能混有标题没写出的规格。 */
function sizeSpread(variant: ReleaseVariantSummary) {
  const min = variant.min_size_per_episode;
  const max = variant.max_size_per_episode;
  return min && max && min > 0 ? max / min : 1;
}

/** 旧版本后端没有返回细分版本时，用整组数据拼一个。 */
function variantsOf(group: ReleaseGroupSummary): ReleaseVariantSummary[] {
  if (group.variants && group.variants.length > 0) return group.variants;
  return [{
    resolution: group.resolutions.length === 1 ? group.resolutions[0] : null,
    codec: group.codecs.length === 1 ? group.codecs[0] : null,
    hdr: group.hdr_formats.length > 0 ? group.hdr_formats.join(" + ") : "SDR",
    sources: group.sources,
    count: group.count,
    sites: group.sites,
    episodes: group.episodes,
    absolute_episodes: group.absolute_episodes,
    has_pack: group.has_pack,
    latest_publish: group.latest_publish,
    size_per_episode: group.size_per_episode,
    max_seeders: group.max_seeders,
    torrents: [],
  }];
}

function compactEpisodes(episodes: number[], prefix = "E"): string {
  if (episodes.length === 0) return "";
  const ranges: string[] = [];
  let start = episodes[0];
  let previous = episodes[0];
  for (const episode of [...episodes.slice(1), Number.NaN]) {
    if (episode === previous + 1) {
      previous = episode;
      continue;
    }
    const pad = (value: number) => String(value).padStart(2, "0");
    ranges.push(start === previous ? `${prefix}${pad(start)}` : `${prefix}${pad(start)}–${prefix}${pad(previous)}`);
    start = episode;
    previous = episode;
  }
  return ranges.join("、");
}

function formatSize(bytes: number | null): string {
  if (!bytes) return "—";
  const gib = bytes / 1024 ** 3;
  return gib >= 1 ? `${gib.toFixed(1)} GB` : `${Math.round(bytes / 1024 ** 2)} MB`;
}

export function formatRelative(value: string | null | undefined): string {
  if (!value) return "时间未知";
  const time = new Date(value).getTime();
  if (Number.isNaN(time)) return "时间未知";
  const minutes = Math.round((Date.now() - time) / 60000);
  if (minutes < 0) {
    const ahead = -minutes;
    if (ahead < 60) return `${ahead} 分钟后`;
    if (ahead < 48 * 60) return `${Math.round(ahead / 60)} 小时后`;
    return `${Math.round(ahead / 1440)} 天后`;
  }
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  if (minutes < 48 * 60) return `${Math.round(minutes / 60)} 小时前`;
  return `${Math.round(minutes / 1440)} 天前`;
}

function waitLabel(hours: number) {
  return hours === 0 ? "不等" : `${hours} 小时`;
}

/** 列表和详情中展示的追更顺序，例如「首选 CHDWEB · 2160p → HHWEB」。 */
export function GroupChain({ policy, compact = false }: { policy: GroupPolicy; compact?: boolean }) {
  if (policy.groups.length === 0) {
    return <span className="text-xs text-muted">不限制作组 · 按质量挑选</span>;
  }
  const shown = compact ? policy.groups.slice(0, 3) : policy.groups;
  return (
    <span className="inline-flex flex-wrap items-center gap-1">
      {shown.map((entry, index) => (
        <span
          key={entryLabel(entry)}
          title={`${index === 0 ? "首选" : `备选 ${index}`}：${entryLabel(entry)}`}
          className={cn(
            "inline-flex h-6 max-w-[260px] items-center gap-1 truncate rounded-md px-2 font-mono text-xs",
            index === 0 ? "bg-primary font-semibold text-primary-foreground" : "bg-secondary text-secondary-foreground",
          )}
        >
          <span className="truncate">{entry.name}</span>
          {entryTraits(entry).length > 0 ? <span className={cn("truncate font-normal", index === 0 ? "opacity-80" : "text-muted")}>{entryTraits(entry).join(" ")}</span> : null}
        </span>
      ))}
      {compact && policy.groups.length > shown.length ? <span className="text-xs text-muted">+{policy.groups.length - shown.length}</span> : null}
      {policy.outside === "quality" ? <span className="text-xs text-muted">· 其他组兜底</span> : null}
    </span>
  );
}

export function groupPolicySummary(policy: GroupPolicy): string {
  if (policy.groups.length === 0) {
    return "不限制作组，按质量配置挑选每集资源。";
  }
  const [primary, ...backups] = policy.groups.map(entryLabel);
  const parts = [`每集优先下载 ${primary}`];
  if (backups.length > 0) {
    parts.push(
      policy.wait_hours === 0
        ? `没有时立即使用备选（${backups.join(" → ")}）`
        : `其他版本发布 ${policy.wait_hours} 小时后仍没有，再用备选（${backups.join(" → ")}）`,
    );
  }
  parts.push(policy.outside === "quality" ? "列表内都没有时按质量配置从其他版本兜底" : "列表外的版本不下载");
  if (policy.upgrade && (backups.length > 0 || policy.outside === "quality")) {
    parts.push(policy.delete_replaced ? "更靠前的版本发布后洗版并删除旧版本" : "更靠前的版本发布后洗版，旧版本保留做种");
  }
  return `${parts.join("；")}。`;
}

function episodesOf(item: { episodes: number[]; absolute_episodes: number[] }, absoluteNumbering: boolean) {
  return absoluteNumbering ? item.absolute_episodes : item.episodes;
}

function torrentEpisodeLabel(torrent: ProbeTorrent, absoluteNumbering: boolean) {
  const episodes = episodesOf(torrent, absoluteNumbering);
  if (episodes.length > 0) return compactEpisodes(episodes, absoluteNumbering ? "#" : "E");
  if (torrent.total_episodes) return `全 ${torrent.total_episodes} 集`;
  return torrent.full_season ? "季包" : "";
}

function EpisodeStrip({ episodes, maxEpisode, primary, label }: { episodes: number[]; maxEpisode: number; primary: boolean; label: string }) {
  if (maxEpisode <= 0 || maxEpisode > 120) return null;
  return (
    <div className="flex flex-wrap gap-[3px]" role="img" aria-label={`${label} 已发布 ${episodes.length} 集`}>
      {Array.from({ length: maxEpisode }, (_, index) => index + 1).map((episode) => (
        <span
          key={episode}
          className={cn(
            "h-5 min-w-[26px] rounded px-0.5 text-center font-mono text-[10px] leading-5",
            episodes.includes(episode)
              ? (primary ? "bg-primary text-primary-foreground" : "bg-[#9a82c2] text-white")
              : "border border-dashed border-[#c9bedb] leading-[18px] text-muted",
          )}
        >
          {episode}
        </span>
      ))}
    </div>
  );
}

const TORRENT_PAGE = 20;

function TorrentList({ torrents, absoluteNumbering, isMovie }: { torrents: ProbeTorrent[]; absoluteNumbering: boolean; isMovie: boolean }) {
  const [showAll, setShowAll] = useState(false);
  if (torrents.length === 0) return <p className="text-xs text-muted">没有种子明细，重新探测后可查看。</p>;
  const shown = showAll ? torrents : torrents.slice(0, TORRENT_PAGE);
  return (
    <div className="flex flex-col gap-1.5">
      <ul className="flex flex-col divide-y divide-border overflow-hidden rounded-lg border border-border bg-card">
        {shown.map((torrent) => {
          const { result } = torrent;
          const episodes = isMovie ? "" : torrentEpisodeLabel(torrent, absoluteNumbering);
          return (
            <li key={`${result.site_id}:${result.torrent_id}`} className="flex flex-col gap-1 px-3 py-2">
              {result.detail_url ? (
                <a
                  href={result.detail_url}
                  target="_blank"
                  rel="noreferrer noopener"
                  className="group inline-flex items-start gap-1 break-all font-mono text-xs leading-5 text-foreground hover:text-primary hover:underline"
                >
                  {result.title}
                  <ExternalLink className="mt-1 size-3 shrink-0 text-muted group-hover:text-primary" aria-hidden="true" />
                  <span className="sr-only">（在站点打开）</span>
                </a>
              ) : (
                <span className="break-all font-mono text-xs leading-5">{result.title}</span>
              )}
              {result.subtitle ? <span className="break-all text-xs text-muted">{result.subtitle}</span> : null}
              <span className="flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-secondary-foreground">
                <span className="font-semibold">{result.source_site}</span>
                {episodes ? <span className="font-mono">{episodes}</span> : null}
                {torrent.numbering_note ? <span className="rounded bg-[#e3f0ec] px-1.5 font-mono text-[#2b5e51]" title="按编号映射换算">{torrent.numbering_note}</span> : null}
                {torrent.source ? <span>{torrent.source}</span> : null}
                <span>{formatSize(result.size)}</span>
                <span title="做种 / 下载">↑{result.seeders} ↓{result.leechers}</span>
                <span>{formatRelative(result.publish_time)}</span>
              </span>
            </li>
          );
        })}
      </ul>
      {torrents.length > TORRENT_PAGE ? (
        <button type="button" onClick={() => setShowAll((value) => !value)} className="min-h-8 self-start rounded-md px-2 text-xs font-medium text-primary hover:bg-accent">
          {showAll ? "收起" : `显示全部 ${torrents.length} 条`}
        </button>
      ) : null}
    </div>
  );
}

function RankBadge({ rank, policy }: { rank: number; policy: GroupPolicy }) {
  if (policy.groups.length === 0) return null;
  if (rank === 0) return <span className="rounded-full bg-primary px-2 py-0.5 text-xs font-semibold text-primary-foreground">首选</span>;
  if (rank > 0) return <span className="rounded-full bg-accent px-2 py-0.5 text-xs font-semibold text-[#4e2f77]">备选 {rank}</span>;
  return policy.outside === "quality"
    ? <span className="rounded-full border border-border px-2 py-0.5 text-xs text-muted" title="列表内都没有时，可能按质量配置兜底使用">列表外 · 兜底</span>
    : <span className="rounded-full border border-dashed border-border px-2 py-0.5 text-xs text-muted">不会下载</span>;
}

function hdrOrder(label: string) {
  if (label.includes("Dolby Vision")) return 0;
  if (label.includes("HDR10+")) return 1;
  if (label.includes("HDR10")) return 2;
  if (label === "SDR") return 4;
  return 3;
}

function TraitTag({ children, strong = false, highlight = false, muted = false }: { children: ReactNode; strong?: boolean; highlight?: boolean; muted?: boolean }) {
  return (
    <span
      className={cn(
        "rounded-md px-2 py-0.5 font-mono text-xs",
        highlight ? "bg-[#2b2140] text-white" : muted ? "border border-border text-muted" : "bg-surface-container text-secondary-foreground",
        strong && "font-semibold",
      )}
    >
      {children}
    </span>
  );
}

type PickerProps = {
  tmdbId: number;
  mediaType: string;
  season: number | null;
  absoluteNumbering: boolean;
  siteIds: number[];
  policy: GroupPolicy;
  onChange: (policy: GroupPolicy) => void;
  numbering: NumberingRule[];
  onNumberingChange: (rules: NumberingRule[]) => void;
  qualityProfileName?: string;
};

type Filter = { resolution: string; hdr: string; platform: string };

export function ReleaseGroupPicker({
  tmdbId,
  mediaType,
  season,
  absoluteNumbering,
  siteIds,
  policy,
  onChange,
  numbering,
  onNumberingChange,
  qualityProfileName,
}: PickerProps) {
  const idPrefix = useId();
  const [probe, setProbe] = useState<ProbeResponse | null>(null);
  const [probing, setProbing] = useState(false);
  const [probeError, setProbeError] = useState("");
  const [filter, setFilter] = useState<Filter>({ resolution: "all", hdr: "all", platform: "all" });
  const [manualGroup, setManualGroup] = useState("");
  const [showUnknown, setShowUnknown] = useState(false);
  const [openTorrents, setOpenTorrents] = useState<Set<string>>(() => new Set());
  const [advancedOpen, setAdvancedOpen] = useState(policy.include.length > 0 || policy.exclude.length > 0);
  const [numberingOpen, setNumberingOpen] = useState(numbering.length > 0);
  const [regroupStatus, setRegroupStatus] = useState("");
  const requestRef = useRef(0);
  const numberingKey = JSON.stringify(cleanNumbering(numbering));
  const numberingRef = useRef(numberingKey);
  numberingRef.current = numberingKey;
  const [probedNumbering, setProbedNumbering] = useState(numberingKey);
  const siteKey = siteIds.join(",");
  const isMovie = mediaType === "movie";

  const runProbe = useCallback(async () => {
    if (siteIds.length === 0) {
      setProbeError("请先在上方选择站点。");
      return;
    }
    const request = ++requestRef.current;
    const sentNumbering = numberingRef.current;
    setProbing(true);
    setProbeError("");
    try {
      const response = await api<ProbeResponse>("/api/media/release-groups/probe", {
        method: "POST",
        body: JSON.stringify({
          tmdb_id: tmdbId,
          media_type: mediaType,
          season: isMovie ? null : season,
          absolute_numbering: absoluteNumbering,
          site_ids: siteIds,
          numbering: JSON.parse(sentNumbering) as NumberingRule[],
        }),
      });
      if (request === requestRef.current) {
        setProbe(response);
        setProbedNumbering(sentNumbering);
        setRegroupStatus("");
        setOpenTorrents(new Set());
      }
    } catch (error) {
      if (request === requestRef.current) {
        setProbeError(error instanceof ApiError || error instanceof Error ? error.message : "探测失败");
      }
    } finally {
      if (request === requestRef.current) setProbing(false);
    }
    // siteKey 代替数组本身作为依赖，避免每次渲染重新探测。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tmdbId, mediaType, season, absoluteNumbering, siteKey, isMovie]);

  const probeKey = `${tmdbId}|${mediaType}|${season}|${absoluteNumbering}|${siteKey}`;
  const [probedKey, setProbedKey] = useState("");
  // 只在用户点击时探测：站点多时一次探测要逐站搜索，不能随打开弹窗或勾选站点自动触发。
  const probeNow = useCallback(() => {
    setProbedKey(probeKey);
    void runProbe();
  }, [probeKey, runProbe]);
  const stale = probe !== null && probedKey !== probeKey;
  const probeId = probe?.probe_id ?? null;

  // 修改编号映射后用缓存的探测结果重新归类，不重新逐站搜索。
  useEffect(() => {
    if (!probeId || stale || probing || numberingKey === probedNumbering) return;
    const timer = window.setTimeout(async () => {
      const request = ++requestRef.current;
      setRegroupStatus("正在按新的映射重新归类…");
      try {
        const response = await api<ProbeResponse>("/api/media/release-groups/regroup", {
          method: "POST",
          body: JSON.stringify({ probe_id: probeId, numbering: JSON.parse(numberingKey) as NumberingRule[] }),
        });
        if (request === requestRef.current) {
          setProbe(response);
          setProbedNumbering(numberingKey);
          setRegroupStatus(response.mapped ? `已按映射归入 ${response.mapped} 条资源。` : "已重新归类，没有资源命中映射。");
        }
      } catch (error) {
        if (request !== requestRef.current) return;
        if (error instanceof ApiError && error.status === 404) {
          setRegroupStatus("探测结果已过期，请重新探测查看映射效果。");
          setProbedNumbering(numberingKey);
        } else {
          setRegroupStatus(error instanceof Error ? `重新归类失败：${error.message}` : "重新归类失败");
        }
      }
    }, 350);
    return () => window.clearTimeout(timer);
  }, [probeId, stale, probing, numberingKey, probedNumbering]);

  const mismatches = useMemo(() => probe?.season_mismatches ?? [], [probe]);
  const targetSeason = season ?? 1;
  const mapSeason = (sourceSeason: number) => {
    onNumberingChange([...numbering, { keyword: null, source_season: sourceSeason, target_season: targetSeason, episode_offset: 0 }]);
    setNumberingOpen(true);
  };
  const sampleEpisode = useMemo(() => {
    const episodes = mismatches.flatMap((mismatch) => mismatch.episodes);
    return episodes.length > 0 ? Math.max(...episodes) : null;
  }, [mismatches]);

  const named = useMemo(() => (probe?.groups ?? []).filter((group) => group.name), [probe]);
  const unknown = useMemo(() => (probe?.groups ?? []).find((group) => !group.name) ?? null, [probe]);
  const allVariants = useMemo(() => named.flatMap((group) => variantsOf(group)), [named]);
  const resolutions = useMemo(() => {
    const values = new Set<string>();
    allVariants.forEach((variant) => { if (variant.resolution) values.add(variant.resolution); });
    return [...values].sort((left, right) => (parseInt(right) || 0) - (parseInt(left) || 0));
  }, [allVariants]);
  const hdrs = useMemo(() => {
    const values = new Set<string>();
    allVariants.forEach((variant) => values.add(variant.hdr));
    return [...values].sort((left, right) => hdrOrder(left) - hdrOrder(right) || left.localeCompare(right));
  }, [allVariants]);
  const platforms = useMemo(() => {
    const values = new Set<string>();
    allVariants.forEach((variant) => values.add(variantTraits(variant).platform));
    return [...values].sort((left, right) => Number(left === UNKNOWN_PLATFORM) - Number(right === UNKNOWN_PLATFORM) || left.localeCompare(right));
  }, [allVariants]);
  const variantVisible = (variant: ReleaseVariantSummary) =>
    (filter.resolution === "all" || variant.resolution === filter.resolution)
    && (filter.hdr === "all" || variant.hdr === filter.hdr)
    && (filter.platform === "all" || variantTraits(variant).platform === filter.platform);
  const visible = named
    .map((group) => ({ group, variants: variantsOf(group).filter(variantVisible) }))
    .filter(({ variants }) => variants.length > 0);
  const maxEpisode = useMemo(
    () => Math.max(0, ...named.flatMap((group) => episodesOf(group, absoluteNumbering))),
    [named, absoluteNumbering],
  );

  /** 追更顺序里每一项在探测结果中能匹配到的版本，用于说明「会下载什么」。 */
  const coverage = useMemo(() => policy.groups.map((entry) => {
    const matched = named.flatMap((group) => variantsOf(group)
      .filter((variant) => entryMatchesVariant(entry, group.name as string, variant)));
    const episodes = [...new Set(matched.flatMap((variant) => episodesOf(variant, absoluteNumbering)))].sort((a, b) => a - b);
    return {
      torrents: matched.reduce((total, variant) => total + variant.count, 0),
      episodes,
      hasPack: matched.some((variant) => variant.has_pack),
    };
  }), [policy.groups, named, absoluteNumbering]);

  const rankOfEntry = (entry: GroupEntry) => policy.groups.findIndex((existing) => sameEntry(existing, entry));
  const setPrimary = (entry: GroupEntry) => {
    const rest = policy.groups.filter((existing) => !sameEntry(existing, entry));
    onChange({ ...policy, groups: [entry, ...rest] });
  };
  const toggleBackup = (entry: GroupEntry) => {
    const rank = rankOfEntry(entry);
    if (rank === 0) return;
    if (rank > 0) onChange({ ...policy, groups: policy.groups.filter((_, index) => index !== rank) });
    else onChange({ ...policy, groups: [...policy.groups, entry] });
  };
  const moveUp = (index: number) => {
    if (index <= 0) return;
    const groups = [...policy.groups];
    [groups[index - 1], groups[index]] = [groups[index], groups[index - 1]];
    onChange({ ...policy, groups });
  };
  const remove = (index: number) => onChange({ ...policy, groups: policy.groups.filter((_, position) => position !== index) });
  const addManual = () => {
    const entry = { name: manualGroup.trim() };
    if (!entry.name || rankOfEntry(entry) >= 0) {
      setManualGroup("");
      return;
    }
    onChange({ ...policy, groups: [...policy.groups, entry] });
    setManualGroup("");
  };
  const toggleTorrents = (key: string) => setOpenTorrents((current) => {
    const next = new Set(current);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    return next;
  });
  const rulesText = (rules: string[]) => rules.join("\n");
  const parseRules = (text: string) => text.split("\n");

  const entryButtons = (entry: GroupEntry, scope: string) => {
    const rank = rankOfEntry(entry);
    return (
      <>
        <Button type="button" size="sm" variant={rank === 0 ? "default" : "outline"} aria-pressed={rank === 0} aria-label={`${scope}设为首选`} onClick={() => setPrimary(entry)}>
          {rank === 0 ? "已是首选" : "设为首选"}
        </Button>
        {policy.groups.length > 0 && rank !== 0 ? (
          <Button type="button" size="sm" variant={rank > 0 ? "secondary" : "outline"} aria-pressed={rank > 0} aria-label={rank > 0 ? `${scope}移出备选` : `${scope}加入备选`} onClick={() => toggleBackup(entry)}>
            {rank > 0 ? `移出备选 ${rank}` : "加入备选"}
          </Button>
        ) : null}
      </>
    );
  };

  const chips = (label: string, values: string[], current: string, key: keyof Filter) => (
    values.length > 1 ? (
      <div role="group" aria-label={`按${label}筛选`} className="flex flex-wrap items-center gap-1">
        <span className="mr-0.5 text-xs text-muted" aria-hidden="true">{label}</span>
        {["all", ...values].map((value) => (
          <button
            key={value}
            type="button"
            aria-pressed={current === value}
            onClick={() => setFilter((existing) => ({ ...existing, [key]: value }))}
            className={cn(
              "min-h-8 rounded-full border px-3 text-xs transition-colors",
              current === value ? "border-primary bg-accent font-semibold text-primary" : "border-border bg-card text-secondary-foreground hover:bg-accent",
            )}
          >
            {value === "all" ? "全部" : value}
          </button>
        ))}
      </div>
    ) : null
  );

  return (
    <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_340px]">
      <section aria-labelledby={`${idPrefix}-groups`} className="flex min-w-0 flex-col gap-3">
        <div className="flex flex-wrap items-end justify-between gap-3">
          <div>
            <h3 id={`${idPrefix}-groups`} className="text-base font-bold">选择制作组与版本</h3>
            <p className="mt-0.5 text-xs text-muted" role="status">
              {probing
                ? "正在各站点搜索并按制作组、画质归类…"
                : probe
                  ? `在 ${probe.successful_sites}/${probe.total_sites} 个站点找到 ${probe.matched} 条资源${probe.mapped ? `（其中 ${probe.mapped} 条经编号映射）` : ""}，归为 ${named.length} 个制作组、${allVariants.length} 个版本 · ${formatRelative(probe.probed_at)}`
                  : siteIds.length === 0
                    ? "先在上方选择站点，再点击「探测制作组」。"
                    : `选好站点后点击「探测制作组」，将在已选的 ${siteIds.length} 个站点搜索并按制作组、画质归类。`}
            </p>
          </div>
          <Button
            type="button"
            variant={stale || !probe ? "default" : "outline"}
            size="sm"
            loading={probing}
            disabled={siteIds.length === 0}
            title={siteIds.length === 0 ? "请先选择站点" : undefined}
            onClick={probeNow}
          >
            {probing ? null : <RefreshCw aria-hidden="true" />}
            {stale ? "条件已变，重新探测" : probe ? "重新探测" : "探测制作组"}
          </Button>
        </div>

        {resolutions.length > 1 || hdrs.length > 1 || platforms.length > 1 ? (
          <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
            {chips("分辨率", resolutions, filter.resolution, "resolution")}
            {chips("动态范围", hdrs, filter.hdr, "hdr")}
            {chips("平台", platforms, filter.platform, "platform")}
          </div>
        ) : null}

        {probeError ? <p role="alert" className="rounded-lg border border-destructive/30 bg-destructive/5 px-3 py-2 text-sm text-destructive">{probeError}</p> : null}
        {probe && probe.errors.length > 0 ? (
          <p className="text-xs text-muted">部分站点搜索失败：{probe.errors.slice(0, 3).map((error) => `${error.source_site}（${error.message}）`).join("；")}</p>
        ) : null}

        {probing && !probe ? (
          <div className="flex items-center gap-2 rounded-xl border border-dashed border-border p-6 text-sm text-muted">
            <LoaderCircle className="size-4 animate-spin" aria-hidden="true" />正在探测，站点较多时需要十几秒。
          </div>
        ) : null}
        {!probe && !probing ? (
          <div className="rounded-xl border border-dashed border-border p-4 text-sm text-muted">
            尚未探测。探测会在每个已选站点各搜索一次{siteIds.length > 10 ? `，当前已选 ${siteIds.length} 个站点，可先缩小到常用站点再探测` : ""}；也可以跳过探测，在右侧手动填写制作组，或不选制作组、按质量配置追更。
          </div>
        ) : null}
        {probe && named.length === 0 && !probing ? (
          <div className="rounded-xl border border-dashed border-border p-4 text-sm text-muted">
            {mismatches.length > 0
              ? "没有找到季号对得上的资源，但有同名资源用了别的季号，见下方。确认是同一季后可以设置编号映射。"
              : "暂时没有找到可识别制作组的资源。可以在右侧手动填写制作组名称，或不选制作组、按质量配置追更。"}
          </div>
        ) : null}
        {probe && named.length > 0 && visible.length === 0 ? (
          <div className="rounded-xl border border-dashed border-border p-4 text-sm text-muted">没有符合筛选条件的版本。</div>
        ) : null}

        <div className="flex flex-col gap-3">
          {visible.map(({ group, variants }) => {
            const name = group.name as string;
            const groupEntry: GroupEntry = { name };
            const groupRank = rankOfEntry(groupEntry);
            const ranks = variantsOf(group).map((variant) => variantRank(policy, name, variant)).filter((rank) => rank >= 0);
            const bestRank = ranks.length > 0 ? Math.min(...ranks) : -1;
            const episodes = episodesOf(group, absoluteNumbering);
            const hiddenVariants = variantsOf(group).length - variants.length;
            return (
              <article
                key={name}
                className={cn(
                  "flex flex-col gap-3 rounded-2xl bg-card p-4",
                  bestRank === 0 ? "border-2 border-primary" : bestRank > 0 ? "border-2 border-[#cdbde6]" : "border border-border",
                )}
              >
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <h4 className="break-all font-mono text-base font-bold">{name}</h4>
                      <span className="text-xs text-muted">
                        {group.count} 条 · {variantsOf(group).length} 个版本 · {group.sites.length} 站{group.has_pack ? " · 含合集" : ""}
                      </span>
                    </div>
                    <p className="mt-1 text-xs text-muted">
                      {!isMovie ? <>已发布 <span className="font-semibold text-secondary-foreground">{episodes.length > 0 ? compactEpisodes(episodes, absoluteNumbering ? "#" : "E") : group.has_pack ? "季包" : "—"}</span> · </> : null}
                      最近更新 {formatRelative(group.latest_publish)}
                      {groupRank >= 0 ? <> · 整组在{groupRank === 0 ? "首选" : `备选 ${groupRank}`}</> : null}
                    </p>
                  </div>
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="text-xs text-muted">整组（任意版本）</span>
                    {entryButtons(groupEntry, `${name} 整组`)}
                  </div>
                </div>

                <ul className="flex flex-col gap-2" aria-label={`${name} 的版本`}>
                  {variants.map((variant) => {
                    const key = `${name}|${variantKey(variant)}`;
                    const entry = variantEntry(name, variant);
                    const label = entryLabel(entry);
                    const effective = variantRank(policy, name, variant);
                    const open = openTorrents.has(key);
                    const variantEpisodes = episodesOf(variant, absoluteNumbering);
                    return (
                      <li
                        key={key}
                        className={cn(
                          "flex flex-col gap-2 rounded-xl border px-3 py-2.5",
                          effective === 0 ? "border-primary/50 bg-accent/50" : effective > 0 ? "border-[#cdbde6] bg-[#f7f3fc]" : "border-border bg-background",
                        )}
                      >
                        <div className="flex flex-wrap items-start justify-between gap-2">
                          <div className="flex min-w-0 flex-col gap-1">
                            <div className="flex flex-wrap items-center gap-1">
                              <TraitTag strong>{variant.resolution ?? "分辨率未标注"}</TraitTag>
                              <TraitTag>{variant.codec ?? "编码未标注"}</TraitTag>
                              <TraitTag highlight={variant.hdr.includes("Dolby Vision")}>{variant.hdr}</TraitTag>
                              {variant.frame_rate && variant.frame_rate !== STANDARD_FRAME_RATE ? <TraitTag highlight>{variant.frame_rate}</TraitTag> : null}
                              {variant.tier && variant.tier !== STANDARD_TIER ? <TraitTag highlight>{variant.tier}</TraitTag> : null}
                              {variant.platform && variant.platform !== UNKNOWN_PLATFORM ? <TraitTag>{variant.platform}</TraitTag> : null}
                              {variant.sources.map((source) => <TraitTag key={source} muted>{source}</TraitTag>)}
                              <RankBadge rank={effective} policy={policy} />
                            </div>
                            <span className="flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-secondary-foreground">
                              {!isMovie ? (
                                <span className="font-mono">
                                  {variantEpisodes.length > 0 ? compactEpisodes(variantEpisodes, absoluteNumbering ? "#" : "E") : variant.has_pack ? "季包" : "集数未知"}
                                </span>
                              ) : null}
                              <span>
                                {isMovie ? "体积" : "单集"} {sizeSpread(variant) >= 1.2
                                  ? `${formatSize(variant.min_size_per_episode ?? null)}–${formatSize(variant.max_size_per_episode ?? null)}`
                                  : formatSize(variant.size_per_episode)}
                              </span>
                              <span>做种最多 {variant.max_seeders}</span>
                              <span>{variant.sites.join("、")}</span>
                              <span>{formatRelative(variant.latest_publish)}</span>
                            </span>
                          </div>
                          <div className="flex flex-wrap items-center gap-1.5">
                            {entryButtons(entry, `${label} `)}
                            <Button
                              type="button"
                              size="sm"
                              variant="outline"
                              aria-expanded={open}
                              aria-label={`${open ? "收起" : "查看"} ${label} 的 ${variant.count} 个种子`}
                              onClick={() => toggleTorrents(key)}
                            >
                              种子 {variant.count}
                              <ChevronDown className={cn("transition-transform", open && "rotate-180")} aria-hidden="true" />
                            </Button>
                          </div>
                        </div>
                        {sizeSpread(variant) >= 2 ? (
                          <p className="text-xs text-[#9a5b0c]">
                            单集体积相差 {sizeSpread(variant).toFixed(1)} 倍：可能是每集时长不同，也可能混有标题没写出来的不同规格，展开种子核对一下。
                          </p>
                        ) : null}
                        {open ? (
                          <div className="flex flex-col gap-2 border-t border-border pt-2">
                            {!isMovie ? <EpisodeStrip episodes={variantEpisodes} maxEpisode={maxEpisode} primary={effective === 0} label={label} /> : null}
                            <TorrentList torrents={variant.torrents} absoluteNumbering={absoluteNumbering} isMovie={isMovie} />
                          </div>
                        ) : null}
                      </li>
                    );
                  })}
                </ul>
                {hiddenVariants > 0 ? <p className="text-xs text-muted">另有 {hiddenVariants} 个版本被筛选隐藏。</p> : null}
              </article>
            );
          })}
        </div>

        {unknown ? (
          <div className="rounded-2xl border border-dashed border-[#c9bedb] p-4 text-sm">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <div className="font-semibold">未识别制作组 · {unknown.count} 条</div>
                <div className="text-xs text-muted">标题里没有可识别的组名，不参与首选/备选；仅在「列表外的版本」选择按质量兜底时可能被使用。</div>
              </div>
              <Button type="button" variant="outline" size="sm" aria-expanded={showUnknown} onClick={() => setShowUnknown((value) => !value)}>
                {showUnknown ? "收起" : `查看种子 ${unknown.count}`}
              </Button>
            </div>
            {showUnknown ? (
              <div className="mt-2">
                <TorrentList
                  torrents={variantsOf(unknown).flatMap((variant) => variant.torrents)
                    .sort((left, right) => (right.result.publish_time ?? "").localeCompare(left.result.publish_time ?? ""))}
                  absoluteNumbering={absoluteNumbering}
                  isMovie={isMovie}
                />
              </div>
            ) : null}
          </div>
        ) : null}

        {!isMovie ? (
          <>
            <SeasonMismatchList
              mismatches={mismatches}
              season={targetSeason}
              rules={numbering}
              onMap={mapSeason}
              renderTorrents={(torrents) => <TorrentList torrents={torrents} absoluteNumbering={false} isMovie={false} />}
            />
            {numberingOpen || numbering.length > 0 ? (
              <NumberingRulesEditor
                rules={numbering}
                onChange={onNumberingChange}
                season={targetSeason}
                sampleEpisode={sampleEpisode}
                status={regroupStatus || (numbering.length > 0 && !probe ? "探测后可以看到映射命中了哪些种子。" : undefined)}
              />
            ) : (
              <button
                type="button"
                onClick={() => setNumberingOpen(true)}
                className="min-h-8 self-start rounded-md px-2 text-xs font-medium text-primary hover:bg-accent"
              >
                站点季号和 TMDB 不一致？设置编号映射
              </button>
            )}
          </>
        ) : null}
      </section>

      <aside className="flex min-w-0 flex-col gap-4">
        <section aria-labelledby={`${idPrefix}-chain`} className="flex flex-col gap-3 rounded-2xl border border-border bg-card p-4">
          <div>
            <h3 id={`${idPrefix}-chain`} className="text-base font-bold">追更顺序</h3>
            <p className="mt-0.5 text-xs text-muted">每集从上到下找第一个已发布的版本；只写组名时匹配该组任意版本。</p>
          </div>
          {policy.groups.length === 0 ? (
            <p className="text-sm text-muted">还没有选择制作组。不选时按质量配置{qualityProfileName ? `「${qualityProfileName}」` : ""}挑选每集资源。</p>
          ) : (
            <ol className="flex flex-col gap-1.5">
              {policy.groups.map((entry, index) => {
                const label = entryLabel(entry);
                const traits = entryTraits(entry);
                const covered = coverage[index];
                return (
                  <li key={label} className={cn("flex items-center gap-2.5 rounded-xl px-2.5 py-2", index === 0 ? "bg-accent" : "border border-border")}>
                    <span className={cn("inline-flex size-6 shrink-0 items-center justify-center rounded-full text-xs font-bold", index === 0 ? "bg-primary text-primary-foreground" : "bg-surface-container text-secondary-foreground")}>{index + 1}</span>
                    <span className="min-w-0 flex-1">
                      <span className="block truncate font-mono text-sm font-semibold">{entry.name}</span>
                      <span className="block truncate text-xs text-secondary-foreground">{traits.length > 0 ? traits.join(" · ") : "任意版本"}</span>
                      {probe && covered ? (
                        <span className={cn("block text-xs", covered.torrents > 0 ? "text-muted" : "text-[#9a5b0c]")}>
                          {covered.torrents === 0
                            ? "本次探测没有找到"
                            : `探测到 ${covered.torrents} 条${!isMovie && covered.episodes.length > 0 ? ` · ${compactEpisodes(covered.episodes, absoluteNumbering ? "#" : "E")}` : covered.hasPack && !isMovie ? " · 季包" : ""}`}
                        </span>
                      ) : null}
                    </span>
                    <button type="button" aria-label={`将 ${label} 上移`} disabled={index === 0} onClick={() => moveUp(index)} className="inline-flex size-8 shrink-0 items-center justify-center rounded-md border border-border bg-card text-secondary-foreground hover:bg-accent disabled:opacity-40">
                      <ArrowUp className="size-3.5" aria-hidden="true" />
                    </button>
                    <button type="button" aria-label={`移除 ${label}`} onClick={() => remove(index)} className="inline-flex size-8 shrink-0 items-center justify-center rounded-md border border-border bg-card text-secondary-foreground hover:bg-accent">
                      <X className="size-3.5" aria-hidden="true" />
                    </button>
                  </li>
                );
              })}
            </ol>
          )}
          <div className="flex gap-2">
            <label htmlFor={`${idPrefix}-manual`} className="sr-only">手动添加制作组</label>
            <Input
              id={`${idPrefix}-manual`}
              value={manualGroup}
              placeholder="手动添加组名，如 ANi"
              className="h-9"
              onChange={(event) => setManualGroup(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  addManual();
                }
              }}
            />
            <Button type="button" variant="outline" size="sm" onClick={addManual} disabled={!manualGroup.trim()}>
              <Plus aria-hidden="true" />添加
            </Button>
          </div>

          {policy.groups.length > 0 ? (
            <>
              <div className="flex flex-col gap-1.5 border-t border-border pt-3">
                <span className="text-sm font-semibold">首选版本没发布时</span>
                <div className="flex flex-wrap items-center gap-2 text-sm text-secondary-foreground">
                  <span>其他版本发布后等待</span>
                  <div role="group" aria-label="备选等待时长" className="inline-flex flex-wrap gap-0.5 rounded-lg bg-surface-container p-0.5">
                    {(WAIT_CHOICES.includes(policy.wait_hours) ? WAIT_CHOICES : [...WAIT_CHOICES, policy.wait_hours].sort((a, b) => a - b)).map((hours) => (
                      <button
                        key={hours}
                        type="button"
                        aria-pressed={policy.wait_hours === hours}
                        onClick={() => onChange({ ...policy, wait_hours: hours })}
                        className={cn("min-h-8 rounded-md px-2.5 text-xs", policy.wait_hours === hours ? "bg-card font-semibold text-primary shadow-sm" : "text-secondary-foreground")}
                      >
                        {waitLabel(hours)}
                      </button>
                    ))}
                  </div>
                </div>
              </div>

              <fieldset className="flex flex-col gap-1.5">
                <legend className="mb-1.5 text-sm font-semibold">列表外的版本</legend>
                {([
                  ["quality", "按质量配置兜底", "列表内都没有时，从其他组或版本里按质量挑选。"],
                  ["skip", "不下载", "只下载列表内的版本。"],
                ] as const).map(([value, label, description]) => (
                  <label key={value} className={cn("flex cursor-pointer items-start gap-2.5 rounded-lg border px-3 py-2", policy.outside === value ? "border-primary bg-accent/60" : "border-border")}>
                    <input type="radio" name={`${idPrefix}-outside`} className="mt-0.5 accent-primary" checked={policy.outside === value} onChange={() => onChange({ ...policy, outside: value })} />
                    <span>
                      <span className="block text-sm font-semibold">{label}</span>
                      <span className="block text-xs text-muted">{description}</span>
                    </span>
                  </label>
                ))}
              </fieldset>

              <label className="flex cursor-pointer items-start gap-2.5 rounded-lg border border-border px-3 py-2">
                <input type="checkbox" className="mt-0.5 size-4 accent-primary" checked={policy.upgrade} onChange={(event) => onChange({ ...policy, upgrade: event.target.checked })} />
                <span>
                  <span className="block text-sm font-semibold">更靠前的版本发布后洗版</span>
                  <span className="block text-xs text-muted">用备选或兜底版本下载后，继续关注 30 天；首选或更靠前的版本发布时自动下载替换。</span>
                </span>
              </label>
              {policy.upgrade ? (
                <label className="ml-6 flex cursor-pointer items-start gap-2.5 text-sm">
                  <input type="checkbox" className="mt-0.5 size-4 accent-primary" checked={policy.delete_replaced} onChange={(event) => onChange({ ...policy, delete_replaced: event.target.checked })} />
                  <span>
                    <span className="block font-medium">洗版后删除旧版本的种子和文件</span>
                    <span className="block text-xs text-muted">不勾选时旧版本保留做种，并打上「云母:已洗版」标签，避免 H&R。</span>
                  </span>
                </label>
              ) : null}
            </>
          ) : null}

          <div className="border-t border-border pt-3">
            <button type="button" aria-expanded={advancedOpen} onClick={() => setAdvancedOpen((value) => !value)} className="flex min-h-9 w-full items-center justify-between text-sm font-semibold">
              附加条件（可选）
              <span className="flex items-center gap-1 text-xs font-normal text-muted">
                {policy.include.length + policy.exclude.length > 0 ? `${policy.include.length + policy.exclude.length} 条规则` : "未设置"}
                <ChevronDown className={cn("size-4 transition-transform", advancedOpen && "rotate-180")} aria-hidden="true" />
              </span>
            </button>
            {advancedOpen ? (
              <div className="mt-2 flex flex-col gap-3">
                <label className="flex flex-col gap-1 text-xs font-medium text-secondary-foreground">
                  必须包含（每行一条正则，全部满足）
                  <textarea
                    rows={2}
                    value={rulesText(policy.include)}
                    onChange={(event) => onChange({ ...policy, include: parseRules(event.target.value) })}
                    placeholder="如 HDR|DV"
                    className="rounded-lg border border-border bg-input px-3 py-2 font-mono text-sm text-foreground focus-visible:border-primary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  />
                </label>
                <label className="flex flex-col gap-1 text-xs font-medium text-secondary-foreground">
                  排除（每行一条正则，命中任意一条即排除）
                  <textarea
                    rows={2}
                    value={rulesText(policy.exclude)}
                    onChange={(event) => onChange({ ...policy, exclude: parseRules(event.target.value) })}
                    placeholder="如 60fps"
                    className="rounded-lg border border-border bg-input px-3 py-2 font-mono text-sm text-foreground focus-visible:border-primary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  />
                </label>
                <p className="text-xs text-muted">
                  写成 <code className="rounded bg-surface-container px-1 font-mono">{"{{HHWEB}}:60fps"}</code> 只对该组生效；不区分大小写。
                </p>
              </div>
            ) : null}
          </div>
        </section>

        <p role="status" className="rounded-xl bg-[#e3f0ec] px-3.5 py-3 text-sm leading-5 text-[#2b5e51]">{groupPolicySummary(policy)}</p>
      </aside>
    </div>
  );
}

/** 提交前清理：去掉空行、首尾空白和空的限定。 */
export function cleanGroupPolicy(policy: GroupPolicy): GroupPolicy {
  const clean = (values: string[]) => values.map((value) => value.trim()).filter(Boolean);
  const trait = (value: string | null | undefined) => (value && value.trim() ? value.trim() : undefined);
  const groups = policy.groups
    .map((entry) => ({
      name: entry.name.trim(),
      ...Object.fromEntries(TRAIT_KEYS.map((key) => [key, trait(entry[key])])),
    }))
    .filter((entry) => entry.name);
  return { ...policy, groups, include: clean(policy.include), exclude: clean(policy.exclude) };
}

const UPGRADE_STATUS: Record<string, string> = {
  watching: "等待更好的版本",
  upgrading: "新版本下载中",
  done: "已洗版到首选",
  expired: "已停止关注",
  cancelled: "已取消",
};

export function GroupUpgradeList({ upgrades }: { upgrades: GroupUpgradeRecord[] }) {
  if (upgrades.length === 0) return null;
  return (
    <section aria-label="洗版记录" className="rounded-xl border border-border p-4">
      <h3 className="text-sm font-semibold">洗版记录</h3>
      <ul className="mt-2 flex flex-col divide-y divide-border text-sm">
        {upgrades.map((upgrade) => {
          const episode = upgrade.absolute_episode != null
            ? `#${String(upgrade.absolute_episode).padStart(2, "0")}`
            : upgrade.episode != null
              ? `S${String(upgrade.season ?? 1).padStart(2, "0")}E${String(upgrade.episode).padStart(2, "0")}`
              : "电影";
          return (
            <li key={upgrade.id} className="flex flex-wrap items-center justify-between gap-2 py-2">
              <span className="flex flex-wrap items-center gap-2">
                <span className="font-mono font-semibold">{episode}</span>
                <span className="rounded-md bg-secondary px-2 py-0.5 font-mono text-xs text-secondary-foreground">{upgrade.current_group ?? "未识别"}</span>
                {upgrade.upgrade_group ? <span className="text-xs text-muted">→ <span className="font-mono">{upgrade.upgrade_group}</span></span> : null}
                {upgrade.replaced_count > 0 ? <span className="text-xs text-muted">已替换 {upgrade.replaced_count} 次</span> : null}
              </span>
              <span className="text-xs text-muted">
                {UPGRADE_STATUS[upgrade.status] ?? upgrade.status}
                {upgrade.last_checked_at ? ` · 检查于 ${formatRelative(upgrade.last_checked_at)}` : ""}
                {upgrade.last_error ? <span className="block text-destructive">{upgrade.last_error}</span> : null}
              </span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
