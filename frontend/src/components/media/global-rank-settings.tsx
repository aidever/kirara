import { useEffect, useMemo, useState } from "react";
import { ExternalLink, LoaderCircle, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { api } from "@/lib/api";
import { cn } from "@/lib/utils";
import type {
  MaoyanChartKey,
  MaoyanPlatformKey,
  RankOption,
  RankResourceOption,
  RankSettings,
  RankSettingsOptions,
  RankSubscribeRecord,
} from "./global-rank-types";

type SettingsPayload = { settings: RankSettings; options: RankSettingsOptions };

function describe(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function Toggle({ id, checked, onChange, label, hint }: { id: string; checked: boolean; onChange: (value: boolean) => void; label: string; hint?: string }) {
  return (
    <div className="flex items-start gap-3">
      <button
        type="button"
        role="switch"
        id={id}
        aria-checked={checked}
        onClick={() => onChange(!checked)}
        className={cn(
          "relative mt-0.5 inline-flex h-6 w-11 shrink-0 cursor-pointer rounded-full border-2 border-transparent transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40",
          checked ? "bg-primary" : "bg-border",
        )}
      >
        <span className={cn("pointer-events-none inline-block size-5 rounded-full bg-white shadow-sm transition-transform", checked ? "translate-x-5" : "translate-x-0")} />
      </button>
      <label htmlFor={id} className="cursor-pointer text-sm">
        <span className="block font-medium">{label}</span>
        {hint ? <span className="mt-0.5 block text-xs text-muted">{hint}</span> : null}
      </label>
    </div>
  );
}

/** 一组可多选的标签按钮。 */
function ChipGroup({ label, options, value, onChange, disabled }: { label: string; options: RankOption[]; value: string[]; onChange: (value: string[]) => void; disabled?: boolean }) {
  const toggle = (option: string) => onChange(value.includes(option) ? value.filter((item) => item !== option) : [...value, option]);
  return (
    <fieldset className="flex flex-col gap-2" disabled={disabled}>
      <legend className="mb-2 text-xs font-medium text-muted">{label}</legend>
      <div className="flex flex-wrap gap-1.5">
        {options.map((option) => {
          const selected = value.includes(option.value);
          return (
            <button
              key={option.value}
              type="button"
              aria-pressed={selected}
              onClick={() => toggle(option.value)}
              className={cn(
                "h-8 cursor-pointer rounded-full border px-3 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40 disabled:cursor-not-allowed disabled:opacity-50",
                selected ? "border-primary/40 bg-primary/10 text-foreground" : "border-border text-muted hover:bg-accent hover:text-foreground",
              )}
            >
              {option.label}
            </button>
          );
        })}
      </div>
    </fieldset>
  );
}

function CountField({ id, label, value, max, onChange, disabled }: { id: string; label: string; value: number; max: number; onChange: (value: number) => void; disabled?: boolean }) {
  return (
    <label htmlFor={id} className="flex items-center justify-between gap-3 rounded-lg border border-border bg-surface-container/35 px-3 py-2 text-sm">
      <span className="min-w-0">{label}</span>
      <span className="flex shrink-0 items-center gap-1.5 text-xs text-muted">
        前
        <Input
          id={id}
          type="number"
          inputMode="numeric"
          min={0}
          max={max}
          disabled={disabled}
          value={value}
          onChange={(event) => onChange(Math.max(0, Math.min(max, Number.parseInt(event.target.value, 10) || 0)))}
          className="h-8 w-16 px-2 text-center tabular-nums"
        />
        条
      </span>
    </label>
  );
}

function Section({ title, description, enabled, onToggle, children }: { title: string; description: string; enabled?: boolean; onToggle?: (value: boolean) => void; children?: React.ReactNode }) {
  const id = `rank-setting-${title}`;
  return (
    <section className="flex flex-col gap-4 rounded-2xl border border-border bg-card p-4" aria-labelledby={`${id}-title`}>
      <div className="flex items-start justify-between gap-3">
        <div>
          <h3 id={`${id}-title`} className="text-sm font-semibold">{title}</h3>
          <p className="mt-0.5 text-xs leading-5 text-muted">{description}</p>
        </div>
        {onToggle ? (
          <button
            type="button"
            role="switch"
            aria-checked={Boolean(enabled)}
            aria-label={`启用${title}`}
            onClick={() => onToggle(!enabled)}
            className={cn(
              "relative inline-flex h-6 w-11 shrink-0 cursor-pointer rounded-full border-2 border-transparent transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40",
              enabled ? "bg-primary" : "bg-border",
            )}
          >
            <span className={cn("pointer-events-none inline-block size-5 rounded-full bg-white shadow-sm transition-transform", enabled ? "translate-x-5" : "translate-x-0")} />
          </button>
        ) : null}
      </div>
      {enabled === false ? null : children}
    </section>
  );
}

const PLATFORM_SUBSCRIBE_KEYS: Record<Exclude<MaoyanPlatformKey, "all">, keyof RankSettings["maoyan"]> = {
  tencent: "subscribe_tencent",
  iqiyi: "subscribe_iqiyi",
  mango: "subscribe_mango",
  youku: "subscribe_youku",
};

export function RankSettingsDialog({
  open,
  onClose,
  onSaved,
  profiles,
  downloaders,
  sites,
}: {
  open: boolean;
  onClose: () => void;
  onSaved: () => void;
  profiles: RankResourceOption[];
  downloaders: RankResourceOption[];
  sites: RankResourceOption[];
}) {
  const [payload, setPayload] = useState<SettingsPayload | null>(null);
  const [form, setForm] = useState<RankSettings | null>(null);
  const [loadError, setLoadError] = useState("");
  const [saveError, setSaveError] = useState("");
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (!open) return;
    let active = true;
    setLoadError("");
    setSaveError("");
    api<SettingsPayload>("/api/media/global-rank/settings")
      .then((result) => {
        if (!active) return;
        setPayload(result);
        setForm(result.settings);
      })
      .catch((error) => { if (active) setLoadError(describe(error)); });
    return () => { active = false; };
  }, [open]);

  const options = payload?.options;
  const maxCount = options?.max_subscribe_count ?? 30;
  const update = <K extends keyof RankSettings>(key: K, patch: Partial<RankSettings[K]>) =>
    setForm((current) => (current ? { ...current, [key]: { ...(current[key] as object), ...patch } } : current));

  const chartGroups = useMemo(() => {
    const groups = new Map<string, RankOption[]>();
    options?.maoyan_charts.forEach((option) => {
      const group = option.group ?? "";
      groups.set(group, [...(groups.get(group) ?? []), option]);
    });
    return [...groups.entries()];
  }, [options]);
  const musicGroups = useMemo(() => {
    const groups = new Map<string, RankOption[]>();
    options?.music_boards.forEach((option) => {
      const group = option.group ?? "";
      groups.set(group, [...(groups.get(group) ?? []), option]);
    });
    return [...groups.entries()];
  }, [options]);

  const subscribing = form
    ? form.maoyan.subscribe_movie + form.maoyan.subscribe_tv + form.maoyan.subscribe_variety + form.maoyan.subscribe_tencent + form.maoyan.subscribe_iqiyi + form.maoyan.subscribe_mango + form.maoyan.subscribe_youku > 0
    : false;
  const missingDefaults = Boolean(form?.maoyan.enabled && subscribing && (!form.subscribe.quality_profile_id || !form.subscribe.downloader_id));

  const save = async () => {
    if (!form) return;
    setSaving(true);
    setSaveError("");
    try {
      await api<SettingsPayload>("/api/media/global-rank/settings", { method: "PUT", body: JSON.stringify(form) });
      onSaved();
      onClose();
    } catch (error) {
      setSaveError(describe(error));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="榜单设置"
      description="选择要抓取的数据源和榜单，以及猫眼榜单的自动订阅。新增的榜单保存后会在后台抓取。"
      panelClassName="max-w-3xl"
      footer={
        <div className="flex flex-wrap items-center justify-end gap-2">
          {saveError ? <p role="alert" className="mr-auto text-sm text-destructive">{saveError}</p> : null}
          <Button variant="outline" onClick={onClose}>取消</Button>
          <Button disabled={!form || saving || missingDefaults} onClick={() => void save()}>
            {saving ? <LoaderCircle className="animate-spin" data-icon="inline-start" /> : null}保存
          </Button>
        </div>
      }
    >
      {loadError ? (
        <p role="alert" className="p-6 text-sm text-destructive">设置读取失败：{loadError}</p>
      ) : !form || !options ? (
        <p role="status" className="flex items-center gap-2 p-6 text-sm text-muted"><LoaderCircle className="size-4 animate-spin" aria-hidden="true" />正在读取设置</p>
      ) : (
        <div className="flex flex-col gap-4 bg-background/60 p-4 sm:p-6">
          <Toggle
            id="rank-auto-refresh"
            checked={form.auto_refresh}
            onChange={(value) => setForm({ ...form, auto_refresh: value })}
            label="后台自动刷新"
            hint="猫眼约每小时、红果短剧与音乐每 3 小时、Netflix 每 12 小时检查一次；关闭后只在点击刷新时抓取，自动订阅也随之停止。"
          />

          <Section
            title="猫眼榜单"
            description="票房榜可直接抓取；热度榜（网络电影、剧集、综艺）需要在「自动签到 → 配置签到工具」中配置 Browserless 或 Lightpanda。条目会先匹配豆瓣（带评分），再定位 TMDB。"
            enabled={form.maoyan.enabled}
            onToggle={(value) => update("maoyan", { enabled: value })}
          >
            {chartGroups.map(([group, items]) => (
              <ChipGroup
                key={group}
                label={`${group}榜单`}
                options={items}
                value={form.maoyan.charts}
                onChange={(value) => update("maoyan", { charts: value as MaoyanChartKey[] })}
              />
            ))}
            <ChipGroup
              label="平台专项榜（剧集、综艺热度榜额外抓取这些平台）"
              options={options.maoyan_platforms}
              value={form.maoyan.platforms}
              onChange={(value) => update("maoyan", { platforms: value as MaoyanPlatformKey[] })}
            />
            <div className="flex flex-col gap-2">
              <p className="text-xs font-medium text-muted">自动订阅（每轮刷新后把榜单前 N 条加入订阅，0 为只看不订阅；每部只自动订阅一次）</p>
              <div className="grid gap-2 sm:grid-cols-2">
                <CountField id="rank-sub-movie" label="电影票房 / 网络电影" value={form.maoyan.subscribe_movie} max={maxCount} onChange={(value) => update("maoyan", { subscribe_movie: value })} />
                <CountField id="rank-sub-tv" label="全网剧集综合" value={form.maoyan.subscribe_tv} max={maxCount} onChange={(value) => update("maoyan", { subscribe_tv: value })} />
                <CountField id="rank-sub-variety" label="全网综艺" value={form.maoyan.subscribe_variety} max={maxCount} onChange={(value) => update("maoyan", { subscribe_variety: value })} />
              </div>
              <div className="grid gap-2 sm:grid-cols-2">
                {options.maoyan_platforms.map((platform) => {
                  const key = PLATFORM_SUBSCRIBE_KEYS[platform.value as Exclude<MaoyanPlatformKey, "all">];
                  if (!key) return null;
                  return (
                    <CountField
                      key={platform.value}
                      id={`rank-sub-${platform.value}`}
                      label={platform.label}
                      value={form.maoyan[key] as number}
                      max={maxCount}
                      onChange={(value) => update("maoyan", { [key]: value } as Partial<RankSettings["maoyan"]>)}
                    />
                  );
                })}
              </div>
              <p className="text-xs text-muted">平台条数对该平台的剧集综合榜和综艺榜都生效；设置了条数的平台即使没勾选专项榜也会抓取。剧集没写季号时订阅 TMDB 最新一季。</p>
            </div>
            {subscribing ? (
              <div className="grid gap-3 rounded-xl border border-border bg-surface-container/35 p-3 sm:grid-cols-2">
                <label className="flex flex-col gap-1.5 text-xs font-medium text-muted">
                  质量配置
                  <Select
                    aria-label="自动订阅使用的质量配置"
                    value={form.subscribe.quality_profile_id ? String(form.subscribe.quality_profile_id) : ""}
                    onChange={(value) => update("subscribe", { quality_profile_id: value ? Number(value) : null })}
                    options={profiles.map((profile) => ({ value: String(profile.id), label: profile.name }))}
                    placeholder="选择质量配置"
                  />
                </label>
                <label className="flex flex-col gap-1.5 text-xs font-medium text-muted">
                  下载器
                  <Select
                    aria-label="自动订阅使用的下载器"
                    value={form.subscribe.downloader_id ? String(form.subscribe.downloader_id) : ""}
                    onChange={(value) => update("subscribe", { downloader_id: value ? Number(value) : null })}
                    options={downloaders.map((downloader) => ({ value: String(downloader.id), label: downloader.name }))}
                    placeholder="选择下载器"
                  />
                </label>
                <label className="flex flex-col gap-1.5 text-xs font-medium text-muted sm:col-span-2">
                  搜索站点（不选则使用全部站点）
                  <Select
                    multiple
                    searchable
                    aria-label="自动订阅搜索的站点"
                    value={form.subscribe.site_ids.map(String)}
                    onChange={(value) => update("subscribe", { site_ids: value.map(Number) })}
                    options={sites.map((site) => ({ value: String(site.id), label: site.name }))}
                    placeholder="全部站点"
                    searchPlaceholder="按名称筛选站点"
                  />
                </label>
                <label className="flex flex-col gap-1.5 text-xs font-medium text-muted">
                  电影保存路径（可选）
                  <Input value={form.subscribe.movie_save_path ?? ""} placeholder="使用下载器默认路径" onChange={(event) => update("subscribe", { movie_save_path: event.target.value || null })} />
                </label>
                <label className="flex flex-col gap-1.5 text-xs font-medium text-muted">
                  剧集保存路径（可选）
                  <Input value={form.subscribe.tv_save_path ?? ""} placeholder="使用下载器默认路径" onChange={(event) => update("subscribe", { tv_save_path: event.target.value || null })} />
                </label>
                {missingDefaults ? <p role="alert" className="text-xs text-destructive sm:col-span-2">开启自动订阅需要先选择质量配置和下载器。</p> : null}
              </div>
            ) : null}
          </Section>

          <Section
            title="Netflix 榜单"
            description="Netflix 官方 Tudum Top 10，每周二更新。全球榜带周观看次数、观看时长、片长与在榜周数，地区榜只有名次与在榜周数。条目按 TMDB 匹配中文名，并补充豆瓣评分。"
            enabled={form.netflix.enabled}
            onToggle={(value) => update("netflix", { enabled: value })}
          >
            <ChipGroup label="榜单地区" options={options.netflix_regions} value={form.netflix.regions} onChange={(value) => update("netflix", { regions: value })} />
          </Section>

          <Section
            title="红果短剧榜单"
            description="红果短剧官网热播榜，按每页 20 条向后翻页。短剧不在 TMDB / 豆瓣收录，只供浏览，可跳转官网观看或试看前 3 集。"
            enabled={form.hongguo.enabled}
            onToggle={(value) => update("hongguo", { enabled: value })}
          >
            <ChipGroup label="抓取榜单" options={options.hongguo_boards} value={form.hongguo.boards} onChange={(value) => update("hongguo", { boards: value })} />
            <ChipGroup
              label="每榜条数"
              options={options.hongguo_limits.map((limit) => ({ value: String(limit), label: `${limit} 条` }))}
              value={[String(form.hongguo.limit)]}
              onChange={(value) => update("hongguo", { limit: Number(value.find((item) => item !== String(form.hongguo.limit)) ?? form.hongguo.limit) })}
            />
          </Section>

          <Section
            title="音乐榜单"
            description="网易云音乐、QQ 音乐的站内榜单与 Apple Music 官方 Feed，无需账号。音乐条目只供浏览。"
            enabled={form.music.enabled}
            onToggle={(value) => update("music", { enabled: value })}
          >
            {musicGroups.map(([group, items]) => (
              <ChipGroup key={group} label={group} options={items} value={form.music.boards.filter((board) => items.some((item) => item.value === board))} onChange={(value) => update("music", { boards: [...form.music.boards.filter((board) => !items.some((item) => item.value === board)), ...value] })} />
            ))}
            <ChipGroup
              label="每榜条数（Apple Music 最多 100 条）"
              options={options.music_limits.map((limit) => ({ value: String(limit), label: `${limit} 首` }))}
              value={[String(form.music.limit)]}
              onChange={(value) => update("music", { limit: Number(value.find((item) => item !== String(form.music.limit)) ?? form.music.limit) })}
            />
          </Section>
        </div>
      )}
    </Dialog>
  );
}

function formatTime(value: string) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false });
}

export function RankHistoryDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [records, setRecords] = useState<RankSubscribeRecord[] | null>(null);
  const [error, setError] = useState("");
  const [confirming, setConfirming] = useState(false);
  const [clearing, setClearing] = useState(false);

  useEffect(() => {
    if (!open) return;
    let active = true;
    setRecords(null);
    setError("");
    setConfirming(false);
    api<{ records: RankSubscribeRecord[] }>("/api/media/global-rank/history")
      .then((result) => { if (active) setRecords(result.records); })
      .catch((loadError) => { if (active) setError(describe(loadError)); });
    return () => { active = false; };
  }, [open]);

  const clear = async () => {
    setClearing(true);
    try {
      await api("/api/media/global-rank/history", { method: "DELETE" });
      setRecords([]);
      setConfirming(false);
    } catch (clearError) {
      setError(describe(clearError));
    } finally {
      setClearing(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="自动订阅记录"
      description="猫眼榜单自动加入的订阅。每部只会自动订阅一次，之后删除订阅也不会被再次加入。"
      panelClassName="max-w-3xl"
      footer={
        <div className="flex flex-wrap items-center justify-end gap-2">
          {records && records.length > 0 ? (
            confirming ? (
              <>
                <span className="mr-auto text-xs text-muted">清空后，仍在榜上的条目可能在下次刷新时被再次订阅。</span>
                <Button variant="outline" onClick={() => setConfirming(false)}>取消</Button>
                <Button variant="destructive" disabled={clearing} onClick={() => void clear()}>确认清空</Button>
              </>
            ) : (
              <Button variant="outline" className="mr-auto" onClick={() => setConfirming(true)}><Trash2 data-icon="inline-start" />清空记录</Button>
            )
          ) : null}
          {!confirming ? <Button variant="outline" onClick={onClose}>关闭</Button> : null}
        </div>
      }
    >
      <div className="p-4 sm:p-6">
        {error ? (
          <p role="alert" className="text-sm text-destructive">{error}</p>
        ) : records === null ? (
          <p role="status" className="flex items-center gap-2 text-sm text-muted"><LoaderCircle className="size-4 animate-spin" aria-hidden="true" />正在读取</p>
        ) : records.length === 0 ? (
          <p className="py-6 text-center text-sm text-muted">暂无记录。在「榜单设置」里为猫眼榜单设置自动订阅条数后，新上榜的条目会记在这里。</p>
        ) : (
          <div className="overflow-x-auto rounded-xl border border-border">
            <table className="w-full min-w-[560px] text-sm">
              <thead className="bg-surface-container text-left text-xs text-muted">
                <tr>
                  <th scope="col" className="px-3 py-2 font-medium">标题</th>
                  <th scope="col" className="px-3 py-2 font-medium">来自榜单</th>
                  <th scope="col" className="px-3 py-2 font-medium">平台 · 上线</th>
                  <th scope="col" className="px-3 py-2 font-medium">时间</th>
                </tr>
              </thead>
              <tbody>
                {records.map((record) => (
                  <tr key={record.unique} className="border-t border-border">
                    <td className="px-3 py-2">
                      <a className="font-medium underline-offset-4 hover:underline" href={`https://www.themoviedb.org/${record.media_type}/${record.tmdb_id}`} target="_blank" rel="noreferrer">
                        {record.tmdb_title}{record.year ? ` (${record.year})` : ""}<ExternalLink className="ml-1 inline size-3 text-muted" aria-hidden="true" />
                      </a>
                      <span className="block text-xs text-muted">
                        {record.media_type === "tv" ? `剧集${record.season ? ` · 第 ${record.season} 季` : ""}` : "电影"}
                        {record.title !== record.tmdb_title ? ` · 榜单名「${record.title}」` : ""}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-xs">{record.board}</td>
                    <td className="px-3 py-2 text-xs text-muted">{[record.platform, record.release_info].filter(Boolean).join(" · ") || "—"}</td>
                    <td className="whitespace-nowrap px-3 py-2 text-xs tabular-nums text-muted">{formatTime(record.time)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </Dialog>
  );
}
