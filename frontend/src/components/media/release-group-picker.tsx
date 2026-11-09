import { useCallback, useId, useMemo, useRef, useState } from "react";
import { ArrowUp, ChevronDown, LoaderCircle, Plus, RefreshCw, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { api, ApiError } from "@/lib/api";
import { cn } from "@/lib/utils";

/** 与后端 `GroupPolicy` 对应：第一个制作组为首选，其余为备选。 */
export type GroupPolicy = {
  groups: string[];
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
};

type ProbeResponse = {
  queries: string[];
  matched: number;
  groups: ReleaseGroupSummary[];
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

export function normalizeGroupPolicy(value: unknown): GroupPolicy {
  if (!value || typeof value !== "object") return { ...DEFAULT_GROUP_POLICY };
  const record = value as Record<string, unknown>;
  const strings = (input: unknown) => (Array.isArray(input) ? input.filter((item): item is string => typeof item === "string") : []);
  return {
    groups: strings(record.groups),
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

/** 列表和详情中展示的追更顺序，例如「首选 CHDWEB → HHWEB」。 */
export function GroupChain({ policy, compact = false }: { policy: GroupPolicy; compact?: boolean }) {
  if (policy.groups.length === 0) {
    return <span className="text-xs text-muted">不限制作组 · 按质量挑选</span>;
  }
  const shown = compact ? policy.groups.slice(0, 3) : policy.groups;
  return (
    <span className="inline-flex flex-wrap items-center gap-1">
      {shown.map((group, index) => (
        <span
          key={group}
          title={index === 0 ? "首选制作组" : `备选 ${index}`}
          className={cn(
            "inline-flex h-6 max-w-[160px] items-center gap-1 truncate rounded-md px-2 font-mono text-xs",
            index === 0 ? "bg-primary font-semibold text-primary-foreground" : "bg-secondary text-secondary-foreground",
          )}
        >
          {group}
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
  const [primary, ...backups] = policy.groups;
  const parts = [`每集优先下载 ${primary}`];
  if (backups.length > 0) {
    parts.push(
      policy.wait_hours === 0
        ? `没有时立即使用备选（${backups.join(" → ")}）`
        : `其他组发布 ${policy.wait_hours} 小时后仍没有，再用备选（${backups.join(" → ")}）`,
    );
  }
  parts.push(policy.outside === "quality" ? "列表内都没有时按质量配置从其他组兜底" : "列表外的制作组不下载");
  if (policy.upgrade && (backups.length > 0 || policy.outside === "quality")) {
    parts.push(policy.delete_replaced ? "更靠前的组发布后洗版并删除旧版本" : "更靠前的组发布后洗版，旧版本保留做种");
  }
  return `${parts.join("；")}。`;
}

type PickerProps = {
  tmdbId: number;
  mediaType: string;
  season: number | null;
  absoluteNumbering: boolean;
  siteIds: number[];
  policy: GroupPolicy;
  onChange: (policy: GroupPolicy) => void;
  qualityProfileName?: string;
};

export function ReleaseGroupPicker({
  tmdbId,
  mediaType,
  season,
  absoluteNumbering,
  siteIds,
  policy,
  onChange,
  qualityProfileName,
}: PickerProps) {
  const idPrefix = useId();
  const [probe, setProbe] = useState<ProbeResponse | null>(null);
  const [probing, setProbing] = useState(false);
  const [probeError, setProbeError] = useState("");
  const [resolution, setResolution] = useState("all");
  const [manualGroup, setManualGroup] = useState("");
  const [showUnknown, setShowUnknown] = useState(false);
  const [advancedOpen, setAdvancedOpen] = useState(policy.include.length > 0 || policy.exclude.length > 0);
  const requestRef = useRef(0);
  const siteKey = siteIds.join(",");
  const isMovie = mediaType === "movie";

  const runProbe = useCallback(async () => {
    if (siteIds.length === 0) {
      setProbeError("请先在上方选择站点。");
      return;
    }
    const request = ++requestRef.current;
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
        }),
      });
      if (request === requestRef.current) setProbe(response);
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

  const named = useMemo(() => (probe?.groups ?? []).filter((group) => group.name), [probe]);
  const unknown = useMemo(() => (probe?.groups ?? []).find((group) => !group.name) ?? null, [probe]);
  const resolutions = useMemo(() => {
    const values = new Set<string>();
    named.forEach((group) => group.resolutions.forEach((value) => values.add(value)));
    return [...values].sort((left, right) => (parseInt(right) || 0) - (parseInt(left) || 0));
  }, [named]);
  const visible = named.filter((group) => resolution === "all" || group.resolutions.includes(resolution));
  const maxEpisode = useMemo(
    () => Math.max(0, ...named.flatMap((group) => (absoluteNumbering ? group.absolute_episodes : group.episodes))),
    [named, absoluteNumbering],
  );

  const rankOf = (name: string) => policy.groups.findIndex((group) => sameGroup(group, name));
  const setPrimary = (name: string) => {
    const rest = policy.groups.filter((group) => !sameGroup(group, name));
    onChange({ ...policy, groups: [name, ...rest] });
  };
  const toggleBackup = (name: string) => {
    const rank = rankOf(name);
    if (rank === 0) return;
    if (rank > 0) onChange({ ...policy, groups: policy.groups.filter((_, index) => index !== rank) });
    else onChange({ ...policy, groups: policy.groups.length === 0 ? [name] : [...policy.groups, name] });
  };
  const moveUp = (index: number) => {
    if (index <= 0) return;
    const groups = [...policy.groups];
    [groups[index - 1], groups[index]] = [groups[index], groups[index - 1]];
    onChange({ ...policy, groups });
  };
  const remove = (index: number) => onChange({ ...policy, groups: policy.groups.filter((_, position) => position !== index) });
  const addManual = () => {
    const name = manualGroup.trim();
    if (!name || rankOf(name) >= 0) {
      setManualGroup("");
      return;
    }
    onChange({ ...policy, groups: [...policy.groups, name] });
    setManualGroup("");
  };
  const rulesText = (rules: string[]) => rules.join("\n");
  const parseRules = (text: string) => text.split("\n");

  return (
    <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_340px]">
      <section aria-labelledby={`${idPrefix}-groups`} className="flex min-w-0 flex-col gap-3">
        <div className="flex flex-wrap items-end justify-between gap-3">
          <div>
            <h3 id={`${idPrefix}-groups`} className="text-base font-bold">选择制作组</h3>
            <p className="mt-0.5 text-xs text-muted" role="status">
              {probing
                ? "正在各站点搜索并按制作组归类…"
                : probe
                  ? `在 ${probe.successful_sites}/${probe.total_sites} 个站点找到 ${probe.matched} 条资源，归为 ${named.length} 个制作组 · ${formatRelative(probe.probed_at)}`
                  : siteIds.length === 0
                    ? "先在上方选择站点，再点击「探测制作组」。"
                    : `选好站点后点击「探测制作组」，将在已选的 ${siteIds.length} 个站点搜索并按制作组归类。`}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            {resolutions.length > 1 ? (
              <div role="group" aria-label="按分辨率筛选" className="flex flex-wrap gap-1">
                {["all", ...resolutions].map((value) => (
                  <button
                    key={value}
                    type="button"
                    aria-pressed={resolution === value}
                    onClick={() => setResolution(value)}
                    className={cn(
                      "min-h-8 rounded-full border px-3 text-xs transition-colors",
                      resolution === value ? "border-primary bg-accent font-semibold text-primary" : "border-border bg-card text-secondary-foreground hover:bg-accent",
                    )}
                  >
                    {value === "all" ? "全部" : value}
                  </button>
                ))}
              </div>
            ) : null}
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
        </div>

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
            暂时没有找到可识别制作组的资源。可以在右侧手动填写制作组名称，或不选制作组、按质量配置追更。
          </div>
        ) : null}

        <div className="flex flex-col gap-3">
          {visible.map((group) => {
            const name = group.name as string;
            const rank = rankOf(name);
            const episodes = absoluteNumbering ? group.absolute_episodes : group.episodes;
            const tags = [...group.sources, ...group.resolutions, ...group.codecs, ...group.hdr_formats];
            return (
              <article
                key={name}
                className={cn(
                  "flex flex-col gap-3 rounded-2xl bg-card p-4",
                  rank === 0 ? "border-2 border-primary" : rank > 0 ? "border-2 border-[#cdbde6]" : "border border-border",
                )}
              >
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <h4 className="break-all font-mono text-base font-bold">{name}</h4>
                      {rank === 0 ? <span className="rounded-full bg-primary px-2 py-0.5 text-xs font-semibold text-primary-foreground">首选</span> : null}
                      {rank > 0 ? <span className="rounded-full bg-accent px-2 py-0.5 text-xs font-semibold text-[#4e2f77]">备选 {rank}</span> : null}
                      <span className="text-xs text-muted">{group.count} 条 · {group.sites.length} 站{group.has_pack ? " · 含合集" : ""}</span>
                    </div>
                    {tags.length > 0 ? (
                      <div className="mt-1.5 flex flex-wrap gap-1">
                        {tags.map((tag) => <span key={tag} className="rounded-md bg-surface-container px-2 py-0.5 text-xs text-secondary-foreground">{tag}</span>)}
                      </div>
                    ) : null}
                  </div>
                  <div className="flex flex-wrap gap-1.5">
                    <Button type="button" size="sm" variant={rank === 0 ? "default" : "outline"} aria-pressed={rank === 0} onClick={() => setPrimary(name)}>
                      {rank === 0 ? "已设为首选" : "设为首选"}
                    </Button>
                    <Button type="button" size="sm" variant={rank > 0 ? "secondary" : "outline"} aria-pressed={rank > 0} disabled={rank === 0} onClick={() => toggleBackup(name)}>
                      {rank === 0 ? "首选组" : rank > 0 ? "移出备选" : policy.groups.length === 0 ? "设为首选" : "加入备选"}
                    </Button>
                  </div>
                </div>

                <dl className="grid grid-cols-2 gap-3 text-sm sm:grid-cols-4">
                  {!isMovie ? (
                    <div className="col-span-2 sm:col-span-1">
                      <dt className="text-xs text-muted">已发布</dt>
                      <dd className="font-semibold">{episodes.length > 0 ? compactEpisodes(episodes, absoluteNumbering ? "#" : "E") : group.has_pack ? "季包" : "—"}</dd>
                    </div>
                  ) : null}
                  <div>
                    <dt className="text-xs text-muted">最近更新</dt>
                    <dd className="font-semibold">{formatRelative(group.latest_publish)}</dd>
                  </div>
                  <div>
                    <dt className="text-xs text-muted">{isMovie ? "体积" : "单集体积"}</dt>
                    <dd className="font-semibold">{formatSize(group.size_per_episode)}</dd>
                  </div>
                  <div>
                    <dt className="text-xs text-muted">做种（最多）</dt>
                    <dd className="font-semibold">{group.max_seeders}</dd>
                  </div>
                </dl>

                {!isMovie && maxEpisode > 0 && maxEpisode <= 120 ? (
                  <div className="flex flex-wrap gap-[3px]" role="img" aria-label={`${name} 已发布 ${episodes.length} 集`}>
                    {Array.from({ length: maxEpisode }, (_, index) => index + 1).map((episode) => {
                      const has = episodes.includes(episode);
                      return (
                        <span
                          key={episode}
                          className={cn(
                            "h-5 min-w-[26px] rounded px-0.5 text-center font-mono text-[10px] leading-5",
                            has ? (rank === 0 ? "bg-primary text-primary-foreground" : "bg-[#9a82c2] text-white") : "border border-dashed border-[#c9bedb] leading-[18px] text-muted",
                          )}
                        >
                          {episode}
                        </span>
                      );
                    })}
                  </div>
                ) : null}

                <p className="break-all rounded-lg bg-background px-3 py-2 font-mono text-xs leading-5 text-secondary-foreground">
                  <span className="text-muted">样本 · {group.sites.join("、")} · </span>{group.sample_title}
                </p>
              </article>
            );
          })}
        </div>

        {unknown ? (
          <div className="rounded-2xl border border-dashed border-[#c9bedb] p-4 text-sm">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <div className="font-semibold">未识别制作组 · {unknown.count} 条</div>
                <div className="text-xs text-muted">标题里没有可识别的组名，不参与首选/备选；仅在「列表外制作组」选择按质量兜底时可能被使用。</div>
              </div>
              <Button type="button" variant="outline" size="sm" aria-expanded={showUnknown} onClick={() => setShowUnknown((value) => !value)}>
                {showUnknown ? "收起" : "查看样本"}
              </Button>
            </div>
            {showUnknown ? <p className="mt-2 break-all font-mono text-xs text-secondary-foreground">{unknown.sample_title}</p> : null}
          </div>
        ) : null}
      </section>

      <aside className="flex min-w-0 flex-col gap-4">
        <section aria-labelledby={`${idPrefix}-chain`} className="flex flex-col gap-3 rounded-2xl border border-border bg-card p-4">
          <h3 id={`${idPrefix}-chain`} className="text-base font-bold">追更顺序</h3>
          {policy.groups.length === 0 ? (
            <p className="text-sm text-muted">还没有选择制作组。不选时按质量配置{qualityProfileName ? `「${qualityProfileName}」` : ""}挑选每集资源。</p>
          ) : (
            <ol className="flex flex-col gap-1.5">
              {policy.groups.map((group, index) => (
                <li key={group} className={cn("flex items-center gap-2.5 rounded-xl px-2.5 py-2", index === 0 ? "bg-accent" : "border border-border")}>
                  <span className={cn("inline-flex size-6 shrink-0 items-center justify-center rounded-full text-xs font-bold", index === 0 ? "bg-primary text-primary-foreground" : "bg-surface-container text-secondary-foreground")}>{index + 1}</span>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate font-mono text-sm font-semibold">{group}</span>
                    <span className="block text-xs text-muted">{index === 0 ? "首选" : `备选 ${index}`}</span>
                  </span>
                  <button type="button" aria-label={`将 ${group} 上移`} disabled={index === 0} onClick={() => moveUp(index)} className="inline-flex size-8 items-center justify-center rounded-md border border-border bg-card text-secondary-foreground hover:bg-accent disabled:opacity-40">
                    <ArrowUp className="size-3.5" aria-hidden="true" />
                  </button>
                  <button type="button" aria-label={`移除 ${group}`} onClick={() => remove(index)} className="inline-flex size-8 items-center justify-center rounded-md border border-border bg-card text-secondary-foreground hover:bg-accent">
                    <X className="size-3.5" aria-hidden="true" />
                  </button>
                </li>
              ))}
            </ol>
          )}
          <div className="flex gap-2">
            <label htmlFor={`${idPrefix}-manual`} className="sr-only">手动添加制作组</label>
            <Input
              id={`${idPrefix}-manual`}
              value={manualGroup}
              placeholder="手动添加，如 ANi"
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
                <span className="text-sm font-semibold">首选组没发布时</span>
                <div className="flex flex-wrap items-center gap-2 text-sm text-secondary-foreground">
                  <span>其他组发布后等待</span>
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
                <legend className="mb-1.5 text-sm font-semibold">列表外的制作组</legend>
                {([
                  ["quality", "按质量配置兜底", "列表内都没有时，从其他组里按质量挑选。"],
                  ["skip", "不下载", "只追列表内的制作组。"],
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
                  <span className="block text-sm font-semibold">更靠前的组发布后洗版</span>
                  <span className="block text-xs text-muted">用备选或兜底版本下载后，继续关注 30 天；首选或更靠前的组发布时自动下载替换。</span>
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

/** 提交前清理：去掉空行、首尾空白。 */
export function cleanGroupPolicy(policy: GroupPolicy): GroupPolicy {
  const clean = (values: string[]) => values.map((value) => value.trim()).filter(Boolean);
  return { ...policy, groups: clean(policy.groups), include: clean(policy.include), exclude: clean(policy.exclude) };
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
