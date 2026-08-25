import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Ban,
  ChevronDown,
  ExternalLink,
  FlaskConical,
  Package,
  RefreshCw,
  Search as SearchIcon,
  Trash2,
  Undo2,
  X,
} from "lucide-react";
import {
  api,
  type EpisodeRow,
  type EpisodeState,
  type PlannedGrab,
  type QualityProfile,
  type ShowRow,
} from "../lib/api";
import { sameProfile } from "../lib/format";
import { useStore } from "../store";
import { Button, CloseBtn, IconBtn, StatusPill, cx } from "./ui";

export default function ShowDetail({ show, onClose }: { show: ShowRow; onClose: () => void }) {
  // per-field selectors: this modal re-renders on the 15s shows poll anyway —
  // a whole-store subscription would add every keystroke and toast to that
  const unfollowShow = useStore((s) => s.unfollowShow);
  const findReleaseForEpisode = useStore((s) => s.findReleaseForEpisode);
  const toast = useStore((s) => s.toast);
  const loadShows = useStore((s) => s.loadShows);
  const [episodes, setEpisodes] = useState<EpisodeRow[] | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [confirmUnfollow, setConfirmUnfollow] = useState(false);
  const [openSeasons, setOpenSeasons] = useState<Record<number, boolean>>({});
  const [preview, setPreview] = useState<PlannedGrab[] | "loading" | null>(null);

  const runPreview = async () => {
    setPreview("loading");
    try {
      setPreview(await api.previewShowGrabs(show.tvmazeId));
    } catch (e) {
      toast(String(e), "bad");
      setPreview(null);
    }
  };

  // only the newest fetch may land — a slow response must not clobber
  // optimistic per-episode edits made after a newer one
  const loadSeq = useRef(0);
  const load = useCallback(async () => {
    const mine = ++loadSeq.current;
    try {
      const eps = await api.getShowEpisodes(show.tvmazeId);
      if (mine === loadSeq.current) setEpisodes(eps);
    } catch (e) {
      if (mine === loadSeq.current) toast(String(e), "bad");
    }
  }, [show.tvmazeId, toast]);

  useEffect(() => {
    void load();
  }, [load]);

  // onClose is an inline closure recreated on every parent render — keep it
  // in a ref so the listener isn't torn down and rebuilt each store write
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onCloseRef.current();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const seasons = useMemo(() => {
    const by = new Map<number, EpisodeRow[]>();
    for (const e of episodes ?? []) {
      if (!by.has(e.season)) by.set(e.season, []);
      by.get(e.season)!.push(e);
    }
    return [...by.entries()].sort((a, b) => b[0] - a[0]); // newest season first
  }, [episodes]);

  // newest season open by default
  useEffect(() => {
    if (seasons.length && Object.keys(openSeasons).length === 0) {
      setOpenSeasons({ [seasons[0][0]]: true });
    }
  }, [seasons, openSeasons]);

  const refresh = async () => {
    setRefreshing(true);
    try {
      await api.refreshShow(show.tvmazeId);
      await load();
      await loadShows();
      toast(`${show.name} refreshed from TVmaze`, "ok");
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setRefreshing(false);
    }
  };

  const setState = async (ep: EpisodeRow, state: EpisodeState) => {
    try {
      await api.setEpisodeState(ep.tvmazeEpId, state);
      setEpisodes((eps) =>
        (eps ?? []).map((e) => (e.tvmazeEpId === ep.tvmazeEpId ? { ...e, state } : e)),
      );
      void loadShows();
    } catch (e) {
      toast(String(e), "bad");
    }
  };

  const [seasonBusy, setSeasonBusy] = useState<string | null>(null);
  const setSeasonState = async (season: number, eps: EpisodeRow[], state: EpisodeState) => {
    const key = `${season}:${state}`;
    if (seasonBusy) return;
    setSeasonBusy(key);
    // never rewrite a downloaded episode back to wanted — that would silently
    // queue a re-download of something the user already has
    const targets = eps.filter(
      (e) => e.state !== "upcoming" && e.state !== state && !(state === "wanted" && e.state === "downloaded"),
    );
    let failed = 0;
    for (const e of targets) {
      try {
        await api.setEpisodeState(e.tvmazeEpId, state);
      } catch {
        failed++;
      }
    }
    await load();
    void loadShows();
    setSeasonBusy(null);
    if (failed > 0) {
      toast(`${failed} of ${targets.length} episodes couldn't be updated`, "bad");
    } else if (targets.length === 0) {
      toast("Nothing to change in that season", "info");
    }
  };

  return (
    <div
      className="fixed inset-0 z-40 flex justify-end bg-black/50 backdrop-blur-[2px]"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <div className="slide-in-right flex h-full w-[520px] max-w-[94vw] flex-col border-l border-line2 bg-bg1 shadow-[-24px_0_80px_-20px_rgba(0,0,0,0.9)]">
        {/* header */}
        <div className="relative overflow-hidden border-b border-line">
          {show.posterUrl && (
            <img
              src={show.posterUrl}
              alt=""
              className="absolute inset-0 size-full scale-110 object-cover opacity-[0.13] blur-xl"
            />
          )}
          <div className="relative flex items-start gap-3.5 p-4">
            <div className="h-[96px] w-[64px] shrink-0 overflow-hidden rounded-lg bg-bg3 shadow-lg">
              {show.posterUrl && <img src={show.posterUrl} alt="" className="size-full object-cover" />}
            </div>
            <div className="min-w-0 flex-1 pt-0.5">
              <div className="text-[16px] font-semibold leading-tight">{show.name}</div>
              <div className="mt-1 flex flex-wrap items-center gap-1.5 text-[11.5px] text-faint">
                <StatusPill status={show.status} />
                {show.network && <span>{show.network}</span>}
                <span>
                  {show.premiered?.slice(0, 4)}
                  {show.ended ? `–${show.ended.slice(0, 4)}` : show.status === "Running" ? "–" : ""}
                </span>
                {show.imdbId && (
                  <a
                    href={`https://www.imdb.com/title/${show.imdbId}/`}
                    target="_blank"
                    rel="noreferrer"
                    className="inline-flex items-center gap-0.5 text-faint hover:text-dim"
                  >
                    IMDb <ExternalLink size={10} />
                  </a>
                )}
              </div>
              <div className="mt-2 text-[11.5px] text-dim">
                <span className="text-ok">{show.downloaded}</span> downloaded ·{" "}
                <span className="text-accent">{show.grabbed}</span> grabbing ·{" "}
                <span className="text-warn">{show.wanted}</span> wanted
              </div>
            </div>
            <CloseBtn onClick={onClose} />
          </div>
          <div className="relative flex items-center gap-2 px-4 pb-3">
            <Button onClick={refresh} busy={refreshing} className="px-2.5 py-1 text-[11.5px]">
              <RefreshCw size={12} /> Refresh
            </Button>
            <Button
              onClick={runPreview}
              busy={preview === "loading"}
              className="px-2.5 py-1 text-[11.5px]"
              title="Dry run: what would the scheduler grab for this show right now?"
            >
              <FlaskConical size={12} /> Preview grabs
            </Button>
            {confirmUnfollow ? (
              <>
                <Button
                  variant="danger"
                  onClick={() => unfollowShow(show.tvmazeId)}
                  className="px-2.5 py-1 text-[11.5px]"
                >
                  Really unfollow
                </Button>
                <Button variant="ghost" onClick={() => setConfirmUnfollow(false)} className="px-2 py-1 text-[11.5px]">
                  Keep
                </Button>
              </>
            ) : (
              <Button
                variant="ghost"
                onClick={() => setConfirmUnfollow(true)}
                className="px-2.5 py-1 text-[11.5px] hover:text-bad"
              >
                <Trash2 size={12} /> Unfollow
              </Button>
            )}
            <QualitySelect show={show} />
          </div>
        </div>

        {/* dry-run result */}
        {preview && preview !== "loading" && (
          <div className="border-b border-line bg-bg2/40 px-4 py-3">
            <div className="mb-1.5 flex items-center justify-between">
              <span className="text-[11.5px] font-semibold text-dim">
                Next cycle would grab {preview.length === 0 ? "nothing" : `${preview.length} release${preview.length === 1 ? "" : "s"}`}
              </span>
              <button
                type="button"
                onClick={() => setPreview(null)}
                className="cursor-pointer text-faint hover:text-dim"
              >
                <X size={12} />
              </button>
            </div>
            {preview.length === 0 ? (
              <div className="text-[11.5px] text-faint">
                Nothing wanted has a matching release right now — missing episodes are retried
                automatically on the schedule.
              </div>
            ) : (
              <div className="space-y-1.5">
                {preview.map((p, i) => (
                  <div key={i} className="flex items-center gap-2 text-[11.5px]">
                    {p.isPack ? (
                      <Package size={12} className="shrink-0 text-accent" />
                    ) : (
                      <span className="size-[6px] shrink-0 rounded-full bg-accent" />
                    )}
                    <span className="shrink-0 font-mono text-dim">
                      {p.isPack
                        ? `S${String(p.season).padStart(2, "0")} pack (${p.episodes.length} eps)`
                        : `S${String(p.season).padStart(2, "0")}E${String(p.episodes[0]).padStart(2, "0")}`}
                    </span>
                    <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-faint" title={p.title}>
                      {p.title}
                    </span>
                    <span className="shrink-0 text-faint">{(p.size / 1e9).toFixed(1)} GB</span>
                    <span className="shrink-0 text-faint">{p.seeders ?? "?"}s</span>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}

        {/* seasons */}
        <div className="min-h-0 flex-1 overflow-y-auto p-3">
          {seasons.map(([num, eps]) => {
            const open = openSeasons[num] ?? false;
            const dl = eps.filter((e) => e.state === "downloaded").length;
            const aired = eps.filter((e) => e.state !== "upcoming").length;
            return (
              <div key={num} className="mb-2 overflow-hidden rounded-(--radius-card) border border-line bg-bg2/40">
                <div className="flex w-full items-center gap-2 pr-2 hover:bg-bg2">
                  <button
                    type="button"
                    onClick={() => setOpenSeasons((o) => ({ ...o, [num]: !open }))}
                    className="flex min-w-0 flex-1 cursor-pointer items-center gap-2 px-3 py-2.5 text-left"
                  >
                    <ChevronDown
                      size={14}
                      className={cx("shrink-0 text-faint transition-transform", !open && "-rotate-90")}
                    />
                    <span className="text-[12.5px] font-semibold">Season {num}</span>
                    <span className="text-[11px] text-faint">
                      {dl}/{aired} downloaded · {eps.length} eps
                    </span>
                  </button>
                  <span className="flex shrink-0 gap-1">
                    <SeasonAction
                      label="Want all"
                      busy={seasonBusy === `${num}:wanted`}
                      onClick={() => void setSeasonState(num, eps, "wanted")}
                    />
                    <SeasonAction
                      label="Ignore all"
                      busy={seasonBusy === `${num}:ignored`}
                      onClick={() => void setSeasonState(num, eps, "ignored")}
                    />
                  </span>
                </div>
                {open && (
                  <div className="border-t border-line/60">
                    {eps.map((ep) => (
                      <EpisodeLine
                        key={ep.tvmazeEpId}
                        ep={ep}
                        onFind={() => findReleaseForEpisode(show.name, ep, eps)}
                        onSetState={(s) => setState(ep, s)}
                      />
                    ))}
                  </div>
                )}
              </div>
            );
          })}
          {episodes && episodes.length === 0 && (
            <div className="py-10 text-center text-[12px] text-faint">
              No regular episodes registered on TVmaze yet.
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

/** Per-show quality override: App default, a named preset, or an existing
 *  custom profile. Writes quality_json through set_show_options. */
function QualitySelect({ show }: { show: ShowRow }) {
  const config = useStore((s) => s.config);
  const loadShows = useStore((s) => s.loadShows);
  const toast = useStore((s) => s.toast);

  const presets = config?.qualityPresets ?? [];
  const current: QualityProfile | null = (() => {
    if (!show.qualityJson) return null;
    try {
      return JSON.parse(show.qualityJson) as QualityProfile;
    } catch {
      return null;
    }
  })();
  const matched = current ? presets.find((p) => sameProfile(p.profile, current)) : undefined;
  const value = current === null ? "" : (matched?.name ?? "__custom");
  const [selected, setSelected] = useState(value);
  const [saving, setSaving] = useState(false);
  const savingRef = useRef(false);

  useEffect(() => {
    if (!savingRef.current) setSelected(value);
  }, [value]);

  const apply = async (v: string) => {
    if (savingRef.current) return;
    savingRef.current = true;
    setSaving(true);
    setSelected(v);
    const preset = presets.find((p) => p.name === v);
    const json = v === "" ? null : preset ? JSON.stringify(preset.profile) : show.qualityJson;
    try {
      await api.setShowOptions(show.tvmazeId, json, show.savePathOverride ?? null);
      await loadShows();
      toast(
        json === null
          ? `${show.name} follows the app default quality`
          : `${show.name} now grabs “${v}” quality`,
        "ok",
      );
    } catch (e) {
      setSelected(value);
      toast(String(e), "bad");
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  };

  return (
    <label className="ml-auto flex items-center gap-1.5 text-[11px] text-faint" title="Quality profile used when grabbing this show">
      Quality
      <select
        value={selected}
        disabled={saving}
        onChange={(e) => void apply(e.target.value)}
        className="cursor-pointer rounded-(--radius-btn) border border-line2 bg-bg1 px-1.5 py-1 text-[11.5px] text-dim focus:border-accent/50"
      >
        <option value="">App default</option>
        {presets.map((p) => (
          <option key={p.name} value={p.name}>{p.name}</option>
        ))}
        {selected === "__custom" && <option value="__custom">Custom</option>}
      </select>
    </label>
  );
}

function SeasonAction({
  label,
  onClick,
  busy,
}: {
  label: string;
  onClick: () => void;
  busy?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={busy}
      className="cursor-pointer rounded-md bg-bg3 px-1.5 py-0.5 text-[10.5px] text-dim transition-colors hover:bg-bg4 hover:text-ink active:scale-[0.97] disabled:pointer-events-none disabled:opacity-50"
    >
      {busy ? "…" : label}
    </button>
  );
}

const STATE_DOT: Record<EpisodeState, string> = {
  downloaded: "bg-ok",
  grabbed: "bg-accent",
  wanted: "bg-warn",
  upcoming: "bg-bg4",
  ignored: "bg-bg4",
};

const STATE_LABEL: Record<EpisodeState, string> = {
  downloaded: "Downloaded",
  grabbed: "Grabbing",
  wanted: "Wanted",
  upcoming: "Not aired yet",
  ignored: "Ignored",
};

function EpisodeLine({
  ep,
  onFind,
  onSetState,
}: {
  ep: EpisodeRow;
  onFind: () => void;
  onSetState: (s: EpisodeState) => void;
}) {
  const code = `S${String(ep.season).padStart(2, "0")}E${String(ep.number).padStart(2, "0")}`;
  const airText = ep.airstamp ? ep.airstamp.slice(0, 10) : "TBA";

  return (
    <div
      className={cx(
        "group flex items-center gap-2.5 px-3 py-1.5 hover:bg-bg2/70",
        ep.state === "ignored" && "opacity-45",
      )}
    >
      <span
        title={STATE_LABEL[ep.state] + (ep.grabbedTitle ? ` — ${ep.grabbedTitle}` : "")}
        className={cx("size-[7px] shrink-0 rounded-full", STATE_DOT[ep.state])}
      />
      <span className="w-[52px] shrink-0 font-mono text-[11.5px] text-dim">{code}</span>
      <span className="min-w-0 flex-1 truncate text-[12px]" title={ep.title ?? undefined}>
        {ep.title ?? "—"}
      </span>
      <span className="shrink-0 font-mono text-[10.5px] text-faint">{airText}</span>
      <span className="flex shrink-0 items-center gap-0.5 opacity-0 transition-opacity group-focus-within:opacity-100 group-hover:opacity-100">
        {ep.state !== "upcoming" && (
          <>
            <IconBtn title="Find a release" onClick={onFind}>
              <SearchIcon size={12.5} />
            </IconBtn>
            {ep.state === "ignored" ? (
              <IconBtn title="Mark as wanted" onClick={() => onSetState("wanted")}>
                <Undo2 size={12.5} />
              </IconBtn>
            ) : (
              <IconBtn title="Ignore" onClick={() => onSetState("ignored")}>
                <Ban size={12.5} />
              </IconBtn>
            )}
          </>
        )}
      </span>
    </div>
  );
}
