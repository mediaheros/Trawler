// Typed wrappers over Tauri invoke, with a browser-mock fallback so the UI
// can be developed and demoed in a plain browser (vite dev) without the shell.

import { invoke as tauriInvoke, isTauri } from "@tauri-apps/api/core";
import { listen as tauriListen } from "@tauri-apps/api/event";

export interface Config {
  prowlarrUrl: string;
  prowlarrApiKey: string;
  qbitUrl: string;
  qbitUsername: string;
  qbitPassword: string;
  qbitCategory: string;
  seedPolicy: "default" | "none" | "ratio";
  seedRatio: number;
  savePathMovies: string;
  savePathTv: string;
  addPaused: boolean;
  defaultQuality: QualityProfile;
  qualityPresets: QualityPreset[];
  schedulerMinutes: number;
  notifyOnGrab: boolean;
  discordWebhook: string;
  telegramBotToken: string;
  telegramChatId: string;
  notifyGrabs: boolean;
  notifyCompletions: boolean;
  notifyProposals: boolean;
  notifyErrors: boolean;
  closeToTray: boolean;
  agentBaseUrl: string;
  agentModel: string;
  agentEnabled: boolean;
  agentMinFreeDiskGb: number;
  medicMode: "off" | "propose" | "auto";
  setupCompleted: boolean;
  rssEnabled: boolean;
  rssMinutes: number;
  upgradeScoutEnabled: boolean;
  upgradeWindowDays: number;
}

export interface CalendarItem {
  showId: number;
  showName: string;
  posterUrl: string | null;
  season: number;
  number: number;
  title: string | null;
  airstamp: string;
  /** local calendar day (YYYY-MM-DD) computed backend-side */
  localDate: string;
  state: EpisodeState;
}

export interface DiscoverEntry {
  showId: number;
  name: string;
  poster: string | null;
  network: string | null;
  status: string;
  weight: number;
  airstamp: string | null;
  season: number | null;
  number: number | null;
  episodeTitle: string | null;
  followed: boolean;
}

export interface DiscoverRows {
  tonight: DiscoverEntry[];
  premieres: DiscoverEntry[];
  popular: DiscoverEntry[];
}

export interface ShowPreview {
  tvmazeId: number;
  name: string;
  summary: string | null;
  genres: string[];
  status: string;
  premiered: string | null;
  ended: string | null;
  network: string | null;
  runtime: number | null;
  rating: number | null;
  poster: string | null;
  imdb: string | null;
  scheduleDays: string | null;
  scheduleTime: string | null;
  episodeCount: number;
  airedEpisodeCount: number;
  seasonCount: number;
  cast: Array<{ person: string; character: string | null; image: string | null }>;
  followed: boolean;
}

export interface SetupStatus {
  qbit: "ok" | "running_no_webui" | "installed_stopped" | "missing";
  prowlarr: "ok" | "needs_key" | "managed_stopped" | "missing";
  agent: "ok" | "unreachable";
  prowlarrHasIndexers: boolean;
}

export interface SetupStep {
  component: string;
  kind: "progress" | "log" | "done" | "error";
  payload: { pct?: number; message?: string };
}

export interface IndexerDef {
  name: string;
  description: string;
  privacy: string;
  language: string;
}

export interface PlannedGrab {
  showId: number;
  showName: string;
  season: number;
  episodes: number[];
  epIds: number[];
  title: string;
  indexer: string | null;
  size: number;
  seeders: number | null;
  isPack: boolean;
  magnetUrl: string | null;
  downloadUrl: string | null;
}

export interface ParsedRelease {
  resolution: string | null;
  source: string | null;
  codec: string | null;
  audio: string | null;
  hdr: string | null;
  group: string | null;
  season: number | null;
  episode: number | null;
  year: number | null;
  proper: boolean;
  seasonPack: boolean;
  cleanTitle: string;
}

export interface Release {
  guid: string | null;
  title: string;
  size: number;
  indexer: string | null;
  indexerId: number;
  infoUrl: string | null;
  downloadUrl: string | null;
  magnetUrl: string | null;
  infoHash: string | null;
  seeders: number | null;
  leechers: number | null;
  protocol: string | null;
  publishDate: string | null;
  age: number;
  grabs: number | null;
  parsed: ParsedRelease;
  score: number;
  dupeCount: number;
  alsoOn: string[];
  kind: "movie" | "tv" | "other";
  relevant: boolean;
}

export interface IndexerOutcome {
  id: number;
  name: string;
  count: number;
  ok: boolean;
  timedOut: boolean;
  elapsedMs: number;
}

export interface SearchResponse {
  releases: Release[];
  totalBeforeDedupe: number;
  indexers: IndexerOutcome[];
}

export interface ConnectionStatus {
  prowlarrOk: boolean;
  prowlarrDetail: string;
  qbitOk: boolean;
  qbitDetail: string;
}

export interface Indexer {
  id: number;
  name: string;
  enable: boolean;
  protocol: string | null;
  privacy: string | null;
}

export interface QbitTorrent {
  hash: string;
  name: string;
  size: number;
  progress: number;
  dlspeed: number;
  upspeed: number;
  eta: number;
  state: string;
  category: string;
  save_path: string;
  added_on: number;
  num_seeds: number;
  num_leechs: number;
  ratio: number;
  content_path: string;
}

export interface DownloadsView {
  torrents: QbitTorrent[];
  transfer: { dl_info_speed: number; up_info_speed: number } | null;
}

export interface GrabResult {
  ok: boolean;
  detail: string;
}

// ---------- followed shows ----------

export interface QualityProfile {
  resolutions: string[];
  codec: "prefer-x265" | "prefer-x264" | "any";
  maxSizeGb: number;
  allowSeasonPacks: boolean;
}

export interface QualityPreset {
  name: string;
  profile: QualityProfile;
}

export interface ImportMatch {
  id: number;
  name: string;
  premiered: string | null;
  network: string | null;
  status: string;
  poster: string | null;
  followed: boolean;
}

export interface ImportRow {
  idx: number;
  input: string;
  matches: ImportMatch[];
  chosen: number | null;
  confidence: "exact" | "good" | "ambiguous" | "none";
  lookupFailed: boolean;
}

export interface ImportLookup {
  rows: ImportRow[];
  totalFound: number;
}

export interface ImportEstimate {
  shows: number;
  airedEpisodes: number;
  estGb: number;
  failed: number;
}

export interface NotifyTestOutcome {
  discord: string | null;
  discordOk: boolean;
  telegram: string | null;
  telegramOk: boolean;
}

export interface TvmazeResult {
  id: number;
  name: string;
  status: string;
  premiered: string | null;
  ended: string | null;
  network: string | null;
  poster: string | null;
  genres: string[];
  summary: string | null;
  followed: boolean;
}

export type EpisodeState = "upcoming" | "wanted" | "grabbed" | "downloaded" | "ignored";

export interface ShowRow {
  tvmazeId: number;
  name: string;
  status: string;
  posterUrl: string | null;
  premiered: string | null;
  ended: string | null;
  network: string | null;
  imdbId: string | null;
  followedAt: number;
  refreshedAt: number;
  qualityJson: string | null;
  savePathOverride: string | null;
  backfill: boolean;
  total: number;
  downloaded: number;
  grabbed: number;
  wanted: number;
  nextAirstamp: string | null;
}

export interface EpisodeRow {
  tvmazeEpId: number;
  showId: number;
  season: number;
  number: number;
  title: string | null;
  airstamp: string | null;
  state: EpisodeState;
  grabbedTitle: string | null;
}

export interface ActivityRow {
  id: number;
  ts: number;
  kind: string;
  showId: number | null;
  message: string;
}

/** Running inside the actual Tauri shell (vs a plain browser tab). */
export const inTauri = isTauri();

/** Demo data is ONLY for `vite dev` in a browser. A production page outside
 * the shell gets hard errors, never silently-fake results. */
const mockAllowed = !inTauri && import.meta.env.DEV;

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (inTauri) return tauriInvoke<T>(cmd, args);
  if (mockAllowed) return mock(cmd, args) as Promise<T>;
  throw new Error("Trawler's backend isn't available in a plain browser — use the desktop app");
}

/** Subscribe to agent-step events (real Tauri events, or the mock emitter in dev). */
export function onAgentStep(cb: (s: AgentStep) => void): () => void {
  if (inTauri) {
    const un = tauriListen<AgentStep>("agent-step", (e) => cb(e.payload));
    return () => {
      void un.then((f) => f());
    };
  }
  mockStepListeners.add(cb);
  return () => mockStepListeners.delete(cb);
}

export function onSetupStep(cb: (s: SetupStep) => void): () => void {
  if (inTauri) {
    const un = tauriListen<SetupStep>("setup-step", (e) => cb(e.payload));
    return () => {
      void un.then((f) => f());
    };
  }
  mockSetupListeners.add(cb);
  return () => mockSetupListeners.delete(cb);
}

const mockSetupListeners = new Set<(s: SetupStep) => void>();
function emitMockSetup(s: SetupStep) {
  for (const cb of mockSetupListeners) cb(s);
}

const mockStepListeners = new Set<(s: AgentStep) => void>();
function emitMockStep(s: AgentStep) {
  for (const cb of mockStepListeners) cb(s);
}

export const api = {
  getConfig: () => call<Config>("get_config"),
  setConfig: (config: Config) => call<Config>("set_config", { config }),
  testConnections: () => call<ConnectionStatus>("test_connections"),
  listIndexers: () => call<Indexer[]>("list_indexers"),
  search: (query: string, kind: string, indexerIds: number[] = []) =>
    call<SearchResponse>("search", { query, kind, indexerIds }),
  grab: (r: Release) =>
    call<GrabResult>("grab", {
      title: r.title,
      kind: r.kind,
      magnetUrl: r.magnetUrl,
      downloadUrl: r.downloadUrl,
    }),
  downloads: (all = false) => call<DownloadsView>("downloads", { all }),
  torrentAction: (action: string, hash: string) =>
    call<void>("torrent_action", { action, hash }),
  searchTvmaze: (query: string) => call<TvmazeResult[]>("search_tvmaze", { query }),
  followShow: (tvmazeId: number, backfill: boolean, seasons: number[] | null) =>
    call<ShowRow>("follow_show", { tvmazeId, backfill, seasons }),
  unfollowShow: (tvmazeId: number) => call<void>("unfollow_show", { tvmazeId }),
  getShows: () => call<ShowRow[]>("get_shows"),
  getShowEpisodes: (tvmazeId: number) => call<EpisodeRow[]>("get_show_episodes", { tvmazeId }),
  refreshShow: (tvmazeId: number) => call<void>("refresh_show", { tvmazeId }),
  setEpisodeState: (tvmazeEpId: number, newState: EpisodeState, grabbedTitle?: string) =>
    call<void>("set_episode_state", { tvmazeEpId, newState, grabbedTitle: grabbedTitle ?? null }),
  setShowOptions: (tvmazeId: number, qualityJson: string | null, savePathOverride: string | null) =>
    call<void>("set_show_options", { tvmazeId, qualityJson, savePathOverride }),
  notifyTest: () => call<NotifyTestOutcome>("notify_test"),
  upgradeScanNow: () => call<void>("upgrade_scan_now"),
  importLookup: (input: string) => call<ImportLookup>("import_lookup", { input }),
  importEstimate: (ids: number[]) => call<ImportEstimate>("import_estimate", { ids }),
  importDisambiguate: (
    rows: Array<{
      input: string;
      candidates: Array<{ id: number; name: string; year: string | null; network: string | null; status: string }>;
    }>,
  ) => call<Array<number | null>>("import_disambiguate", { rows }),
  getActivity: () => call<ActivityRow[]>("get_activity"),
  previewShowGrabs: (tvmazeId: number) => call<PlannedGrab[]>("preview_show_grabs", { tvmazeId }),
  runSchedulerNow: () => call<void>("run_scheduler_now"),
  indexerDefs: () => call<IndexerDef[]>("indexer_defs"),
  addIndexer: (name: string) => call<string>("add_indexer", { name }),
  toggleIndexer: (id: number, enable: boolean) => call<void>("toggle_indexer", { id, enable }),
  removeIndexer: (id: number) => call<void>("remove_indexer", { id }),
  getAutostart: () => call<boolean>("get_autostart"),
  setAutostart: (enabled: boolean) => call<void>("set_autostart", { enabled }),

  // agent
  agentSend: (text: string) => call<string>("agent_send", { text }),
  agentHistory: () => call<ChatRow[]>("agent_history"),
  agentClear: () => call<void>("agent_clear"),
  agentModels: () => call<string[]>("agent_models"),
  briefsList: () => call<BriefRow[]>("briefs_list"),
  compileBrief: (prompt: string) => call<HuntPlan>("compile_brief", { prompt }),
  briefSave: (b: BriefSaveInput) => call<number>("brief_save", { ...b }),
  briefDelete: (id: number) => call<void>("brief_delete", { id }),
  briefRunNow: (id: number) => call<void>("brief_run_now", { id }),
  proposalsList: () => call<ProposalRow[]>("proposals_list"),
  proposalResolve: (id: number, approve: boolean) =>
    call<string>("proposal_resolve", { id, approve }),
  setupStatus: () => call<SetupStatus>("setup_status"),
  setupInstallProwlarr: () => call<string>("setup_install_prowlarr"),
  setupStartProwlarr: () => call<void>("setup_start_prowlarr"),
  setupInstallQbit: () => call<void>("setup_install_qbit"),
  setupConfigureQbit: () => call<void>("setup_configure_qbit"),
  setupStarterIndexers: () => call<string[]>("setup_starter_indexers"),
  setupSaveProwlarrKey: (key: string) => call<void>("setup_save_prowlarr_key", { key }),
  setupFinish: () => call<void>("setup_finish"),
  calendarRange: (start: string, end: string) =>
    call<CalendarItem[]>("calendar_range", { start, end }),
  exportIcal: () => call<string>("export_ical"),
  discover: (force = false) => call<DiscoverRows>("discover", { force }),
  showPreview: (tvmazeId: number) => call<ShowPreview>("show_preview", { tvmazeId }),
  rssSweepNow: () =>
    call<{ releasesSeen: number; episodeGrabs: number; briefGrabs: number; briefProposals: number }>("rss_sweep_now"),
};

// ---------- agent types ----------

export interface HuntPlan {
  queries: string[];
  include: string[];
  exclude: string[];
  resolutions: string[];
  maxSizeGb: number;
  minSeeders: number;
  notes: string;
}

export interface BriefRow {
  id: number;
  name: string;
  prompt: string;
  planJson: string;
  cadenceMinutes: number;
  mode: "propose" | "auto";
  maxGrabsPerRun: number;
  maxGbPerRun: number;
  maxGbPerDay: number;
  enabled: boolean;
  createdAt: number;
  lastRunAt: number;
  lastReport: string | null;
  failStreak: number;
  pausedReason: string | null;
}

export interface BriefSaveInput {
  id: number | null;
  name: string;
  prompt: string;
  plan: HuntPlan;
  cadenceMinutes: number;
  mode: "propose" | "auto";
  maxGrabsPerRun: number;
  maxGbPerRun: number;
  maxGbPerDay: number;
  enabled: boolean;
}

export interface ChatRow {
  id: number;
  ts: number;
  role: "user" | "assistant" | "tool";
  content: string | null;
  toolName: string | null;
  toolPayload: string | null;
}

export interface ProposalRow {
  id: number;
  briefId: number | null;
  briefName: string | null;
  contentKey: string;
  result: {
    title: string;
    size: number;
    seeders: number | null;
    indexer: string | null;
    resolution: string | null;
    source: string | null;
    codec: string | null;
  };
  reason: string | null;
  status: string;
  firstSeen: number;
  lastSeen: number;
}

/** agent-step event payloads (Tauri event 'agent-step') */
export interface AgentStep {
  runId: string;
  kind: "thinking" | "tool_call" | "tool_result" | "text" | "done" | "error";
  payload: {
    tool?: string;
    args?: Record<string, unknown>;
    summary?: string;
    detail?: unknown;
    text?: string;
    message?: string;
  };
}

// ------------------------- browser mock -------------------------

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

let n = 0;
const mockReleases: Release[] = [
  mk("The.Expanse.S02E05.1080p.WEB-DL.DDP5.1.H.264-NTb", 2.4e9, 412, 31, "TorrentLeech", "tv", { resolution: "1080p", source: "WEB-DL", codec: "x264", audio: "DDP", season: 2, episode: 5, cleanTitle: "The Expanse", group: "NTb" }, 2, ["IPTorrents"]),
  mk("The.Expanse.S02E05.2160p.AMZN.WEB-DL.DDP5.1.HDR.HEVC-CasStudio", 6.1e9, 158, 12, "IPTorrents", "tv", { resolution: "2160p", source: "WEB-DL", codec: "x265", audio: "DDP", hdr: "HDR", season: 2, episode: 5, cleanTitle: "The Expanse", group: "CasStudio" }),
  mk("The Expanse S02 Complete 1080p BluRay x265-RARBG", 18.7e9, 89, 22, "1337x", "tv", { resolution: "1080p", source: "BluRay", codec: "x265", season: 2, seasonPack: true, cleanTitle: "The Expanse", group: "RARBG" }, 3, ["TorrentGalaxy", "LimeTorrents"]),
  mk("The.Expanse.S02E05.720p.HDTV.x264-AVS", 0.9e9, 34, 6, "TorrentGalaxy", "tv", { resolution: "720p", source: "HDTV", codec: "x264", season: 2, episode: 5, cleanTitle: "The Expanse", group: "AVS" }),
  mk("Dune.Part.Two.2024.2160p.UHD.BluRay.REMUX.DV.HDR10.TrueHD.Atmos.7.1-FraMeSToR", 78.2e9, 267, 45, "TorrentLeech", "movie", { resolution: "2160p", source: "Remux", hdr: "DV", audio: "Atmos", year: 2024, cleanTitle: "Dune Part Two", group: "FraMeSToR" }),
  mk("Dune Part Two (2024) 1080p WEBRip x264 AAC-YTS", 2.1e9, 1834, 210, "YTS", "movie", { resolution: "1080p", source: "WEBRip", codec: "x264", audio: "AAC", year: 2024, cleanTitle: "Dune Part Two", group: "YTS" }, 4, ["1337x", "TorrentGalaxy", "LimeTorrents"]),
];

function mk(title: string, size: number, seeders: number, leechers: number, indexer: string, kind: Release["kind"], p: Partial<ParsedRelease>, dupeCount = 1, alsoOn: string[] = []): Release {
  n += 1;
  return {
    guid: `mock-${n}`, title, size, indexer, indexerId: n,
    infoUrl: null, downloadUrl: null,
    magnetUrl: `magnet:?xt=urn:btih:${"0".repeat(39)}${n}`,
    infoHash: null, seeders, leechers, protocol: "torrent",
    publishDate: new Date(Date.now() - n * 8.64e7).toISOString(),
    age: n * 3, grabs: seeders * 4,
    parsed: {
      resolution: null, source: null, codec: null, audio: null, hdr: null,
      group: null, season: null, episode: null, year: null,
      proper: false, seasonPack: false, cleanTitle: title, ...p,
    },
    score: seeders * 0.3, dupeCount, alsoOn, kind, relevant: true,
  };
}

let mockTorrents: QbitTorrent[] = [
  { hash: "a1", name: "Dune.Part.Two.2024.1080p.WEBRip.x264-YTS", size: 2.1e9, progress: 0.63, dlspeed: 8.4e6, upspeed: 0.2e6, eta: 94, state: "downloading", category: "trawler", save_path: "C:\\Media\\Movies", added_on: Date.now() / 1000 - 600, num_seeds: 84, num_leechs: 12, ratio: 0.12, content_path: "" },
  { hash: "b2", name: "The.Expanse.S02E05.1080p.WEB-DL.DDP5.1.H.264-NTb", size: 2.4e9, progress: 1, dlspeed: 0, upspeed: 1.1e6, eta: 8640000, state: "uploading", category: "trawler", save_path: "C:\\Media\\TV", added_on: Date.now() / 1000 - 7200, num_seeds: 0, num_leechs: 3, ratio: 1.45, content_path: "" },
];

const mockTvm: TvmazeResult[] = [
  { id: 44933, name: "Severance", status: "Running", premiered: "2022-02-18", ended: null, network: "Apple TV+", poster: "https://static.tvmaze.com/uploads/images/medium_portrait/385/964725.jpg", genres: ["Drama", "Science-Fiction"], summary: null, followed: true },
  { id: 41428, name: "Foundation", status: "Running", premiered: "2021-09-24", ended: null, network: "Apple TV+", poster: "https://static.tvmaze.com/uploads/images/medium_portrait/566/1415280.jpg", genres: ["Science-Fiction"], summary: null, followed: false },
  { id: 2993, name: "The Expanse", status: "Ended", premiered: "2015-12-14", ended: "2022-01-14", network: "Amazon Prime", poster: "https://static.tvmaze.com/uploads/images/medium_portrait/330/826443.jpg", genres: ["Science-Fiction"], summary: null, followed: false },
];

function mkShowRow(s: TvmazeResult): ShowRow {
  return {
    tvmazeId: s.id, name: s.name, status: s.status, posterUrl: s.poster,
    premiered: s.premiered, ended: s.ended, network: s.network, imdbId: null,
    followedAt: Date.now() / 1000, refreshedAt: Date.now() / 1000,
    qualityJson: null, savePathOverride: null, backfill: true,
    total: 19, downloaded: 12, grabbed: 2, wanted: 5,
    nextAirstamp: new Date(Date.now() + 4 * 8.64e7).toISOString(),
  };
}

const MOCK_SHOW_SEED: Array<[string, string, string | null, string, number, number, number, number]> = [
  // name, status, poster, network, total, downloaded, grabbed, wanted
  ["Severance", "Running", "https://static.tvmaze.com/uploads/images/medium_portrait/548/1371406.jpg", "Apple TV+", 19, 12, 2, 5],
  ["Foundation", "Running", "https://static.tvmaze.com/uploads/images/medium_portrait/573/1433544.jpg", "Apple TV+", 30, 30, 0, 0],
  ["Silo", "Running", "https://static.tvmaze.com/uploads/images/medium_portrait/631/1577677.jpg", "Apple TV+", 20, 17, 1, 2],
  ["Slow Horses", "Running", "https://static.tvmaze.com/uploads/images/medium_portrait/637/1593462.jpg", "Apple TV+", 36, 36, 0, 0],
  ["Shōgun", "Running", "https://static.tvmaze.com/uploads/images/medium_portrait/506/1265637.jpg", "Hulu", 10, 10, 0, 0],
  ["The Expanse", "Ended", "https://static.tvmaze.com/uploads/images/medium_portrait/445/1114081.jpg", "Prime Video", 62, 62, 0, 0],
  ["Andor", "Ended", "https://static.tvmaze.com/uploads/images/medium_portrait/564/1411766.jpg", "Disney+", 24, 21, 3, 0],
  ["Dark", "Ended", "https://static.tvmaze.com/uploads/images/medium_portrait/504/1262352.jpg", "Netflix", 26, 26, 0, 0],
];

const mockShows: ShowRow[] = MOCK_SHOW_SEED.map(([name, status, poster, network, total, downloaded, grabbed, wanted], i) => ({
  tvmazeId: 44933 + i,
  name, status, posterUrl: poster, network,
  premiered: "2022-01-01", ended: status === "Ended" ? "2025-01-01" : null, imdbId: null,
  followedAt: Date.now() / 1000 - 86400 * (i + 1), refreshedAt: Date.now() / 1000,
  qualityJson: null, savePathOverride: null, backfill: true,
  total, downloaded, grabbed, wanted,
  nextAirstamp: status === "Running" && wanted === 0 && i % 2 === 0
    ? new Date(Date.now() + (i + 2) * 8.64e7).toISOString()
    : wanted > 0
      ? new Date(Date.now() + 4 * 8.64e7).toISOString()
      : null,
}));

const mockEpisodes: EpisodeRow[] = Array.from({ length: 19 }, (_, i) => {
  const season = i < 9 ? 1 : 2;
  const number = (i % 9) + 1;
  return {
    tvmazeEpId: 9000 + i, showId: 44933, season, number,
    title: `Episode ${number}`,
    airstamp: new Date(Date.now() - (19 - i) * 7 * 8.64e7).toISOString(),
    state: (i < 12 ? "downloaded" : i < 14 ? "grabbed" : i < 17 ? "wanted" : "upcoming") as EpisodeState,
    grabbedTitle: i < 14 ? `Severance.S0${season}E0${number}.1080p.WEB-DL.x265-GROUP` : null,
  };
});

const mockChat: ChatRow[] = [];

const mockSetup: SetupStatus = {
  qbit: "missing",
  prowlarr: "missing",
  agent: "ok",
  prowlarrHasIndexers: false,
};

const mockBriefs: BriefRow[] = [
  {
    id: 1, name: "UFC numbered events", prompt: "Grab UFC numbered events, main card only, 1080p, under 6 GB",
    planJson: JSON.stringify({ queries: ["ufc"], include: ["ufc"], exclude: ["prelims"], resolutions: ["1080p"], maxSizeGb: 6, minSeeders: 5, notes: "" }),
    cadenceMinutes: 180, mode: "propose", maxGrabsPerRun: 2, maxGbPerRun: 12, maxGbPerDay: 30,
    enabled: true, createdAt: Date.now() / 1000 - 86400, lastRunAt: Date.now() / 1000 - 3600,
    lastReport: "Searched 3 variations. UFC 319 main card found in 1080p WEB-DL (4.2 GB, 88 seeders) — proposed for approval. Nothing else new.",
    failStreak: 0, pausedReason: null,
  },
  {
    id: 2, name: "F1 race weekends", prompt: "F1 2026 race weekends, race only not practice, 1080p",
    planJson: JSON.stringify({ queries: ["formula 1 2026"], include: ["formula"], exclude: ["practice", "fp1"], resolutions: ["1080p"], maxSizeGb: 12, minSeeders: 3, notes: "" }),
    cadenceMinutes: 720, mode: "auto", maxGrabsPerRun: 1, maxGbPerRun: 12, maxGbPerDay: 24,
    enabled: true, createdAt: Date.now() / 1000 - 172800, lastRunAt: Date.now() / 1000 - 21600,
    lastReport: "No new races since Zandvoort. Next race weekend begins Friday.", failStreak: 0, pausedReason: null,
  },
];

const mockProposals: ProposalRow[] = [
  {
    id: 1, briefId: 1, briefName: "UFC numbered events", contentKey: "item:ufc 319",
    result: { title: "UFC.319.Main.Card.1080p.WEB-DL.H264-VERUM", size: 4.2e9, seeders: 88, indexer: "Knaben", resolution: "1080p", source: "WEB-DL", codec: "x264" },
    reason: "Numbered event, main card only, comfortably under the 6 GB cap with healthy seeders.",
    status: "pending", firstSeen: Date.now() / 1000 - 1800, lastSeen: Date.now() / 1000 - 1800,
  },
];

/** Simulates the backend's streaming agent-step events for browser dev. */
async function runMockAgent(text: string) {
  const runId = "chat-mock";
  const step = (kind: AgentStep["kind"], payload: AgentStep["payload"] = {}) =>
    emitMockStep({ runId, kind, payload });
  await sleep(400);
  step("thinking");
  await sleep(900);
  step("tool_call", { tool: "search_releases", args: { query: text.slice(0, 40) } });
  await sleep(1400);
  step("tool_result", {
    tool: "search_releases",
    summary: `Searched «${text.slice(0, 30)}» → 23 results`,
    detail: { results: [{ id: "r1", title: "Example.Release.1080p.WEB-DL.x265-GROUP", sizeGb: 3.4, seeders: 64 }] },
  });
  await sleep(700);
  step("thinking");
  await sleep(1100);
  const reply =
    "I found **23 releases** and the strongest match is `Example.Release.1080p.WEB-DL.x265-GROUP` — 3.4 GB, 64 seeders, WEB-DL rather than a re-encode.\n\nWant me to grab it, or should I show the alternatives first?";
  step("text", { text: reply });
  mockChat.push({ id: Date.now(), ts: Date.now() / 1000, role: "assistant", content: reply, toolName: null, toolPayload: null });
  step("done");
}

async function mock(cmd: string, args?: Record<string, unknown>): Promise<unknown> {
  await sleep(cmd === "search" ? 900 : 250);
  switch (cmd) {
    case "get_config":
      return {
        prowlarrUrl: "http://127.0.0.1:9696", prowlarrApiKey: "",
        qbitUrl: "http://127.0.0.1:8080", qbitUsername: "", qbitPassword: "",
        qbitCategory: "trawler", seedPolicy: "default", seedRatio: 1.0,
        savePathMovies: "", savePathTv: "", addPaused: false,
        defaultQuality: { resolutions: ["1080p", "720p"], codec: "prefer-x265", maxSizeGb: 0, allowSeasonPacks: true },
        qualityPresets: [
          { name: "Best available", profile: { resolutions: ["2160p", "1080p"], codec: "prefer-x265", maxSizeGb: 0, allowSeasonPacks: true } },
          { name: "1080p quality", profile: { resolutions: ["1080p"], codec: "prefer-x265", maxSizeGb: 8, allowSeasonPacks: true } },
          { name: "1080p balanced", profile: { resolutions: ["1080p", "720p"], codec: "prefer-x265", maxSizeGb: 3, allowSeasonPacks: true } },
          { name: "Space saver", profile: { resolutions: ["720p"], codec: "prefer-x265", maxSizeGb: 1.5, allowSeasonPacks: true } },
        ],
        schedulerMinutes: 30,
        notifyOnGrab: true,
        discordWebhook: "",
        telegramBotToken: "",
        telegramChatId: "",
        notifyGrabs: true,
        notifyCompletions: true,
        notifyProposals: true,
        notifyErrors: false,
        closeToTray: true,
        agentBaseUrl: "http://127.0.0.1:11434",
        agentModel: "kimi-k2.6:cloud",
        agentEnabled: true,
        agentMinFreeDiskGb: 50,
        medicMode: "propose",
        setupCompleted: !new URLSearchParams(window.location.search).has("wizard"),
        rssEnabled: true,
        rssMinutes: 15,
        upgradeScoutEnabled: false,
        upgradeWindowDays: 30,
      } satisfies Config;
    case "set_config":
      return args?.config;
    case "test_connections":
      return { prowlarrOk: true, prowlarrDetail: "Prowlarr 2.1.3 (mock)", qbitOk: true, qbitDetail: "qBittorrent v5.2.3 (mock)" } satisfies ConnectionStatus;
    case "list_indexers":
      return [
        { id: 1, name: "TorrentLeech", enable: true, protocol: "torrent", privacy: "private" },
        { id: 2, name: "1337x", enable: true, protocol: "torrent", privacy: "public" },
        { id: 3, name: "TorrentGalaxy", enable: true, protocol: "torrent", privacy: "public" },
        { id: 4, name: "YTS", enable: true, protocol: "torrent", privacy: "public" },
      ] satisfies Indexer[];
    case "search": {
      // demo hook: see the no-indexers recovery state in the browser
      if (String(args?.query ?? "").includes("noindexers")) {
        return { releases: [], totalBeforeDedupe: 0, indexers: [] } satisfies SearchResponse;
      }
      const kind = (args?.kind as string) ?? "all";
      const q = ((args?.query as string) ?? "").toLowerCase();
      const rel = mockReleases.filter(
        (r) =>
          (kind === "all" || (kind === "movies" ? r.kind === "movie" : r.kind === "tv")) &&
          r.parsed.cleanTitle.toLowerCase().includes(q.split(" ")[0] ?? ""),
      );
      return {
        releases: rel.length ? rel : mockReleases,
        totalBeforeDedupe: 23,
        indexers: [
          { id: 1, name: "TorrentLeech", count: 9, ok: true, timedOut: false, elapsedMs: 412 },
          { id: 2, name: "1337x", count: 8, ok: true, timedOut: false, elapsedMs: 630 },
          { id: 3, name: "The Pirate Bay", count: 0, ok: false, timedOut: true, elapsedMs: 15000 },
          { id: 4, name: "YTS", count: 6, ok: true, timedOut: false, elapsedMs: 227 },
        ],
      } satisfies SearchResponse;
    }
    case "grab":
      return { ok: true, detail: `Sent to qBittorrent: ${args?.title}` } satisfies GrabResult;
    case "downloads":
      mockTorrents = mockTorrents.map((t) =>
        t.state === "downloading"
          ? { ...t, progress: Math.min(1, t.progress + 0.01), eta: Math.max(0, t.eta - 2) }
          : t,
      );
      return { torrents: mockTorrents, transfer: { dl_info_speed: 8.4e6, up_info_speed: 1.3e6 } } satisfies DownloadsView;
    case "torrent_action":
      return;
    case "search_tvmaze": {
      const q = ((args?.query as string) ?? "").toLowerCase();
      return mockTvm.filter((s) => s.name.toLowerCase().includes(q));
    }
    case "follow_show": {
      const src = mockTvm.find((s) => s.id === args?.tvmazeId);
      if (src) {
        src.followed = true;
        const row = mkShowRow(src);
        mockShows.push(row);
        return row;
      }
      // ids from the import flow aren't in the seeded search list — synthesize
      const row = mkShowRow({
        id: Number(args?.tvmazeId) || 0,
        name: `Imported show ${args?.tvmazeId}`,
        status: "Running",
        premiered: "2020-01-01",
        ended: null,
        network: "HBO",
        poster: null,
        genres: [],
        summary: null,
        followed: true,
      });
      mockShows.push(row);
      return row;
    }
    case "unfollow_show": {
      const i = mockShows.findIndex((s) => s.tvmazeId === args?.tvmazeId);
      if (i >= 0) mockShows.splice(i, 1);
      return;
    }
    case "get_shows":
      return mockShows;
    case "get_show_episodes":
      return mockEpisodes.filter((e) => e.showId === args?.tvmazeId);
    case "refresh_show":
      return;
    case "set_episode_state": {
      const ep = mockEpisodes.find((e) => e.tvmazeEpId === args?.tvmazeEpId);
      if (ep) ep.state = args?.newState as EpisodeState;
      return;
    }
    case "import_lookup": {
      const all = String(args?.input ?? "").split("\n").map((l) => l.trim()).filter(Boolean);
      const lines = all.slice(0, 50);
      const mk = (id: number, name: string, year: string, followed = false): ImportMatch => ({
        id, name, premiered: year + "-01-01", network: "HBO", status: "Running", poster: null, followed,
      });
      const rows = lines.map((line, i): ImportRow => {
        if (i % 4 === 3) return { idx: i, input: line, matches: [], chosen: null, confidence: "none", lookupFailed: false };
        if (i % 4 === 2) {
          const c = [mk(70000 + i, line, "1978"), mk(71000 + i, line, "2004"), mk(72000 + i, line + " Aftershow", "2005")];
          return { idx: i, input: line, matches: c, chosen: c[0].id, confidence: "ambiguous", lookupFailed: false };
        }
        const m = mk(60000 + i, line, "2019", i === 0);
        return { idx: i, input: line, matches: [m], chosen: m.id, confidence: i % 4 === 0 ? "exact" : "good", lookupFailed: false };
      });
      return { rows, totalFound: all.length } satisfies ImportLookup;
    }
    case "import_estimate": {
      const n = (args?.ids as number[] | undefined)?.length ?? 0;
      return { shows: n, airedEpisodes: n * 18, estGb: n * 18 * 2.5, failed: 0 } satisfies ImportEstimate;
    }
    case "import_disambiguate": {
      const rows = (args?.rows as Array<{ candidates: Array<{ id: number }> }>) ?? [];
      await sleep(1200);
      return rows.map((r) => r.candidates[1]?.id ?? r.candidates[0]?.id ?? null);
    }
    case "upgrade_scan_now":
      return;
    case "notify_test":
      return { discord: "Delivered", discordOk: true, telegram: null, telegramOk: false } satisfies NotifyTestOutcome;
    case "set_show_options":
      return;
    case "preview_show_grabs":
      return [
        { showId: 44933, showName: "Severance", season: 2, episodes: [6, 7, 8], epIds: [1, 2, 3], title: "Severance.S02.1080p.ATVP.WEB-DL.DDP5.1.x265-PACK", indexer: "Knaben", size: 14.2e9, seeders: 84, isPack: true, magnetUrl: null, downloadUrl: null },
        { showId: 44933, showName: "Severance", season: 1, episodes: [9], epIds: [9], title: "Severance.S01E09.1080p.WEB-DL.x265-NTb", indexer: "TorrentLeech", size: 2.1e9, seeders: 40, isPack: false, magnetUrl: null, downloadUrl: null },
      ] satisfies PlannedGrab[];
    case "run_scheduler_now":
      return;
    case "indexer_defs":
      return [
        { name: "ExtraTorrent.st", description: "Public tracker for MOVIE / TV / GENERAL", privacy: "public", language: "en-US" },
        { name: "TorrentDownloads", description: "Public torrent site", privacy: "public", language: "en-US" },
        { name: "Nyaa.si", description: "Public torrent site focused on Eastern Asian media", privacy: "public", language: "en-US" },
      ] satisfies IndexerDef[];
    case "add_indexer":
      return args?.name;
    case "toggle_indexer":
    case "remove_indexer":
      return;
    case "get_autostart":
      return false;
    case "set_autostart":
      return;

    // ---- setup wizard mocks: interactive state machine ----
    case "setup_status":
      return { ...mockSetup };
    case "setup_install_prowlarr": {
      for (let pct = 5; pct <= 100; pct += 19) {
        await sleep(350);
        emitMockSetup({ component: "prowlarr", kind: "progress", payload: { pct } });
      }
      emitMockSetup({ component: "prowlarr", kind: "log", payload: { message: "Starting Prowlarr…" } });
      await sleep(900);
      mockSetup.prowlarr = "ok";
      emitMockSetup({ component: "prowlarr", kind: "done", payload: { message: "Prowlarr is running and connected" } });
      return "mock-api-key";
    }
    case "setup_start_prowlarr":
      await sleep(1200);
      mockSetup.prowlarr = "ok";
      return;
    case "setup_install_qbit":
      void (async () => {
        await sleep(2500);
        mockSetup.qbit = "installed_stopped";
      })();
      return;
    case "setup_configure_qbit":
      await sleep(900);
      mockSetup.qbit = "ok";
      return;
    case "setup_save_prowlarr_key":
      return;
    case "setup_starter_indexers":
      await sleep(1500);
      mockSetup.prowlarrHasIndexers = true;
      return ["YTS", "The Pirate Bay", "LimeTorrents", "Knaben"];
    case "setup_finish":
      return;
    case "calendar_range": {
      const start = new Date(String(args?.start)).getTime();
      const end = new Date(String(args?.end)).getTime();
      const items: CalendarItem[] = [];
      for (const show of mockShows) {
        // scatter a few episodes for each show across the requested window
        for (let i = 0; i < 6; i++) {
          const t = start + ((show.tvmazeId * 37 + i * 97) % Math.max(1, Math.floor((end - start) / 3.6e6))) * 3.6e6;
          if (t >= end) continue;
          const past = t < Date.now();
          const when = new Date(t);
          items.push({
            showId: show.tvmazeId, showName: show.name, posterUrl: show.posterUrl,
            season: 3, number: i + 1, title: `Episode ${i + 1}`,
            airstamp: when.toISOString(),
            localDate: `${when.getFullYear()}-${String(when.getMonth() + 1).padStart(2, "0")}-${String(when.getDate()).padStart(2, "0")}`,
            state: past ? (i % 3 === 0 ? "wanted" : "downloaded") : "upcoming",
          });
        }
      }
      return items.sort((a, b) => a.airstamp.localeCompare(b.airstamp));
    }
    case "export_ical":
      return "C:\\Users\\you\\Downloads\\trawler-calendar.ics";
    case "discover": {
      const mk = (i: number, followed = false): DiscoverEntry => ({
        showId: 90000 + i,
        name: ["Severance", "Foundation", "Silo", "Slow Horses", "Shōgun", "The Expanse", "Andor", "Dark"][i % 8],
        poster: MOCK_SHOW_SEED[i % 8][2],
        network: ["Apple TV+", "HBO", "Netflix", "Disney+"][i % 4],
        status: "Running", weight: 100 - i,
        airstamp: new Date(Date.now() + (i % 5) * 3.6e6 + 3.6e6).toISOString(),
        season: 3, number: (i % 9) + 1, episodeTitle: "New Episode",
        followed: followed || i % 5 === 0,
      });
      return {
        tonight: Array.from({ length: 12 }, (_, i) => mk(i)),
        premieres: Array.from({ length: 6 }, (_, i) => mk(i + 3)),
        popular: Array.from({ length: 12 }, (_, i) => mk(i + 1)),
      } satisfies DiscoverRows;
    }
    case "rss_sweep_now":
      return { releasesSeen: 367, episodeGrabs: 1, briefGrabs: 0, briefProposals: 1 };
    case "show_preview": {
      const seed = MOCK_SHOW_SEED[(Number(args?.tvmazeId) || 0) % 8];
      return {
        tvmazeId: Number(args?.tvmazeId) || 90000,
        name: seed[0],
        summary:
          "In a world where memory itself can be divided, a group of office workers discover that the boundary between their work selves and their outside lives conceals something far stranger than a corporate perk. Critically adored, quietly terrifying, and impossible to stop watching.",
        genres: ["Drama", "Science-Fiction", "Thriller"],
        status: seed[1],
        premiered: "2022-02-18",
        ended: seed[1] === "Ended" ? "2025-04-18" : null,
        network: seed[3],
        runtime: 52,
        rating: 8.6,
        poster: seed[2],
        imdb: "tt11280740",
        scheduleDays: "Friday",
        scheduleTime: "21:00",
        episodeCount: 19,
        airedEpisodeCount: 16,
        seasonCount: 2,
        cast: [
          { person: "Adam Scott", character: "Mark S.", image: null },
          { person: "Britt Lower", character: "Helly R.", image: null },
          { person: "Patricia Arquette", character: "Ms. Cobel", image: null },
          { person: "John Turturro", character: "Irving B.", image: null },
        ],
        // mirror the discover mock's i % 5 convention so unfollowed previews exist in browser dev
        followed: ((Number(args?.tvmazeId) || 0) - 90000) % 5 === 0,
      } satisfies ShowPreview;
    }

    // ---- agent mocks ----
    case "agent_history":
      return mockChat;
    case "agent_clear":
      mockChat.length = 0;
      return;
    case "agent_models":
      return ["kimi-k2.6:cloud", "deepseek-v4-flash:cloud", "qwen3.6:35b-mlx", "qwen3:8b"];
    case "agent_send": {
      const text = String(args?.text ?? "");
      mockChat.push({ id: Date.now(), ts: Date.now() / 1000, role: "user", content: text, toolName: null, toolPayload: null });
      void runMockAgent(text);
      return "mock-run";
    }
    case "briefs_list":
      return mockBriefs;
    case "compile_brief":
      return {
        queries: ["ufc 319", "ufc 319 main card", "ufc319"],
        include: ["ufc"],
        exclude: ["prelims", "early"],
        resolutions: ["1080p"],
        maxSizeGb: 6,
        minSeeders: 5,
        notes: "Numbered UFC events only, main card broadcast. Prefer WEB-DL over HDTV.",
      } satisfies HuntPlan;
    case "brief_save": {
      const b = args as unknown as BriefSaveInput;
      if (b.id) {
        const i = mockBriefs.findIndex((x) => x.id === b.id);
        if (i >= 0) mockBriefs[i] = { ...mockBriefs[i], ...b, id: b.id, planJson: JSON.stringify(b.plan) };
        return b.id;
      }
      const id = Math.max(0, ...mockBriefs.map((x) => x.id)) + 1;
      mockBriefs.push({
        id, name: b.name, prompt: b.prompt, planJson: JSON.stringify(b.plan),
        cadenceMinutes: b.cadenceMinutes, mode: b.mode, maxGrabsPerRun: b.maxGrabsPerRun,
        maxGbPerRun: b.maxGbPerRun, maxGbPerDay: b.maxGbPerDay, enabled: b.enabled,
        createdAt: Date.now() / 1000, lastRunAt: 0, lastReport: null, failStreak: 0, pausedReason: null,
      });
      return id;
    }
    case "brief_delete": {
      const i = mockBriefs.findIndex((x) => x.id === args?.id);
      if (i >= 0) mockBriefs.splice(i, 1);
      return;
    }
    case "brief_run_now": {
      const b = mockBriefs.find((x) => x.id === args?.id);
      if (b) {
        b.lastRunAt = Date.now() / 1000;
        b.lastReport = "Searched 3 query variations, found 1 new qualifying release and proposed it for approval.";
      }
      return;
    }
    case "proposals_list":
      return mockProposals;
    case "proposal_resolve": {
      const i = mockProposals.findIndex((p) => p.id === args?.id);
      if (i >= 0) {
        const [p] = mockProposals.splice(i, 1);
        return args?.approve ? `Grabbed ${p.result.title}` : "dismissed";
      }
      return "gone";
    }
    case "get_activity":
      return [
        { id: 3, ts: Date.now() / 1000 - 300, kind: "grab", showId: 44933, message: "Grabbed Severance S02E07 · 1080p WEB-DL x265 · Knaben · 1.9 GB" },
        { id: 2, ts: Date.now() / 1000 - 3900, kind: "refresh", showId: 44933, message: "Refreshed Severance — 1 new episode registered" },
        { id: 1, ts: Date.now() / 1000 - 86400, kind: "follow", showId: 44933, message: "Following Severance (Running)" },
      ] satisfies ActivityRow[];
    default:
      throw new Error(`no mock for ${cmd}`);
  }
}
