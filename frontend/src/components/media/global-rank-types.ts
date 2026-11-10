/** 全球榜单的接口类型（与后端 `media::global_rank` 一致）。 */

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

export type RankDoubanSubject = {
  id: string;
  title: string;
  year: number | null;
  rating: number | null;
  poster_id: string | null;
  url: string;
  media_type: "tv" | "movie";
};

export type RankSourceKey = "maoyan" | "netflix" | "hongguo" | "music";

/** 统一的榜单条目；与来源无关的字段不会出现。 */
export type RankItem = {
  rank: number;
  title: string;
  original_title?: string;
  media_type?: "tv" | "movie";
  poster?: string;
  url?: string;
  overview?: string;
  year?: number;
  rank_change?: number;
  new_entry?: boolean;
  douban?: RankDoubanSubject;
  douban_checked_at?: string;
  tmdb?: RankTmdbMedia;
  // 猫眼
  release_info?: string;
  platform?: string;
  box_office?: string;
  total_box_office?: string;
  heat?: string;
  box_rate?: string;
  show_rate?: string;
  play_count?: string;
  cast?: string[];
  tmdb_rating?: number;
  // Netflix
  season_title?: string;
  hours_viewed?: number;
  views?: number;
  runtime_minutes?: number;
  weeks_in_top10?: number;
  // 红果短剧
  series_id?: string;
  episode_count?: number;
  open_episode_count?: number;
  paid?: boolean;
  score?: string;
  favorite?: string;
  likes?: string;
  tags?: string[];
  status_tags?: string[];
  preview_urls?: string[];
  play_url?: string;
  // 音乐
  artist?: string;
  album?: string;
  duration?: number;
  last_rank?: number;
  genre?: string;
  song_id?: string;
};

export type RankBoard = {
  key: string;
  source: RankSourceKey;
  name: string;
  board_type: string;
  group: string;
  group_name: string;
  kind: "movie" | "tv" | null;
  period: string | null;
  url: string | null;
  origin: string;
  updated_at: string;
  stale: boolean;
  count: number;
  items: RankItem[];
};

export type RankSourceView = {
  source: RankSourceKey;
  enabled: boolean;
  fetched_at: string | null;
  boards: RankBoard[];
  refreshing: boolean;
  error: string | null;
  message: string | null;
};

export type RankSourceSummary = {
  source: RankSourceKey;
  name: string;
  origin: string;
  homepage: string;
  enabled: boolean;
  board_count: number;
  item_count: number;
  stale_count: number;
  rated_count: number;
  matched_count: number;
  fetched_at: string | null;
  period: string | null;
  refreshing: boolean;
  error: string | null;
  message: string | null;
};

export type MaoyanChartKey = "movie" | "web_movie" | "web_all" | "tv" | "web_series" | "variety" | "total" | "year" | "global";
export type MaoyanPlatformKey = "all" | "tencent" | "iqiyi" | "youku" | "mango";

export type RankSettings = {
  auto_refresh: boolean;
  maoyan: {
    enabled: boolean;
    charts: MaoyanChartKey[];
    platforms: MaoyanPlatformKey[];
    subscribe_movie: number;
    subscribe_tv: number;
    subscribe_variety: number;
    subscribe_tencent: number;
    subscribe_iqiyi: number;
    subscribe_mango: number;
    subscribe_youku: number;
  };
  netflix: { enabled: boolean; regions: string[] };
  hongguo: { enabled: boolean; boards: string[]; limit: number };
  music: { enabled: boolean; boards: string[]; limit: number };
  subscribe: {
    quality_profile_id: number | null;
    downloader_id: number | null;
    site_ids: number[];
    movie_save_path: string | null;
    tv_save_path: string | null;
  };
};

export type RankOption = { value: string; label: string; group?: string };

export type RankSettingsOptions = {
  maoyan_charts: RankOption[];
  maoyan_platforms: RankOption[];
  netflix_regions: RankOption[];
  hongguo_boards: RankOption[];
  hongguo_limits: number[];
  music_boards: RankOption[];
  music_limits: number[];
  max_subscribe_count: number;
};

export type RankSubscribeRecord = {
  unique: string;
  title: string;
  media_type: "tv" | "movie";
  tmdb_id: number;
  tmdb_title: string;
  year: number | null;
  season: number | null;
  poster_path: string | null;
  board: string;
  platform: string | null;
  release_info: string | null;
  subscription_id: number | null;
  time: string;
};

/** 订阅设置里可选的资源（由媒体页传入）。 */
export type RankResourceOption = { id: number; name: string };

/** 打开条目详情（豆瓣 + TMDB）所需的信息。 */
export type RankDetailTarget = {
  title: string;
  description: string;
  mediaType: "tv" | "movie";
  douban: RankDoubanSubject | null;
  tmdb: RankTmdbMedia | null;
  cast?: string[];
  tmdbRating?: number | null;
};
