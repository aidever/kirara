import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowDown, ArrowUp, ExternalLink, ImageOff, LoaderCircle, Music2, Plus, RefreshCw, Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import { api } from "@/lib/api";
import { usePageState } from "@/lib/page-state";
import { cn } from "@/lib/utils";

/** 与媒体页的 TmdbMedia 结构一致，便于直接交给订阅弹窗。 */
export type RankTmdbMedia = {
  tmdb_id: number;
  media_type: "tv" | "movie";
  title: string;
  original_title: string | null;
  year: number | null;
  overview: string;
  poster_path: string | null;
  is_animation: boolean;
  genres: { id: number; name: string }[];
};

export type GlobalRankSourceKey = "netflix" | "hongguo" | "music";

type RankEntry = {
  rank: number;
  title: string;
  original_title: string | null;
  subtitle: string | null;
  poster: string | null;
  url: string | null;
  overview: string | null;
  metrics: string[];
  tags: string[];
  rank_change: number | null;
  new_entry: boolean;
  media_type: "tv" | "movie" | null;
  tmdb: RankTmdbMedia | null;
};

type RankBoard = {
  key: string;
  name: string;
  group: string;
  group_name: string;
  kind: "movie" | "tv" | null;
  period: string | null;
  url: string | null;
  items: RankEntry[];
};

type RankSourceView = {
  source: GlobalRankSourceKey;
  fetched_at: string | null;
  boards: RankBoard[];
  refreshing: boolean;
  error: string | null;
};

const POLL_MS = 3000;

const SOURCE_COPY: Record<GlobalRankSourceKey, { title: string; hint: string; firstLoad: string }> = {
  netflix: {
    title: "Netflix Top 10",
    hint: "Netflix 官方每周榜单，已按 TMDB 匹配中文片名，可直接订阅。",
    firstLoad: "首次获取 Netflix 榜单，地区榜数据较大，可能需要一分钟",
  },
  hongguo: {
    title: "红果短剧",
    hint: "红果短剧官网热播榜。短剧通常不在 TMDB 收录，只供浏览。",
    firstLoad: "正在获取红果短剧榜单",
  },
  music: {
    title: "音乐榜",
    hint: "网易云音乐、QQ 音乐与 Apple Music 的热门榜单，只供浏览。",
    firstLoad: "正在获取音乐榜单",
  },
};

function formatTime(value: string | null | undefined) {
  if (!value) return "";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleString("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false });
}

function describe(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function imageUrl(path: string | null | undefined) {
  if (!path) return null;
  if (/^(https?:\/\/|data:image\/)/i.test(path)) return path;
  return `https://image.tmdb.org/t/p/w342${path.startsWith("/") ? path : `/${path}`}`;
}

function Cover({ src, title, square = false, className }: { src: string | null; title: string; square?: boolean; className?: string }) {
  const [failed, setFailed] = useState(false);
  const url = imageUrl(src);
  useEffect(() => setFailed(false), [url]);
  return (
    <div className={cn("overflow-hidden rounded-lg border border-border bg-surface-container", square ? "aspect-square" : "aspect-[2/3]", className)}>
      {url && !failed ? (
        <img src={url} alt={`${title} 封面`} referrerPolicy="no-referrer" className="size-full object-cover" loading="lazy" onError={() => setFailed(true)} />
      ) : (
        <div className="flex size-full items-center justify-center text-muted">
          {square ? <Music2 className="size-4" aria-label="暂无封面" /> : <ImageOff className="size-5" aria-label="暂无封面" />}
        </div>
      )}
    </div>
  );
}

function Chip({ selected, onClick, children }: { selected: boolean; onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={selected}
      className={cn(
        "h-8 cursor-pointer rounded-full border px-3 text-xs font-semibold transition-colors duration-200 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40",
        selected ? "border-primary/30 bg-primary/10 text-foreground" : "border-border text-muted hover:bg-accent hover:text-foreground",
      )}
      onClick={onClick}
    >
      {children}
    </button>
  );
}

function useRankSource(source: GlobalRankSourceKey) {
  const [data, setData] = useState<RankSourceView | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [requesting, setRequesting] = useState(false);
  const generation = useRef(0);

  const load = useCallback(async (silent = false) => {
    const current = ++generation.current;
    if (!silent) setLoading(true);
    try {
      const payload = await api<RankSourceView>(`/api/media/global-rank?source=${source}`);
      if (current !== generation.current) return;
      setData(payload);
      setError("");
    } catch (loadError) {
      if (current === generation.current) setError(describe(loadError));
    } finally {
      if (current === generation.current) setLoading(false);
    }
  }, [source]);

  useEffect(() => {
    setData(null);
    setError("");
    void load();
  }, [load]);

  const refreshing = Boolean(data?.refreshing);
  useEffect(() => {
    if (!refreshing) return;
    const timer = window.setInterval(() => void load(true), POLL_MS);
    return () => window.clearInterval(timer);
  }, [refreshing, load]);

  const refresh = async () => {
    setRequesting(true);
    try {
      await api<{ refreshing: boolean }>(`/api/media/global-rank/refresh?source=${source}`, { method: "POST" });
      await load(true);
    } catch (refreshError) {
      setError(describe(refreshError));
    } finally {
      setRequesting(false);
    }
  };

  return { data, error, loading, busy: refreshing || requesting, load, refresh };
}

export function GlobalRankSource({
  source,
  onSubscribe,
  onSearchTitle,
}: {
  source: GlobalRankSourceKey;
  onSubscribe: (media: RankTmdbMedia) => void;
  onSearchTitle: (title: string, type: "tv" | "movie") => void;
}) {
  const { data, error, loading, busy, load, refresh } = useRankSource(source);
  const copy = SOURCE_COPY[source];
  const boards = data?.boards ?? [];
  const fetchedAt = formatTime(data?.fetched_at);

  return (
    <section aria-label={copy.title} className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-semibold">{copy.title}</h3>
          <p className="text-xs text-muted">
            {copy.hint}
            {fetchedAt ? ` 更新于 ${fetchedAt}` : ""}
          </p>
        </div>
        <Button variant="outline" className="h-8 px-3" disabled={busy} onClick={() => void refresh()}>
          <RefreshCw className={cn(busy && "animate-spin")} data-icon="inline-start" />{busy ? "后台刷新中" : "刷新"}
        </Button>
      </div>
      {boards.length > 0 && data?.error && !data.refreshing ? (
        <p role="status" className="text-xs text-muted">部分榜单刷新失败，相关榜单显示的是上次的数据：{data.error}</p>
      ) : null}
      {boards.length > 0 ? (
        source === "netflix" ? (
          <NetflixBoards boards={boards} onSubscribe={onSubscribe} onSearchTitle={onSearchTitle} />
        ) : source === "hongguo" ? (
          <HongguoBoards boards={boards} />
        ) : (
          <MusicBoards boards={boards} />
        )
      ) : loading || busy || (data === null && !error) ? (
        <div className="flex min-h-56 items-center justify-center gap-3 rounded-2xl border border-border bg-card px-4 text-center text-sm text-muted">
          <LoaderCircle className="size-5 shrink-0 animate-spin" aria-hidden="true" />
          {busy ? copy.firstLoad : "正在读取榜单"}
        </div>
      ) : error || data?.error ? (
        <div role="alert" className="flex flex-col items-start gap-3 rounded-2xl border border-destructive/25 bg-destructive/5 p-4 text-sm">
          <p>{error || data?.error}</p>
          <Button variant="outline" className="h-8 px-3" onClick={() => void (error ? load() : refresh())}>重试</Button>
        </div>
      ) : (
        <div className="flex min-h-44 items-center justify-center text-sm text-muted">榜单暂无数据</div>
      )}
    </section>
  );
}

// ---------------------------------------------------------------------------
// Netflix
// ---------------------------------------------------------------------------

function NetflixBoards({
  boards,
  onSubscribe,
  onSearchTitle,
}: {
  boards: RankBoard[];
  onSubscribe: (media: RankTmdbMedia) => void;
  onSearchTitle: (title: string, type: "tv" | "movie") => void;
}) {
  const regions = useMemo(() => {
    const seen = new Map<string, string>();
    boards.forEach((board) => {
      if (!seen.has(board.group)) seen.set(board.group, board.group_name);
    });
    return [...seen.entries()].map(([value, label]) => ({ value, label }));
  }, [boards]);
  const [region, setRegion] = usePageState("media:global-rank:netflix:region", "global");
  const [kind, setKind] = usePageState<"movie" | "tv">("media:global-rank:netflix:kind", "tv", (value): value is "movie" | "tv" => value === "movie" || value === "tv");
  const activeRegion = regions.some((item) => item.value === region) ? region : regions[0]?.value ?? "global";
  const visible = boards.filter((board) => board.group === activeRegion && board.kind === kind);
  const period = visible[0]?.period;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <div className="flex flex-wrap gap-2" role="tablist" aria-label="地区">
          {regions.map((item) => (
            <Chip key={item.value} selected={activeRegion === item.value} onClick={() => setRegion(item.value)}>{item.label}</Chip>
          ))}
        </div>
        <div className="ml-auto flex gap-2" role="tablist" aria-label="类型">
          <Chip selected={kind === "tv"} onClick={() => setKind("tv")}>剧集</Chip>
          <Chip selected={kind === "movie"} onClick={() => setKind("movie")}>电影</Chip>
        </div>
      </div>
      {period ? <p className="text-xs text-muted">统计周期：{period} 起一周 · 每周二更新</p> : null}
      {visible.length === 0 ? <div className="flex min-h-32 items-center justify-center text-sm text-muted">该地区暂无此类榜单</div> : null}
      {visible.map((board) => (
        <div key={board.key} className="flex flex-col gap-2">
          {visible.length > 1 ? <h4 className="text-xs font-semibold text-muted">{board.name}</h4> : null}
          <ol className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
            {board.items.map((item) => {
              const type = item.media_type ?? (board.kind === "movie" ? "movie" : "tv");
              return (
                <li key={`${board.key}:${item.rank}:${item.original_title ?? item.title}`} className="flex gap-3 rounded-xl border border-border bg-card p-3">
                  <div className="relative w-16 shrink-0">
                    <Cover src={item.poster} title={item.title} />
                    <span className="absolute left-1 top-1 rounded-md bg-background/90 px-1.5 text-xs font-semibold tabular-nums">{item.rank}</span>
                  </div>
                  <div className="flex min-w-0 flex-1 flex-col">
                    <h5 className="line-clamp-2 text-sm font-semibold" title={item.overview ?? undefined}>
                      {item.title}
                      {item.new_entry ? <span className="ml-1.5 rounded bg-primary/10 px-1 text-[11px] font-semibold text-primary">新上榜</span> : null}
                    </h5>
                    <p className="mt-0.5 truncate text-xs text-muted">
                      {[item.original_title && item.original_title !== item.title ? item.original_title : null, item.subtitle].filter(Boolean).join(" · ") || " "}
                    </p>
                    {item.metrics.length > 0 ? (
                      <p className="mt-0.5 flex flex-wrap gap-x-2 text-xs tabular-nums text-muted">
                        {item.metrics.map((metric) => <span key={metric}>{metric}</span>)}
                      </p>
                    ) : null}
                    <div className="mt-auto flex items-end justify-between gap-2 pt-2">
                      <span className="truncate text-xs text-muted">
                        {item.tmdb ? `TMDB：${item.tmdb.title}${item.tmdb.year ? ` (${item.tmdb.year})` : ""}` : "未匹配到 TMDB"}
                      </span>
                      {item.tmdb ? (
                        <Button className="h-8 shrink-0 px-3" onClick={() => onSubscribe(item.tmdb as RankTmdbMedia)}>
                          <Plus data-icon="inline-start" />订阅
                        </Button>
                      ) : (
                        <Button variant="outline" className="h-8 shrink-0 px-3" onClick={() => onSearchTitle(item.original_title ?? item.title, type)}>
                          <Search data-icon="inline-start" />搜索
                        </Button>
                      )}
                    </div>
                  </div>
                </li>
              );
            })}
          </ol>
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 红果短剧
// ---------------------------------------------------------------------------

function HongguoBoards({ boards }: { boards: RankBoard[] }) {
  const [key, setKey] = usePageState("media:global-rank:hongguo:board", "hot");
  const active = boards.find((board) => board.key === key) ?? boards[0];
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap gap-2" role="tablist" aria-label="榜单">
        {boards.map((board) => (
          <Chip key={board.key} selected={active.key === board.key} onClick={() => setKey(board.key)}>{board.name}</Chip>
        ))}
        {active.url ? (
          <a className="ml-auto inline-flex h-8 items-center gap-1 rounded-full px-3 text-xs text-muted hover:bg-accent hover:text-foreground" href={active.url} target="_blank" rel="noreferrer">
            官网榜单<ExternalLink className="size-3.5" aria-hidden="true" />
          </a>
        ) : null}
      </div>
      <ol className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {active.items.map((item) => (
          <li key={`${active.key}:${item.rank}:${item.title}`} className="flex gap-3 rounded-xl border border-border bg-card p-3">
            <div className="relative w-20 shrink-0">
              <Cover src={item.poster} title={item.title} />
              <span className="absolute left-1 top-1 rounded-md bg-background/90 px-1.5 text-xs font-semibold tabular-nums">{item.rank}</span>
            </div>
            <div className="flex min-w-0 flex-1 flex-col gap-1">
              <h5 className="line-clamp-2 text-sm font-semibold">{item.title}</h5>
              {item.metrics.length > 0 ? (
                <p className="flex flex-wrap gap-x-2 text-xs tabular-nums text-muted">
                  {item.metrics.map((metric) => <span key={metric}>{metric}</span>)}
                </p>
              ) : null}
              {item.tags.length > 0 ? (
                <div className="flex flex-wrap gap-1">
                  {item.tags.slice(0, 5).map((tag) => (
                    <span key={tag} className={cn("rounded-md px-1.5 py-0.5 text-[11px]", tag === "新剧" || tag === "爆剧" ? "bg-primary/10 font-semibold text-primary" : "bg-surface-container text-secondary-foreground")}>{tag}</span>
                  ))}
                </div>
              ) : null}
              {item.overview ? <p className="line-clamp-2 text-xs leading-5 text-muted" title={item.overview}>{item.overview}</p> : null}
              <div className="mt-auto flex items-center justify-between gap-2 pt-1">
                <span className="text-xs text-muted">{item.subtitle ?? ""}</span>
                {item.url ? (
                  <a className="inline-flex h-8 shrink-0 items-center gap-1 rounded-lg border border-border px-3 text-xs hover:bg-accent" href={item.url} target="_blank" rel="noreferrer">
                    官网观看<ExternalLink className="size-3.5" aria-hidden="true" />
                  </a>
                ) : null}
              </div>
            </div>
          </li>
        ))}
      </ol>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 音乐
// ---------------------------------------------------------------------------

function RankChange({ item }: { item: RankEntry }) {
  if (item.new_entry) return <span className="text-[11px] font-semibold text-primary">新</span>;
  if (item.rank_change === null || item.rank_change === 0) return <span className="text-[11px] text-muted">–</span>;
  const up = item.rank_change > 0;
  const Icon = up ? ArrowUp : ArrowDown;
  return (
    <span className={cn("inline-flex items-center text-[11px] tabular-nums", up ? "text-success" : "text-muted")} aria-label={`${up ? "上升" : "下降"} ${Math.abs(item.rank_change)} 位`}>
      <Icon className="size-3" aria-hidden="true" />{Math.abs(item.rank_change)}
    </span>
  );
}

function MusicBoards({ boards }: { boards: RankBoard[] }) {
  const platforms = useMemo(() => {
    const seen = new Map<string, string>();
    boards.forEach((board) => {
      if (!seen.has(board.group)) seen.set(board.group, board.group_name);
    });
    return [...seen.entries()].map(([value, label]) => ({ value, label }));
  }, [boards]);
  const [platform, setPlatform] = usePageState("media:global-rank:music:platform", "netease");
  const [boardKey, setBoardKey] = usePageState("media:global-rank:music:board", "netease:hot");
  const activePlatform = platforms.some((item) => item.value === platform) ? platform : platforms[0]?.value ?? "";
  const platformBoards = boards.filter((board) => board.group === activePlatform);
  const active = platformBoards.find((board) => board.key === boardKey) ?? platformBoards[0];
  const showChange = active?.items.some((item) => item.rank_change !== null || item.new_entry) ?? false;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2" role="tablist" aria-label="平台">
        {platforms.map((item) => (
          <Chip key={item.value} selected={activePlatform === item.value} onClick={() => setPlatform(item.value)}>{item.label}</Chip>
        ))}
      </div>
      {platformBoards.length > 1 ? (
        <div className="flex flex-wrap gap-1.5" role="tablist" aria-label="榜单">
          {platformBoards.map((board) => (
            <button
              key={board.key}
              type="button"
              role="tab"
              aria-selected={active?.key === board.key}
              onClick={() => setBoardKey(board.key)}
              className={cn(
                "h-7 rounded-lg px-2.5 text-xs transition-colors",
                active?.key === board.key ? "bg-accent font-semibold text-foreground" : "text-muted hover:bg-accent hover:text-foreground",
              )}
            >
              {board.name}
            </button>
          ))}
        </div>
      ) : null}
      {active ? (
        <>
          <div className="flex flex-wrap items-center justify-between gap-2 text-xs text-muted">
            <span>{active.group_name} · {active.name}{active.period ? ` · ${active.period}` : ""} · 共 {active.items.length} 首</span>
            {active.url ? (
              <a className="inline-flex items-center gap-1 hover:text-foreground" href={active.url} target="_blank" rel="noreferrer">
                打开榜单<ExternalLink className="size-3.5" aria-hidden="true" />
              </a>
            ) : null}
          </div>
          <ol className="grid gap-x-6 rounded-xl border border-border bg-card px-2 py-1 lg:grid-cols-2">
            {active.items.map((item) => (
              <li key={`${active.key}:${item.rank}:${item.title}`} className="flex min-w-0 items-center gap-3 border-b border-border/60 px-1 py-2 last:border-b-0 lg:[&:nth-last-child(2):nth-child(odd)]:border-b-0">
                <div className="flex w-7 shrink-0 flex-col items-center">
                  <span className={cn("text-sm font-semibold tabular-nums", item.rank <= 3 ? "text-primary" : "text-muted")}>{item.rank}</span>
                  {showChange ? <RankChange item={item} /> : null}
                </div>
                <Cover src={item.poster} title={item.title} square className="w-11 shrink-0" />
                <div className="min-w-0 flex-1">
                  <p className="truncate text-sm font-medium">
                    {item.url ? <a className="hover:underline" href={item.url} target="_blank" rel="noreferrer">{item.title}</a> : item.title}
                  </p>
                  <p className="truncate text-xs text-muted">
                    {[item.subtitle, ...item.metrics.slice(0, 1), ...item.tags.slice(0, 1)].filter(Boolean).join(" · ")}
                  </p>
                </div>
                {item.metrics[1] ? <span className="shrink-0 text-xs tabular-nums text-muted">{item.metrics[1]}</span> : null}
              </li>
            ))}
          </ol>
        </>
      ) : null}
    </div>
  );
}
