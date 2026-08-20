import { create } from "zustand";
import type { UpdateInfo } from "./lib/updater";
import {
  api,
  onAgentStep,
  type ActivityRow,
  type AgentStep,
  type BriefRow,
  type ChatRow,
  type Config,
  type ConnectionStatus,
  type EpisodeRow,
  type ProposalRow,
  type Release,
  type SearchResponse,
  type ShowRow,
} from "./lib/api";

export type View = "search" | "agent" | "shows" | "downloads" | "settings";

/** A live tool step being rendered inside the in-flight assistant turn. */
export interface LiveStep {
  key: number;
  tool: string;
  summary: string;
  detail?: unknown;
  running: boolean;
}
export type Kind = "all" | "movies" | "tv";
export type SortKey = "score" | "seeders" | "size" | "age";

export interface SearchFilters {
  resolution: string | null;
  codec: string | null;
  source: string | null;
}

export interface Toast {
  id: number;
  text: string;
  tone: "ok" | "bad" | "info";
}

interface Store {
  view: View;
  setView: (v: View) => void;
  goHome: () => void;

  config: Config | null;
  loadConfig: () => Promise<void>;
  /** resolves true when the config actually persisted */
  saveConfig: (c: Config) => Promise<boolean>;

  status: ConnectionStatus | null;
  refreshStatus: () => Promise<void>;

  // search state survives view switches
  query: string;
  kind: Kind;
  sort: SortKey;
  /** secondary sort: decides among releases the banded primary calls equal */
  sortThen: SortKey | null;
  setQuery: (q: string) => void;
  setKind: (k: Kind) => void;
  setSort: (s: SortKey) => void;
  setSortThen: (s: SortKey | null) => void;
  searching: boolean;
  hasSearched: boolean;
  results: SearchResponse | null;
  searchError: string | null;
  /** show releases whose names don't contain the query terms */
  showHidden: boolean;
  setShowHidden: (v: boolean) => void;
  runSearch: () => Promise<void>;

  /** quick post-filters over search results */
  filters: SearchFilters;
  setFilter: (dim: keyof SearchFilters, value: string | null) => void;

  grabState: Record<string, "loading" | "done" | "error">;
  grab: (r: Release) => Promise<void>;

  // ---- followed shows ----
  shows: ShowRow[];
  showsLoaded: boolean;
  loadShows: () => Promise<void>;
  followShow: (tvmazeId: number, backfill: boolean) => Promise<void>;
  unfollowShow: (tvmazeId: number) => Promise<void>;
  selectedShow: ShowRow | null;
  selectShow: (s: ShowRow | null) => void;
  activity: ActivityRow[];
  loadActivity: () => Promise<void>;
  /** when set, a successful grab matching season/episode marks this episode grabbed */
  episodeLink: { ep: EpisodeRow; showName: string } | null;
  findReleaseForEpisode: (showName: string, ep: EpisodeRow) => void;

  // ---- agent ----
  chat: ChatRow[];
  chatLoaded: boolean;
  loadChat: () => Promise<void>;
  agentBusy: boolean;
  liveSteps: LiveStep[];
  agentThinking: boolean;
  sendToAgent: (text: string) => Promise<void>;
  clearChat: () => Promise<void>;
  initAgentEvents: () => void;
  briefs: BriefRow[];
  loadBriefs: () => Promise<void>;
  proposals: ProposalRow[];
  loadProposals: () => Promise<void>;
  resolveProposal: (id: number, approve: boolean) => Promise<void>;

  toasts: Toast[];
  toast: (text: string, tone?: Toast["tone"]) => void;
  dismissToast: (id: number) => void;

  /** an update the updater found — the banner renders it, Settings feeds it */
  pendingUpdate: UpdateInfo | null;
  setPendingUpdate: (u: UpdateInfo | null) => void;
  /** version the user clicked "Later" on (banner stays hidden for it) */
  updateDismissedVersion: string | null;
  setUpdateDismissedVersion: (v: string | null) => void;
}

let toastSeq = 1;
/** Monotonic search id — a newer search always supersedes an older in-flight one. */
let searchSeq = 0;
let agentEventsInitialized = false;

function friendlyToolName(tool?: string): string {
  switch (tool) {
    case "search_releases": return "Searching indexers";
    case "grab_release": return "Grabbing";
    case "propose_release": return "Writing a proposal";
    case "search_shows_tvmaze": return "Looking up shows";
    case "estimate_follow": return "Estimating follow cost";
    case "list_follows": return "Checking your shows";
    case "get_downloads": return "Checking downloads";
    case "get_activity": return "Reading activity";
    case "remember": return "Taking a note";
    default: return tool ?? "Working";
  }
}

export const useStore = create<Store>((set, get) => ({
  view: "search",
  setView: (view) => set({ view }),

  config: null,
  loadConfig: async () => {
    try {
      set({ config: await api.getConfig() });
    } catch (e) {
      get().toast(String(e), "bad");
    }
  },
  saveConfig: async (c) => {
    try {
      const saved = await api.setConfig(c);
      set({ config: saved });
      get().toast("Settings saved", "ok");
      void get().refreshStatus();
      return true;
    } catch (e) {
      get().toast(String(e), "bad");
      return false;
    }
  },

  status: null,
  refreshStatus: async () => {
    try {
      set({ status: await api.testConnections() });
    } catch {
      set({ status: null });
    }
  },

  query: "",
  kind: "all",
  sort: "score",
  sortThen: null,
  // typing a new query manually breaks any episode linkage
  setQuery: (query) => set({ query, episodeLink: null }),
  /** back to the landing hero: view + search state reset */
  goHome: () => {
    const alreadyThere = get().view === "search";
    set({ view: "search" });
    if (alreadyThere) {
      set({
        query: "",
        results: null,
        hasSearched: false,
        searching: false,
        searchError: null,
        showHidden: false,
        episodeLink: null,
        filters: { resolution: null, codec: null, source: null },
        grabState: {},
        sort: "score",
        sortThen: null,
      });
    }
  },
  setKind: (kind) => set({ kind }),
  // "Best" is already a composite score; a secondary only makes sense for
  // single-key sorts, and never the same key twice
  setSort: (sort) =>
    set((st) => ({
      sort,
      // picking the current secondary as the new primary SWAPS them — the
      // user's crossed intent survives instead of being silently dropped
      sortThen:
        sort === "score" ? null : st.sortThen === sort ? (st.sort === "score" ? null : st.sort) : st.sortThen,
    })),
  setSortThen: (sortThen) => set({ sortThen }),
  searching: false,
  hasSearched: false,
  results: null,
  searchError: null,
  showHidden: false,
  setShowHidden: (showHidden) => set({ showHidden }),
  runSearch: async () => {
    const { query, kind } = get();
    if (!query.trim()) return;
    const seq = ++searchSeq;
    set({
      searching: true,
      hasSearched: true,
      searchError: null,
      showHidden: false,
      // a chip armed for the last search silently emptying THIS one is the
      // worst kind of dead end; grab state belongs to a result set, not a session
      filters: { resolution: null, codec: null, source: null },
      grabState: {},
    });
    try {
      const kindParam = kind === "all" ? "all" : kind;
      const results = await api.search(query.trim(), kindParam);
      if (seq !== searchSeq) return; // a newer search took over
      set({ results, searching: false });
    } catch (e) {
      if (seq !== searchSeq) return;
      set({ searchError: String(e), searching: false, results: null });
    }
  },

  filters: { resolution: null, codec: null, source: null },
  setFilter: (dim, value) =>
    set((s) => ({
      filters: { ...s.filters, [dim]: s.filters[dim] === value ? null : value },
    })),

  grabState: {},
  grab: async (r) => {
    const key = r.guid ?? r.title;
    set((s) => ({ grabState: { ...s.grabState, [key]: "loading" } }));
    try {
      // an episode-linked grab carries its episode id so the backend ledger
      // remembers WHO this grab was for (survives unfollow/refollow)
      const link0 = get().episodeLink;
      const linked =
        link0 &&
        r.parsed.season === link0.ep.season &&
        (r.parsed.episode === link0.ep.number || r.parsed.seasonPack)
          ? [link0.ep.tvmazeEpId]
          : [];
      const res = await api.grab(r, linked);
      set((s) => ({ grabState: { ...s.grabState, [key]: "done" } }));
      get().toast(res.detail, "ok");
      // If this grab was initiated from an episode row and the release matches
      // that episode (or is its season pack), record it on the show.
      const link = get().episodeLink;
      if (link && r.parsed.season === link.ep.season) {
        const episodeMatch = r.parsed.episode === link.ep.number;
        const packMatch = r.parsed.seasonPack;
        if (episodeMatch || packMatch) {
          try {
            await api.setEpisodeState(link.ep.tvmazeEpId, "grabbed", r.title);
            set({ episodeLink: null });
            void get().loadShows();
          } catch {
            /* non-fatal */
          }
        }
      }
    } catch (e) {
      set((s) => ({ grabState: { ...s.grabState, [key]: "error" } }));
      get().toast(String(e), "bad");
    }
  },

  shows: [],
  showsLoaded: false,
  loadShows: async () => {
    try {
      const shows = await api.getShows();
      set({ shows, showsLoaded: true });
      const sel = get().selectedShow;
      if (sel) {
        set({ selectedShow: shows.find((s) => s.tvmazeId === sel.tvmazeId) ?? null });
      }
    } catch (e) {
      get().toast(String(e), "bad");
    }
  },
  followShow: async (tvmazeId, backfill) => {
    try {
      const row = await api.followShow(tvmazeId, backfill, null);
      get().toast(`Following ${row.name}`, "ok");
      await get().loadShows();
    } catch (e) {
      get().toast(String(e), "bad");
      throw e; // callers need the failure to be observable
    }
  },
  unfollowShow: async (tvmazeId) => {
    try {
      await api.unfollowShow(tvmazeId);
      set({ selectedShow: null });
      await get().loadShows();
    } catch (e) {
      get().toast(String(e), "bad");
    }
  },
  selectedShow: null,
  selectShow: (selectedShow) => set({ selectedShow }),
  activity: [],
  loadActivity: async () => {
    try {
      set({ activity: await api.getActivity() });
    } catch {
      /* activity is decorative */
    }
  },
  episodeLink: null,
  findReleaseForEpisode: (showName, ep) => {
    const clean = showName
      .replace(/['’]/g, "")
      .replace(/[:,.()!?]/g, " ")
      .replace(/&/g, " and ")
      .replace(/\s+/g, " ")
      .trim();
    const query = `${clean} S${String(ep.season).padStart(2, "0")}E${String(ep.number).padStart(2, "0")}`;
    set({
      episodeLink: { ep, showName },
      query,
      kind: "tv",
      view: "search",
      selectedShow: null,
    });
    void get().runSearch();
  },

  chat: [],
  chatLoaded: false,
  loadChat: async () => {
    try {
      set({ chat: await api.agentHistory(), chatLoaded: true });
    } catch (e) {
      get().toast(String(e), "bad");
    }
  },
  agentBusy: false,
  liveSteps: [],
  agentThinking: false,
  sendToAgent: async (text) => {
    if (!text.trim() || get().agentBusy) return;
    set({ agentBusy: true, liveSteps: [], agentThinking: true });
    // optimistic: show the user's message instantly
    const optimisticId = Date.now();
    set((s) => ({
      chat: [
        ...s.chat,
        { id: optimisticId, ts: optimisticId / 1000, role: "user", content: text, toolName: null, toolPayload: null },
      ],
    }));
    try {
      await api.agentSend(text);
    } catch (e) {
      // roll the phantom message back so the transcript matches what the
      // backend actually stored (a rejected send stored nothing)
      set((s) => ({
        chat: s.chat.filter((c) => c.id !== optimisticId),
        liveSteps: [],
        agentBusy: false,
        agentThinking: false,
      }));
      get().toast(String(e), "bad");
    }
  },
  clearChat: async () => {
    try {
      await api.agentClear();
      set({ chat: [], liveSteps: [] });
    } catch (e) {
      get().toast(String(e), "bad");
    }
  },
  initAgentEvents: () => {
    // idempotent: React StrictMode double-runs mount effects in dev,
    // which would otherwise register two listeners and duplicate messages
    if (agentEventsInitialized) return;
    agentEventsInitialized = true;
    let stepSeq = 1;
    onAgentStep((s: AgentStep) => {
      // briefs and the medic emit the same event stream under their own run
      // ids — those must never touch the chat transcript or the busy flag
      if (!s.runId || !s.runId.startsWith("chat-")) return;
      switch (s.kind) {
        case "thinking":
          set({ agentThinking: true });
          // mark all prior steps as settled
          set((st) => ({ liveSteps: st.liveSteps.map((x) => ({ ...x, running: false })) }));
          break;
        case "tool_call":
          set((st) => ({
            agentThinking: false,
            liveSteps: [
              ...st.liveSteps,
              { key: stepSeq++, tool: s.payload.tool ?? "?", summary: `${friendlyToolName(s.payload.tool)}…`, running: true },
            ],
          }));
          break;
        case "tool_result":
          set((st) => {
            const steps = [...st.liveSteps];
            for (let i = steps.length - 1; i >= 0; i--) {
              if (steps[i].running && steps[i].tool === s.payload.tool) {
                steps[i] = { ...steps[i], summary: s.payload.summary ?? steps[i].summary, detail: s.payload.detail, running: false };
                break;
              }
            }
            return { liveSteps: steps };
          });
          break;
        case "text":
          set((st) => ({
            agentThinking: false,
            chat: [
              ...st.chat,
              { id: Date.now(), ts: Date.now() / 1000, role: "assistant", content: s.payload.text ?? "", toolName: null, toolPayload: null },
            ],
          }));
          break;
        case "error":
          set((st) => ({
            agentBusy: false,
            agentThinking: false,
            liveSteps: st.liveSteps.map((x) => ({ ...x, running: false })),
          }));
          get().toast(s.payload.message ?? "agent error", "bad");
          break;
        case "done":
          set({ agentBusy: false, agentThinking: false });
          void get().loadProposals();
          break;
      }
    });
  },
  briefs: [],
  loadBriefs: async () => {
    try {
      set({ briefs: await api.briefsList() });
    } catch {
      /* agent may be disabled */
    }
  },
  proposals: [],
  loadProposals: async () => {
    try {
      set({ proposals: await api.proposalsList() });
    } catch {
      /* ignore */
    }
  },
  resolveProposal: async (id, approve) => {
    try {
      const msg = await api.proposalResolve(id, approve);
      set((s) => ({ proposals: s.proposals.filter((p) => p.id !== id) }));
      if (approve) get().toast(msg, "ok");
    } catch (e) {
      get().toast(String(e), "bad");
    }
  },

  pendingUpdate: null,
  setPendingUpdate: (u) => {
    const old = get().pendingUpdate;
    if (old && u && old !== u && old.version === u.version) {
      // same version re-checked: keep the object the banner may already have
      // downloaded, and free the fresh duplicate instead
      void u.close();
      return;
    }
    if (old && old !== u) void old.close();
    set({ pendingUpdate: u });
  },
  updateDismissedVersion: null,
  setUpdateDismissedVersion: (updateDismissedVersion) => set({ updateDismissedVersion }),

  toasts: [],
  toast: (text, tone = "info") => {
    const id = toastSeq++;
    set((s) => ({ toasts: [...s.toasts, { id, text, tone }] }));
    setTimeout(() => get().dismissToast(id), tone === "bad" ? 7000 : 3800);
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}));
