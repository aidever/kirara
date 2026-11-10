import { useId, useState, type ReactNode } from "react";
import { ArrowRight, ChevronDown, Plus, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";

/**
 * 识别预处理：站点标题里季号为 `source_season` 的资源视为 TMDB 第 `target_season` 季，
 * 集号加上 `episode_offset`。与后端 `NumberingRule` 对应。
 */
export type NumberingRule = {
  keyword?: string | null;
  source_season: number;
  target_season: number;
  episode_offset: number;
};

export const MAX_NUMBERING_RULES = 8;

export function normalizeNumbering(value: unknown): NumberingRule[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item): NumberingRule[] => {
    if (!item || typeof item !== "object") return [];
    const record = item as Record<string, unknown>;
    if (typeof record.source_season !== "number" || typeof record.target_season !== "number") return [];
    return [{
      keyword: typeof record.keyword === "string" && record.keyword.trim() ? record.keyword.trim() : null,
      source_season: record.source_season,
      target_season: record.target_season,
      episode_offset: typeof record.episode_offset === "number" ? record.episode_offset : 0,
    }];
  });
}

/** 提交前清理：去掉空关键词和不改变任何编号的规则。 */
export function cleanNumbering(rules: NumberingRule[]): NumberingRule[] {
  return rules
    .map((rule) => ({
      keyword: rule.keyword?.trim() || null,
      source_season: Math.max(0, Math.trunc(rule.source_season || 0)),
      target_season: Math.max(0, Math.trunc(rule.target_season || 0)),
      episode_offset: Math.trunc(rule.episode_offset || 0),
    }))
    .filter((rule) => rule.source_season !== rule.target_season || rule.episode_offset !== 0);
}

function pad(value: number) {
  return String(value).padStart(2, "0");
}

export function numberingRuleLabel(rule: NumberingRule) {
  const offset = rule.episode_offset === 0 ? "" : `，集数${rule.episode_offset > 0 ? "+" : ""}${rule.episode_offset}`;
  const keyword = rule.keyword ? `（标题含「${rule.keyword}」）` : "";
  return `站点 S${pad(rule.source_season)} → 第 ${rule.target_season} 季${offset}${keyword}`;
}

/** 与后端一致：把一个站点集号换算成 TMDB 集号，用于编辑时即时预览。 */
function previewEpisode(rule: NumberingRule, episode: number) {
  const mapped = episode + (rule.episode_offset || 0);
  return mapped > 0 ? `S${pad(rule.target_season)}E${pad(mapped)}` : "不适用";
}

function NumberInput({ id, value, onChange, min, label, className }: {
  id: string;
  value: number;
  onChange: (value: number) => void;
  min?: number;
  label: string;
  className?: string;
}) {
  // 允许输入过程中出现空串或单独的负号，失焦时再归整。
  const [draft, setDraft] = useState<string | null>(null);
  return (
    <>
      <label htmlFor={id} className="sr-only">{label}</label>
      <Input
        id={id}
        inputMode="numeric"
        value={draft ?? String(value)}
        className={cn("h-9 w-16 px-2 text-center font-mono", className)}
        onChange={(event) => {
          const text = event.target.value.replace(/[^\d-]/g, "");
          setDraft(text);
          const parsed = Number.parseInt(text, 10);
          if (Number.isFinite(parsed) && (min == null || parsed >= min)) onChange(parsed);
        }}
        onBlur={() => setDraft(null)}
      />
    </>
  );
}

type EditorProps = {
  rules: NumberingRule[];
  onChange: (rules: NumberingRule[]) => void;
  /** 订阅当前选的 TMDB 季，新规则默认映射到这一季。 */
  season: number;
  /** 一个可用于预览的站点集号，取自季号不符的资源。 */
  sampleEpisode?: number | null;
  status?: string;
};

export function NumberingRulesEditor({ rules, onChange, season, sampleEpisode, status }: EditorProps) {
  const idPrefix = useId();
  const update = (index: number, patch: Partial<NumberingRule>) =>
    onChange(rules.map((rule, position) => (position === index ? { ...rule, ...patch } : rule)));
  const remove = (index: number) => onChange(rules.filter((_, position) => position !== index));
  const add = () => onChange([...rules, { keyword: null, source_season: season + 1, target_season: season, episode_offset: 0 }]);

  return (
    <section aria-labelledby={`${idPrefix}-title`} className="flex flex-col gap-3 rounded-2xl border border-border bg-card p-4">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <h4 id={`${idPrefix}-title`} className="text-sm font-bold">编号映射</h4>
          <p className="mt-0.5 text-xs text-muted">
            站点自行编了季号时，先把种子的季/集换算成 TMDB 编号再匹配。只换算解析结果，不改种子名。
          </p>
        </div>
        {rules.length < MAX_NUMBERING_RULES ? (
          <Button type="button" size="sm" variant="outline" onClick={add}>
            <Plus aria-hidden="true" />添加规则
          </Button>
        ) : null}
      </div>

      {rules.length === 0 ? <p className="text-xs text-muted">没有规则，按标题里的季/集原样匹配。</p> : null}

      <ul className="flex flex-col gap-2">
        {rules.map((rule, index) => {
          const key = `${idPrefix}-${index}`;
          const noop = rule.source_season === rule.target_season && rule.episode_offset === 0;
          const otherSeason = rule.target_season !== season;
          return (
            <li key={index} className="flex flex-col gap-2 rounded-xl border border-border bg-background px-3 py-2.5">
              <div className="flex flex-wrap items-center gap-x-2 gap-y-2 text-sm text-secondary-foreground">
                <span>站点</span>
                <span className="font-mono text-xs text-muted">S</span>
                <NumberInput id={`${key}-source`} label="站点上的季号" value={rule.source_season} min={0} onChange={(value) => update(index, { source_season: value })} />
                <ArrowRight className="size-4 text-muted" aria-hidden="true" />
                <span>TMDB 第</span>
                <NumberInput id={`${key}-target`} label="对应的 TMDB 季号" value={rule.target_season} min={0} onChange={(value) => update(index, { target_season: value })} />
                <span>季，集数</span>
                <NumberInput id={`${key}-offset`} label="集数偏移" value={rule.episode_offset} onChange={(value) => update(index, { episode_offset: value })} className="w-20" />
                <button
                  type="button"
                  aria-label={`删除规则：${numberingRuleLabel(rule)}`}
                  onClick={() => remove(index)}
                  className="ml-auto inline-flex size-8 shrink-0 items-center justify-center rounded-md border border-border bg-card text-secondary-foreground hover:bg-accent"
                >
                  <X className="size-3.5" aria-hidden="true" />
                </button>
              </div>
              <div className="flex flex-wrap items-center gap-2">
                <label htmlFor={`${key}-keyword`} className="text-xs text-muted">仅标题含</label>
                <Input
                  id={`${key}-keyword`}
                  value={rule.keyword ?? ""}
                  placeholder="可选，如 LUOXIAOHEI 或 Bilibili"
                  className="h-9 min-w-0 flex-1 text-sm"
                  onChange={(event) => update(index, { keyword: event.target.value })}
                />
              </div>
              <p className={cn("text-xs", noop || otherSeason ? "text-[#9a5b0c]" : "text-muted")}>
                {noop
                  ? "季号相同且偏移为 0，这条规则不会改变任何编号，保存时会忽略。"
                  : `例：站点 S${pad(rule.source_season)}E${pad(sampleEpisode ?? 1)} → ${previewEpisode(rule, sampleEpisode ?? 1)}`
                    + (otherSeason ? `；当前订阅的是第 ${season} 季，换算到其他季的资源不会被下载` : "")}
              </p>
            </li>
          );
        })}
      </ul>
      {status ? <p role="status" className="text-xs text-muted">{status}</p> : null}
    </section>
  );
}

/** 与后端 `SeasonMismatchSummary` 对应。 */
export type SeasonMismatchSummary<Torrent> = {
  season: number;
  count: number;
  episodes: number[];
  groups: string[];
  sites: string[];
  latest_publish: string | null;
  torrents: Torrent[];
};

function compactRange(episodes: number[]) {
  if (episodes.length === 0) return "";
  const sorted = [...episodes].sort((a, b) => a - b);
  const first = sorted[0];
  const last = sorted[sorted.length - 1];
  return first === last ? `E${pad(first)}` : `E${pad(first)}–E${pad(last)}（${sorted.length} 集）`;
}

export function SeasonMismatchList<Torrent>({
  mismatches,
  season,
  rules,
  onMap,
  renderTorrents,
}: {
  mismatches: SeasonMismatchSummary<Torrent>[];
  season: number;
  rules: NumberingRule[];
  onMap: (sourceSeason: number) => void;
  renderTorrents: (torrents: Torrent[]) => ReactNode;
}) {
  const [open, setOpen] = useState<Set<number>>(() => new Set());
  if (mismatches.length === 0) return null;
  const toggle = (value: number) => setOpen((current) => {
    const next = new Set(current);
    if (next.has(value)) next.delete(value);
    else next.add(value);
    return next;
  });
  return (
    <section className="flex flex-col gap-2 rounded-2xl border border-dashed border-[#d9b98a] bg-[#fdf8f0] p-4">
      <div>
        <h4 className="text-sm font-bold text-[#7a4a0a]">同名但季号对不上</h4>
        <p className="mt-0.5 text-xs text-[#8a6a3a]">
          标题和年份都对得上，只是季号不是第 {season} 季。可能是别的季，也可能是站点自己编了季号（比如 TMDB 只有一季，种子却叫 S03）。核对种子后可以按此映射。
        </p>
      </div>
      <ul className="flex flex-col gap-2">
        {mismatches.map((mismatch) => {
          const expanded = open.has(mismatch.season);
          const hasRule = rules.some((rule) => rule.source_season === mismatch.season);
          return (
            <li key={mismatch.season} className="flex flex-col gap-2 rounded-xl border border-border bg-card px-3 py-2.5">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="font-mono text-sm font-bold">站点 S{pad(mismatch.season)}</span>
                    <span className="text-xs text-muted">
                      {mismatch.count} 条{mismatch.episodes.length > 0 ? ` · ${compactRange(mismatch.episodes)}` : ""}
                    </span>
                  </div>
                  <p className="mt-0.5 break-all text-xs text-secondary-foreground">
                    {mismatch.groups.length > 0 ? `${mismatch.groups.slice(0, 4).join("、")}${mismatch.groups.length > 4 ? " 等" : ""} · ` : ""}
                    {mismatch.sites.slice(0, 4).join("、")}
                  </p>
                </div>
                <div className="flex flex-wrap items-center gap-1.5">
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    aria-expanded={expanded}
                    onClick={() => toggle(mismatch.season)}
                  >
                    种子
                    <ChevronDown className={cn("transition-transform", expanded && "rotate-180")} aria-hidden="true" />
                  </Button>
                  <Button type="button" size="sm" variant={hasRule ? "outline" : "default"} onClick={() => onMap(mismatch.season)}>
                    {hasRule ? "再加一条映射" : "按此映射"}
                  </Button>
                </div>
              </div>
              {hasRule ? (
                <p className="text-xs text-[#9a5b0c]">已有站点 S{pad(mismatch.season)} 的规则，但没有命中这些种子：检查关键词是否出现在标题里，或偏移后集数是否小于 1。</p>
              ) : null}
              {expanded ? <div className="border-t border-border pt-2">{renderTorrents(mismatch.torrents)}</div> : null}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
