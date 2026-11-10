import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  ExternalLink,
  History,
  ImageOff,
  LoaderCircle,
  Music2,
  Play,
  Plus,
  RefreshCw,
  Search,
  Settings2,
  Star,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { api } from "@/lib/api";
import { oneOf, usePageState } from "@/lib/page-state";
import { cn } from "@/lib/utils";
import { RankHistoryDialog, RankSettingsDialog } from "./global-rank-settings";
import type {
  RankBoard,
  RankDetailTarget,
  RankItem,
  RankResourceOption,
  RankSourceKey,
  RankSourceSummary,
  RankSourceView,
  RankTmdbMedia,
} from "./global-rank-types";

export type { RankDetailTarget, RankTmdbMedia } from "./global-rank-types";

const POLL_MS = 3000;
const SOURCE_KEYS: RankSourceKey[] = ["maoyan", "netflix", "hongguo", "music"];

const SOURCE_COPY: Record<RankSourceKey, { short: string; firstLoad: string; note: string }> = {
  maoyan: {
    short: "猫眼",
    firstLoad: "首次获取猫眼榜单，匹配豆瓣与 TMDB 需要一点时间",
    note: "实时票房、年度与影史票房、网络电影与剧集综艺热度。条目已匹配豆瓣评分和 TMDB，可直接订阅。",
  },
  netflix: {
    short: "Netflix",
    firstLoad: "首次获取 Netflix 榜单，地区榜数据较大，可能需要一分钟",
    note: "Netflix 官方每周 Top 10，每周二更新。已按 TMDB 匹配中文片名，豆瓣评分在后台逐步补全。",
  },
  hongguo: {
    short: "红果短剧",
    firstLoad: "正在获取红果短剧榜单",
    note: "红果短剧官网热播榜。短剧不在 TMDB / 豆瓣收录，只供浏览，可跳转官网观看或试看。",
  },
  music: {
    short: "音乐",
    firstLoad: "正在获取音乐榜单",
    note: "网易云音乐、QQ 音乐与 Apple Music 的榜单，只供浏览。",
  },
};

// ---------------------------------------------------------------------------
// 格式化
// ---------------------------------------------------------------------------

function formatTime(value: string | null | undefined) {
  if (!value) return "";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleString("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false });
}

function trimZeros(text: string) {
  return text.includes(".") ? text.replace(/\.?0+$/, "") : text;
}

/** 观看次数 / 时长：≥1 亿显示「x.xx 亿」，其余显示「N 万」。 */
function formatLarge(value: number) {
  if (value >= 1e8) return `${trimZeros((value / 1e8).toFixed(2))} 亿`;
  if (value >= 1e4) return `${trimZeros((value / 1e4).toFixed(1))} 万`;
  return value.toLocaleString("zh-CN");
}

function formatRuntime(minutes: number) {
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  if (hours === 0) return `${rest} 分钟`;
  return rest === 0 ? `${hours} 小时` : `${hours} 小时 ${rest} 分`;
}

function formatDuration(seconds: number) {
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

function describe(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function periodLabel(board: RankBoard) {
  const period = board.period;
  if (!period) return null;
  switch (board.source) {
    case "netflix":
      return `统计周期 ${period} 起一周`;
    case "maoyan":
      if (board.board_type === "total") return "中国内地影史累计";
      if (board.board_type === "year") return `${period.slice(0, 4)} 年国内票房`;
      if (board.board_type === "global") return `${period.slice(0, 4)} 年全球票房`;
      if (board.board_type === "web_movie") return `${period.slice(0, 7)} 月分账`;
      return `数据日期 ${period}`;
    case "music":
      return `榜单日期 ${period}`;
    default:
      return period;
  }
}

// ---------------------------------------------------------------------------
// 封面
// ---------------------------------------------------------------------------

// 豆瓣图片有防盗链，由后端代取；同一张海报在会话内只请求一次。
const doubanPosterCache = new Map<string, Promise<string | null>>();
export function loadDoubanPoster(id: string) {
  let pending = doubanPosterCache.get(id);
  if (!pending) {
    pending = api<{ data_url: string }>(`/api/media/douban/poster?id=${encodeURIComponent(id)}`)
      .then((payload) => payload.data_url)
      .catch(() => {
        doubanPosterCache.delete(id);
        return null;
      });
    doubanPosterCache.set(id, pending);
  }
  return pending;
}

// 图床在部分网络下直连失败（尤其桌面端与手机），失败后由后端代取一次。
const proxiedPosterCache = new Map<string, Promise<string | null>>();
function loadProxiedPoster(url: string) {
  let pending = proxiedPosterCache.get(url);
  if (!pending) {
    pending = api<{ data_url: string }>(`/api/media/global-rank/poster?u=${encodeURIComponent(url)}`)
      .then((payload) => payload.data_url)
      .catch(() => null);
    proxiedPosterCache.set(url, pending);
  }
  return pending;
}

function tmdbImage(path: string) {
  return `https://image.tmdb.org/t/p/w342${path.startsWith("/") ? path : `/${path}`}`;
}

function RankCover({ item, square = false, className }: { item: RankItem; square?: boolean; className?: string }) {
  const tmdbPoster = item.tmdb?.poster_path ?? null;
  const direct = tmdbPoster ? tmdbImage(tmdbPoster) : item.poster ? (/^https?:\/\//i.test(item.poster) ? item.poster : tmdbImage(item.poster)) : null;
  const doubanId = direct ? null : item.douban?.poster_id ?? null;
  const [src, setSrc] = useState<string | null>(direct);
  const [failed, setFailed] = useState(false);
  const triedProxy = useRef(false);

  useEffect(() => {
    setSrc(direct);
    setFailed(false);
    triedProxy.current = false;
    if (!doubanId) return;
    let active = true;
    void loadDoubanPoster(doubanId).then((url) => {
      if (active) setSrc(url);
    });
    return () => {
      active = false;
    };
  }, [direct, doubanId]);

  const onError = () => {
    if (src && !triedProxy.current && /^https:\/\//i.test(src) && !src.startsWith("https://image.tmdb.org/")) {
      triedProxy.current = true;
      void loadProxiedPoster(src).then((url) => {
        if (url) setSrc(url);
        else setFailed(true);
      });
      return;
    }
    setFailed(true);
  };

  return (
    <div className={cn("overflow-hidden rounded-lg border border-border bg-surface-container", square ? "aspect-square" : "aspect-[2/3]", className)}>
      {src && !failed ? (
        <img src={src} alt={`${item.title} 封面`} referrerPolicy="no-referrer" className="size-full object-cover" loading="lazy" onError={onError} />
      ) : (
        <div className="flex size-full items-center justify-center text-muted">
          {square ? <Music2 className="size-4" aria-label="暂无封面" /> : <ImageOff className="size-5" aria-label="暂无封面" />}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 小组件
// ---------------------------------------------------------------------------

function Chip({ selected, onClick, children, size = "md" }: { selected: boolean; onClick: () => void; children: React.ReactNode; size?: "md" | "sm" }) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={selected}
      className={cn(
        "cursor-pointer whitespace-nowrap border font-semibold transition-colors duration-200 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40",
        size === "md" ? "h-8 rounded-full px-3 text-xs" : "h-7 rounded-lg px-2.5 text-xs",
        selected ? "border-primary/30 bg-primary/10 text-foreground" : "border-border text-muted hover:bg-accent hover:text-foreground",
      )}
      onClick={onClick}
    >
      {children}
    </button>
  );
}

function RankBadge({ rank }: { rank: number }) {
  return (
    <span className={cn("absolute left-1 top-1 rounded-md px-1.5 text-xs font-semibold tabular-nums", rank <= 3 ? "bg-primary text-primary-foreground" : "bg-background/90 text-foreground")}>
      {rank}
    </span>
  );
}

function NewBadge() {
  return <span className="ml-1.5 inline-flex rounded bg-primary/10 px-1 align-middle text-[11px] font-semibold text-primary">新上榜</span>;
}

/** 豆瓣评分：有分显示分数，查过没分显示「未开分 / 未收录」，还没查过不显示。 */
function DoubanRating({ item }: { item: RankItem }) {
  const douban = item.douban;
  if (douban?.rating) {
    return (
      <a
        href={douban.url}
        target="_blank"
        rel="noreferrer"
        className="inline-flex items-center gap-1 rounded-md bg-amber-50 px-1.5 py-0.5 text-xs font-semibold tabular-nums text-amber-800 underline-offset-4 hover:underline"
        title="在豆瓣查看"
      >
        <Star className="size-3 fill-amber-500 text-amber-500" aria-hidden="true" />豆瓣 {douban.rating.toFixed(1)}
      </a>
    );
  }
  if (douban) {
    return <a href={douban.url} target="_blank" rel="noreferrer" className="text-xs text-muted underline-offset-4 hover:underline">豆瓣未开分</a>;
  }
  if (item.douban_checked_at) return <span className="text-xs text-muted">豆瓣未收录</span>;
  return null;
}

type Stat = [label: string, value: string | null | undefined];

function Stats({ stats }: { stats: Stat[] }) {
  const shown = stats.filter((stat): stat is [string, string] => Boolean(stat[1]));
  if (shown.length === 0) return null;
  return (
    <dl className="grid grid-cols-2 gap-x-3 gap-y-0.5 text-xs">
      {shown.map(([label, value]) => (
        <div key={label} className="flex min-w-0 gap-1">
          <dt className="shrink-0 text-muted">{label}</dt>
          <dd className="truncate font-medium tabular-nums">{value}</dd>
        </div>
      ))}
    </dl>
  );
}

function SourceLink({ href, children }: { href: string; children: React.ReactNode }) {
  return (
    <a className="inline-flex items-center gap-1 text-xs text-muted underline-offset-4 hover:text-foreground hover:underline" href={href} target="_blank" rel="noreferrer">
      {children}<ExternalLink className="size-3" aria-hidden="true" />
    </a>
  );
}

function BoardHeader({ board, title }: { board: RankBoard; title?: string }) {
  const meta = [periodLabel(board), `共 ${board.count} 条`, board.updated_at ? `抓取于 ${formatTime(board.updated_at)}` : null].filter(Boolean);
  return (
    <div className="flex flex-wrap items-end justify-between gap-x-4 gap-y-1">
      <div className="min-w-0">
        <h4 className="text-sm font-semibold">
          {title ?? board.name}
          {board.stale ? <span className="ml-2 rounded bg-amber-100 px-1.5 py-0.5 align-middle text-[11px] font-medium text-amber-800">本轮抓取失败，沿用上次数据</span> : null}
        </h4>
        <p className="text-xs text-muted">{meta.join(" · ")}</p>
      </div>
      {board.url ? <SourceLink href={board.url}>数据来源：{board.origin || "官网"}</SourceLink> : <span className="text-xs text-muted">数据来源：{board.origin}</span>}
    </div>
  );
}

function uniqueBy<T>(values: T[], key: (value: T) => string) {
  const seen = new Set<string>();
  return values.filter((value) => {
    const id = key(value);
    if (seen.has(id)) return false;
    seen.add(id);
    return true;
  });
}

// ---------------------------------------------------------------------------
// 影视条目（猫眼 / Netflix）
// ---------------------------------------------------------------------------

type MediaActions = {
  onSubscribe: (media: RankTmdbMedia) => void;
  onSearchTitle: (title: string, type: "tv" | "movie") => void;
  onOpenDetail: (target: RankDetailTarget) => void;
};

function maoyanStats(board: RankBoard, item: RankItem): Stat[] {
  switch (board.board_type) {
    case "movie":
      return [["当日票房", item.box_office], ["累计", item.total_box_office], ["票房占比", item.box_rate], ["排片占比", item.show_rate]];
    case "web_movie":
      return [["当月分账", item.box_office], ["热度", item.heat]];
    case "total":
    case "year":
      return [["总票房", item.total_box_office]];
    case "global":
      return [["全球票房", item.total_box_office], ["北美", item.box_office]];
    default:
      return [["热度", item.heat]];
  }
}

function netflixStats(item: RankItem): Stat[] {
  return [
    ["观看", item.views ? `${formatLarge(item.views)}次` : null],
    ["时长", item.hours_viewed ? `${formatLarge(item.hours_viewed)}小时` : null],
    ["片长", item.runtime_minutes ? formatRuntime(item.runtime_minutes) : null],
    ["在榜", item.weeks_in_top10 ? `${item.weeks_in_top10} 周` : null],
  ];
}

function MediaCard({ board, item, actions }: { board: RankBoard; item: RankItem; actions: MediaActions }) {
  const type = item.media_type ?? (board.kind === "movie" ? "movie" : "tv");
  const isNetflix = board.source === "netflix";
  const original = item.original_title && item.original_title !== item.title ? item.original_title : null;
  const secondary = isNetflix
    ? [original, item.season_title && item.season_title !== item.original_title ? item.season_title : null].filter(Boolean).join(" · ")
    : [item.platform, item.release_info].filter(Boolean).join(" · ");
  const stats = isNetflix ? netflixStats(item) : maoyanStats(board, item);
  const openDetail = () =>
    actions.onOpenDetail({
      title: item.title,
      description: [secondary, ...stats.filter((stat) => stat[1]).map(([label, value]) => `${label} ${value}`)].filter(Boolean).join(" · "),
      mediaType: type,
      douban: item.douban ?? null,
      tmdb: item.tmdb ?? null,
    });

  return (
    <li className="flex gap-3 rounded-xl border border-border bg-card p-3">
      <button type="button" aria-label={`查看「${item.title}」详情`} className="relative w-[72px] shrink-0 cursor-pointer self-start rounded-lg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40" onClick={openDetail}>
        <RankCover item={item} />
        <RankBadge rank={item.rank} />
      </button>
      <div className="flex min-w-0 flex-1 flex-col gap-1">
        <h5 className="line-clamp-2 text-sm font-semibold">
          <button type="button" className="cursor-pointer text-left underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40" onClick={openDetail}>{item.title}</button>
          {item.year ? <span className="ml-1 font-normal text-muted">({item.year})</span> : null}
          {item.new_entry ? <NewBadge /> : null}
        </h5>
        {secondary ? <p className="truncate text-xs text-muted" title={secondary}>{secondary}</p> : null}
        {item.douban || item.douban_checked_at ? <div className="flex flex-wrap items-center gap-2"><DoubanRating item={item} /></div> : null}
        <Stats stats={stats} />
        <div className="mt-auto flex items-end justify-between gap-2 pt-1.5">
          <span className="min-w-0 truncate text-xs text-muted" title={item.tmdb ? `TMDB：${item.tmdb.title}` : undefined}>
            {item.tmdb ? `TMDB：${item.tmdb.title}${item.tmdb.year ? ` (${item.tmdb.year})` : ""}` : "未匹配到 TMDB"}
          </span>
          {item.tmdb ? (
            <Button className="h-8 shrink-0 px-3" onClick={() => actions.onSubscribe(item.tmdb as RankTmdbMedia)}>
              <Plus data-icon="inline-start" />订阅
            </Button>
          ) : (
            <Button variant="outline" className="h-8 shrink-0 px-3" onClick={() => actions.onSearchTitle(item.original_title ?? item.title, type)}>
              <Search data-icon="inline-start" />搜索
            </Button>
          )}
        </div>
      </div>
    </li>
  );
}

function MediaGrid({ board, actions }: { board: RankBoard; actions: MediaActions }) {
  return (
    <ol className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
      {board.items.map((item) => (
        <MediaCard key={`${board.key}:${item.rank}:${item.original_title ?? item.title}`} board={board} item={item} actions={actions} />
      ))}
    </ol>
  );
}

function MaoyanBoards({ boards, actions }: { boards: RankBoard[]; actions: MediaActions }) {
  const charts = useMemo(() => uniqueBy(boards, (board) => board.board_type).map((board) => ({ value: board.board_type, label: board.name.split(" · ")[0] })), [boards]);
  const [chart, setChart] = usePageState("media:global-rank:maoyan:chart", "movie");
  const activeChart = charts.some((item) => item.value === chart) ? chart : charts[0]?.value ?? "";
  const chartBoards = boards.filter((board) => board.board_type === activeChart);
  const [platform, setPlatform] = usePageState("media:global-rank:maoyan:platform", "all");
  const active = chartBoards.find((board) => board.group === platform) ?? chartBoards.find((board) => board.group === "all") ?? chartBoards[0];

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap gap-2" role="tablist" aria-label="榜单">
        {charts.map((item) => <Chip key={item.value} selected={activeChart === item.value} onClick={() => setChart(item.value)}>{item.label}</Chip>)}
      </div>
      {chartBoards.length > 1 ? (
        <div className="flex flex-wrap gap-1.5" role="tablist" aria-label="平台">
          {chartBoards.map((board) => <Chip key={board.key} size="sm" selected={active?.key === board.key} onClick={() => setPlatform(board.group)}>{board.group_name}</Chip>)}
        </div>
      ) : null}
      {active ? (
        <>
          <BoardHeader board={active} />
          <MediaGrid board={active} actions={actions} />
        </>
      ) : null}
    </div>
  );
}

function NetflixBoards({ boards, actions }: { boards: RankBoard[]; actions: MediaActions }) {
  const regions = useMemo(() => uniqueBy(boards, (board) => board.group).map((board) => ({ value: board.group, label: board.group_name })), [boards]);
  const [region, setRegion] = usePageState("media:global-rank:netflix:region", "global");
  const [kind, setKind] = usePageState<"movie" | "tv">("media:global-rank:netflix:kind", "tv", oneOf(["movie", "tv"] as const));
  const activeRegion = regions.some((item) => item.value === region) ? region : regions[0]?.value ?? "global";
  const visible = boards.filter((board) => board.group === activeRegion && board.kind === kind);

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <div className="flex flex-wrap gap-2" role="tablist" aria-label="地区">
          {regions.map((item) => <Chip key={item.value} selected={activeRegion === item.value} onClick={() => setRegion(item.value)}>{item.label}</Chip>)}
        </div>
        <div className="ml-auto flex gap-1.5" role="tablist" aria-label="类型">
          <Chip size="sm" selected={kind === "tv"} onClick={() => setKind("tv")}>剧集</Chip>
          <Chip size="sm" selected={kind === "movie"} onClick={() => setKind("movie")}>电影</Chip>
        </div>
      </div>
      {visible.length === 0 ? <div className="flex min-h-32 items-center justify-center text-sm text-muted">该地区本周没有此类榜单</div> : null}
      {visible.map((board) => (
        <div key={board.key} className="flex flex-col gap-3">
          <BoardHeader board={board} />
          <MediaGrid board={board} actions={actions} />
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 红果短剧
// ---------------------------------------------------------------------------

function HongguoCard({ item }: { item: RankItem }) {
  const episodes = [
    item.episode_count ? `共 ${item.episode_count} 集` : null,
    item.open_episode_count ? `可试看 ${item.open_episode_count} 集` : null,
    item.paid === false ? "免费" : item.paid ? "付费" : null,
  ].filter(Boolean).join(" · ");
  return (
    <li className="flex gap-3 rounded-xl border border-border bg-card p-3">
      <div className="relative w-20 shrink-0 self-start">
        <RankCover item={item} />
        <RankBadge rank={item.rank} />
      </div>
      <div className="flex min-w-0 flex-1 flex-col gap-1.5">
        <h5 className="line-clamp-2 text-sm font-semibold">
          {item.url ? <a className="underline-offset-4 hover:underline" href={item.url} target="_blank" rel="noreferrer">{item.title}</a> : item.title}
          {item.status_tags?.map((tag) => <span key={tag} className="ml-1.5 inline-flex rounded bg-primary/10 px-1 align-middle text-[11px] font-semibold text-primary">{tag}</span>)}
        </h5>
        {item.score ? (
          <p className="inline-flex w-fit items-center gap-1 rounded-md bg-amber-50 px-1.5 py-0.5 text-xs font-semibold text-amber-800">
            <Star className="size-3 fill-amber-500 text-amber-500" aria-hidden="true" />红果{item.score.replace(/^评分/, " ")}
          </p>
        ) : null}
        <Stats stats={[["热度", item.heat?.replace(/热度$/, "")], ["收藏", item.favorite?.replace(/收藏$/, "")], ["点赞", item.likes?.replace(/点赞$/, "")]]} />
        {episodes ? <p className="text-xs text-muted">{episodes}</p> : null}
        {item.tags && item.tags.length > 0 ? (
          <div className="flex flex-wrap gap-1">
            {item.tags.slice(0, 5).map((tag) => <span key={tag} className="rounded-md bg-surface-container px-1.5 py-0.5 text-[11px] text-secondary-foreground">{tag}</span>)}
          </div>
        ) : null}
        {item.overview ? <p className="line-clamp-3 text-xs leading-5 text-muted" title={item.overview}>{item.overview}</p> : null}
        <div className="mt-auto flex flex-wrap items-center gap-1.5 pt-1">
          {item.play_url ? (
            <a className="inline-flex h-8 items-center gap-1 rounded-lg bg-primary px-3 text-xs font-medium text-primary-foreground hover:bg-[#603c8e]" href={item.play_url} target="_blank" rel="noreferrer">
              <Play className="size-3.5" aria-hidden="true" />官网观看
            </a>
          ) : null}
          {item.preview_urls && item.preview_urls.length > 0 ? (
            <span className="ml-1 inline-flex items-center gap-1 text-xs text-muted">
              试看
              {item.preview_urls.map((url, index) => (
                <a key={url} className="inline-flex size-8 items-center justify-center rounded-lg border border-border text-xs text-foreground hover:bg-accent" href={url} target="_blank" rel="noreferrer" aria-label={`试看第 ${index + 1} 集`}>
                  {index + 1}
                </a>
              ))}
            </span>
          ) : null}
        </div>
      </div>
    </li>
  );
}

function HongguoBoards({ boards }: { boards: RankBoard[] }) {
  const [key, setKey] = usePageState("media:global-rank:hongguo:board", "hot");
  const active = boards.find((board) => board.key === key) ?? boards[0];
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap gap-2" role="tablist" aria-label="榜单">
        {boards.map((board) => <Chip key={board.key} selected={active.key === board.key} onClick={() => setKey(board.key)}>{board.name.replace(/^红果/, "")}</Chip>)}
      </div>
      <BoardHeader board={active} />
      <ol className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {active.items.map((item) => <HongguoCard key={`${active.key}:${item.series_id ?? item.rank}:${item.title}`} item={item} />)}
      </ol>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 音乐
// ---------------------------------------------------------------------------

function RankChange({ item }: { item: RankItem }) {
  if (item.new_entry) return <span className="text-[11px] font-semibold text-primary">新</span>;
  if (item.rank_change === undefined) return null;
  const previous = item.last_rank ? `上期第 ${item.last_rank} 名` : undefined;
  if (item.rank_change === 0) return <span className="text-[11px] text-muted" title={previous}>–</span>;
  const up = item.rank_change > 0;
  const Icon = up ? ArrowUp : ArrowDown;
  return (
    <span className={cn("inline-flex items-center text-[11px] tabular-nums", up ? "text-jade" : "text-muted")} title={previous} aria-label={`${up ? "上升" : "下降"} ${Math.abs(item.rank_change)} 位`}>
      <Icon className="size-3" aria-hidden="true" />{Math.abs(item.rank_change)}
    </span>
  );
}

function MusicBoards({ boards }: { boards: RankBoard[] }) {
  const platforms = useMemo(() => uniqueBy(boards, (board) => board.group).map((board) => ({ value: board.group, label: board.group_name })), [boards]);
  const [platform, setPlatform] = usePageState("media:global-rank:music:platform", "netease");
  const [boardKey, setBoardKey] = usePageState("media:global-rank:music:board", "netease:hot");
  const activePlatform = platforms.some((item) => item.value === platform) ? platform : platforms[0]?.value ?? "";
  const platformBoards = boards.filter((board) => board.group === activePlatform);
  const active = platformBoards.find((board) => board.key === boardKey) ?? platformBoards[0];
  const showChange = active?.items.some((item) => item.rank_change !== undefined || item.new_entry) ?? false;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2" role="tablist" aria-label="平台">
        {platforms.map((item) => <Chip key={item.value} selected={activePlatform === item.value} onClick={() => setPlatform(item.value)}>{item.label}</Chip>)}
      </div>
      {platformBoards.length > 1 ? (
        <div className="flex flex-wrap gap-1.5" role="tablist" aria-label="榜单">
          {platformBoards.map((board) => <Chip key={board.key} size="sm" selected={active?.key === board.key} onClick={() => setBoardKey(board.key)}>{board.name}</Chip>)}
        </div>
      ) : null}
      {active ? (
        <>
          <BoardHeader board={active} title={`${active.group_name} · ${active.name}`} />
          <ol className="grid gap-x-6 rounded-xl border border-border bg-card px-2 py-1 lg:grid-cols-2">
            {active.items.map((item) => (
              <li key={`${active.key}:${item.rank}:${item.song_id ?? item.title}`} className="flex min-w-0 items-center gap-3 border-b border-border/60 px-1 py-2 last:border-b-0 lg:[&:nth-last-child(2):nth-child(odd)]:border-b-0">
                <div className="flex w-7 shrink-0 flex-col items-center">
                  <span className={cn("text-sm font-semibold tabular-nums", item.rank <= 3 ? "text-primary" : "text-muted")}>{item.rank}</span>
                  {showChange ? <RankChange item={item} /> : null}
                </div>
                <RankCover item={item} square className="w-11 shrink-0" />
                <div className="min-w-0 flex-1">
                  <p className="truncate text-sm font-medium">
                    {item.url ? <a className="underline-offset-4 hover:underline" href={item.url} target="_blank" rel="noreferrer">{item.title}</a> : item.title}
                  </p>
                  <p className="truncate text-xs text-muted" title={[item.artist, item.album].filter(Boolean).join(" · ")}>
                    {[item.artist, item.album].filter(Boolean).join(" · ") || " "}
                  </p>
                </div>
                {item.genre ? <span className="hidden shrink-0 rounded-md bg-surface-container px-1.5 py-0.5 text-[11px] text-secondary-foreground sm:inline">{item.genre}</span> : null}
                {item.duration ? <span className="w-10 shrink-0 text-right text-xs tabular-nums text-muted">{formatDuration(item.duration)}</span> : null}
              </li>
            ))}
          </ol>
        </>
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 数据源总览
// ---------------------------------------------------------------------------

function SourceCard({ summary, selected, onSelect }: { summary: RankSourceSummary; selected: boolean; onSelect: () => void }) {
  const media = summary.source === "maoyan" || summary.source === "netflix";
  const updated = formatTime(summary.fetched_at);
  return (
    <button
      type="button"
      role="tab"
      aria-selected={selected}
      onClick={onSelect}
      className={cn(
        "flex min-w-0 cursor-pointer flex-col gap-1 rounded-xl border p-3 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40",
        selected ? "border-primary/40 bg-primary/5 shadow-sm" : "border-border bg-card hover:bg-accent",
        !summary.enabled && "opacity-70",
      )}
    >
      <span className="flex items-center gap-2">
        <span className="truncate text-sm font-semibold">{SOURCE_COPY[summary.source].short}</span>
        {summary.refreshing ? (
          <LoaderCircle className="size-3.5 shrink-0 animate-spin text-primary" aria-label="正在刷新" />
        ) : summary.error ? (
          <AlertTriangle className="size-3.5 shrink-0 text-amber-600" aria-label="最近一次刷新有失败" />
        ) : null}
        {!summary.enabled ? <span className="ml-auto shrink-0 rounded bg-surface-container px-1.5 text-[11px] text-muted">未启用</span> : null}
      </span>
      <span className="truncate text-[11px] text-muted">{summary.origin}</span>
      {summary.enabled ? (
        <>
          <span className="text-xs tabular-nums">
            {summary.board_count > 0 ? `${summary.board_count} 个榜 · ${summary.item_count} 条` : summary.refreshing ? "正在抓取" : "暂无数据"}
            {summary.stale_count > 0 ? <span className="text-amber-700"> · {summary.stale_count} 个沿用旧数据</span> : null}
          </span>
          {media && summary.item_count > 0 ? (
            <span className="text-[11px] tabular-nums text-muted">豆瓣评分 {summary.rated_count} · 可订阅 {summary.matched_count}</span>
          ) : summary.source === "hongguo" && summary.item_count > 0 ? (
            <span className="text-[11px] tabular-nums text-muted">站内评分 {summary.rated_count}</span>
          ) : null}
          <span className="text-[11px] text-muted">{updated ? `更新于 ${updated}` : " "}</span>
        </>
      ) : (
        <span className="text-[11px] text-muted">在「榜单设置」中启用</span>
      )}
    </button>
  );
}

function useGlobalRank(source: RankSourceKey) {
  const [overview, setOverview] = useState<RankSourceSummary[] | null>(null);
  const [view, setView] = useState<RankSourceView | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const generation = useRef(0);

  const load = useCallback(async (silent = false) => {
    const current = ++generation.current;
    if (!silent) setLoading(true);
    try {
      const [summary, payload] = await Promise.all([
        api<{ sources: RankSourceSummary[] }>("/api/media/global-rank/overview"),
        api<RankSourceView>(`/api/media/global-rank?source=${source}`),
      ]);
      if (current !== generation.current) return;
      setOverview(summary.sources);
      setView(payload);
      setError("");
    } catch (loadError) {
      if (current === generation.current) setError(describe(loadError));
    } finally {
      if (current === generation.current) setLoading(false);
    }
  }, [source]);

  useEffect(() => {
    setView(null);
    setError("");
    void load();
  }, [load]);

  const busy = Boolean(view?.refreshing || overview?.some((item) => item.refreshing));
  useEffect(() => {
    if (!busy) return;
    const timer = window.setInterval(() => void load(true), POLL_MS);
    return () => window.clearInterval(timer);
  }, [busy, load]);

  const refresh = async (target: RankSourceKey | "all") => {
    try {
      await api(`/api/media/global-rank/refresh?source=${target}`, { method: "POST" });
      await load(true);
    } catch (refreshError) {
      setError(describe(refreshError));
    }
  };

  return { overview, view, error, loading, load, refresh };
}

/** 全球榜单：复刻 MoviePilot 插件 GlobalRankPro 的四个数据源、评分、数据来源与自动订阅。 */
export function GlobalRankBoard({
  profiles,
  downloaders,
  sites,
  onSubscribe,
  onSearchTitle,
  onOpenDetail,
}: {
  profiles: RankResourceOption[];
  downloaders: RankResourceOption[];
  sites: RankResourceOption[];
} & MediaActions) {
  const [source, setSource] = usePageState<RankSourceKey>("media:global-rank:source", "maoyan", oneOf(SOURCE_KEYS));
  const { overview, view, error, loading, load, refresh } = useGlobalRank(source);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);
  const summary = overview?.find((item) => item.source === source);
  const boards = view?.boards ?? [];
  const copy = SOURCE_COPY[source];
  const actions: MediaActions = { onSubscribe, onSearchTitle, onOpenDetail };
  const anyRefreshing = Boolean(overview?.some((item) => item.refreshing));

  return (
    <section aria-label="全球榜单" className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-semibold">全球榜单</h3>
          <p className="text-xs text-muted">猫眼、Netflix、红果短剧与音乐榜，复刻 MoviePilot 插件 GlobalRankPro。</p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button variant="outline" className="h-8 px-3" onClick={() => setHistoryOpen(true)}><History data-icon="inline-start" />订阅记录</Button>
          <Button variant="outline" className="h-8 px-3" onClick={() => setSettingsOpen(true)}><Settings2 data-icon="inline-start" />榜单设置</Button>
          <Button variant="outline" className="h-8 px-3" disabled={anyRefreshing} onClick={() => void refresh("all")}>
            <RefreshCw className={cn(anyRefreshing && "animate-spin")} data-icon="inline-start" />全部刷新
          </Button>
        </div>
      </div>

      <div className="grid grid-cols-2 gap-2 lg:grid-cols-4" role="tablist" aria-label="数据源">
        {(overview ?? SOURCE_KEYS.map((key) => null)).map((item, index) =>
          item ? (
            <SourceCard key={item.source} summary={item} selected={source === item.source} onSelect={() => setSource(item.source)} />
          ) : (
            <div key={SOURCE_KEYS[index]} className="h-[104px] animate-pulse rounded-xl border border-border bg-surface-container/50" />
          ),
        )}
      </div>

      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 rounded-xl border border-border bg-surface-container/35 px-3 py-2">
        <div className="min-w-0 flex-1">
          <p className="text-xs text-muted">{copy.note}</p>
          {view?.message ? <p role="status" className="mt-0.5 text-xs">最近刷新：{view.message}</p> : null}
        </div>
        <div className="flex items-center gap-3">
          {summary ? <SourceLink href={summary.homepage}>{summary.origin}</SourceLink> : null}
          <Button variant="outline" className="h-8 px-3" disabled={!view?.enabled || view.refreshing} onClick={() => void refresh(source)}>
            <RefreshCw className={cn(view?.refreshing && "animate-spin")} data-icon="inline-start" />{view?.refreshing ? "后台刷新中" : "刷新"}
          </Button>
        </div>
      </div>

      {boards.length > 0 && view?.error ? (
        <p role="status" className="flex items-start gap-2 rounded-lg border border-amber-200 bg-amber-50 px-3 py-2 text-xs text-amber-900">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden="true" />
          <span>部分榜单刷新失败，相关榜单显示的是上次的数据：{view.error}</span>
        </p>
      ) : null}

      {view && !view.enabled ? (
        <div className="flex min-h-44 flex-col items-center justify-center gap-3 rounded-2xl border border-border bg-card px-4 text-center text-sm text-muted">
          {copy.short}数据源未启用。
          <Button variant="outline" className="h-8 px-3" onClick={() => setSettingsOpen(true)}>打开榜单设置</Button>
        </div>
      ) : boards.length > 0 ? (
        source === "maoyan" ? (
          <MaoyanBoards boards={boards} actions={actions} />
        ) : source === "netflix" ? (
          <NetflixBoards boards={boards} actions={actions} />
        ) : source === "hongguo" ? (
          <HongguoBoards boards={boards} />
        ) : (
          <MusicBoards boards={boards} />
        )
      ) : loading || view?.refreshing || (view === null && !error) ? (
        <div className="flex min-h-56 items-center justify-center gap-3 rounded-2xl border border-border bg-card px-4 text-center text-sm text-muted">
          <LoaderCircle className="size-5 shrink-0 animate-spin" aria-hidden="true" />
          {view?.refreshing ? copy.firstLoad : "正在读取榜单"}
        </div>
      ) : error || view?.error ? (
        <div role="alert" className="flex flex-col items-start gap-3 rounded-2xl border border-destructive/25 bg-destructive/5 p-4 text-sm">
          <p>{error || view?.error}</p>
          <Button variant="outline" className="h-8 px-3" onClick={() => void (error ? load() : refresh(source))}>重试</Button>
        </div>
      ) : (
        <div className="flex min-h-44 items-center justify-center text-sm text-muted">榜单暂无数据</div>
      )}

      <RankSettingsDialog
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
        onSaved={() => void load(true)}
        profiles={profiles}
        downloaders={downloaders}
        sites={sites}
      />
      <RankHistoryDialog open={historyOpen} onClose={() => setHistoryOpen(false)} />
    </section>
  );
}
