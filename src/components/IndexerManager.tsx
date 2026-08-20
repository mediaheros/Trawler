import { useCallback, useEffect, useMemo, useState } from "react";
import { ChevronDown, Loader2, Plus, RefreshCw, ShieldCheck, Trash2 } from "lucide-react";
import { api, onSetupStep, type FlaresolverrStatus, type Indexer, type IndexerDef } from "../lib/api";
import { useStore } from "../store";
import { isMac } from "../lib/platform";
import { Badge, Button, IconBtn, TextInput, cx } from "./ui";

/** Verified against Prowlarr's catalog this session — healthy public defs. */
const CURATED = [
  "TorrentDownloads",
  "Torrent[CORE]",
  "TorrentsCSV",
  "Uindex",
  "kickasstorrents.ws",
  "MagnetDownload",
  "TorrentProject2",
];
const CURATED_ANIME = ["Nyaa.si", "SubsPlease", "Anidex"];

export default function IndexerManager() {
  const toast = useStore((s) => s.toast);
  const refreshStatus = useStore((s) => s.refreshStatus);
  const [installed, setInstalled] = useState<Indexer[] | null>(null);
  const [defs, setDefs] = useState<IndexerDef[] | null>(null);
  const [browse, setBrowse] = useState(false);
  const [filter, setFilter] = useState("");
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setInstalled(await api.listIndexers());
    } catch (e) {
      setInstalled([]);
      toast(String(e), "bad");
    }
  }, [toast]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (browse && defs === null) {
      api.indexerDefs().then(setDefs).catch((e) => {
        toast(String(e), "bad");
        setDefs([]);
      });
    }
  }, [browse, defs, toast]);

  const installedNames = useMemo(
    () => new Set((installed ?? []).map((i) => i.name)),
    [installed],
  );

  const add = async (name: string) => {
    setBusy(name);
    try {
      const added = await api.addIndexer(name);
      toast(`Added ${added}`, "ok");
      await load();
      void refreshStatus();
    } catch (e) {
      const msg = String(e);
      toast(
        msg.toLowerCase().includes("cloudflare")
          ? `${name} is Cloudflare-protected — use the unlock card below to make it work`
          : msg,
        "bad",
      );
    } finally {
      setBusy(null);
    }
  };

  const toggle = async (ix: Indexer) => {
    try {
      await api.toggleIndexer(ix.id, !ix.enable);
      setInstalled((list) =>
        (list ?? []).map((i) => (i.id === ix.id ? { ...i, enable: !ix.enable } : i)),
      );
      toast(
        ix.enable ? `${ix.name} disabled — it won't be searched` : `${ix.name} enabled`,
        "info",
      );
    } catch (e) {
      toast(String(e), "bad");
    }
  };

  const remove = async (ix: Indexer) => {
    try {
      await api.removeIndexer(ix.id);
      toast(`Removed ${ix.name}`, "info");
      await load();
    } catch (e) {
      toast(String(e), "bad");
    }
  };

  const filteredDefs = useMemo(() => {
    if (!defs) return [];
    const f = filter.toLowerCase();
    return defs.filter(
      (d) => !installedNames.has(d.name) && (!f || d.name.toLowerCase().includes(f) || d.description.toLowerCase().includes(f)),
    );
  }, [defs, filter, installedNames]);

  return (
    <div>
      {/* installed */}
      {installed === null ? (
        <div className="py-3 text-[12px] text-faint">Loading indexers…</div>
      ) : installed.length === 0 ? (
        <div className="py-2 text-[12px] text-faint">No indexers configured in Prowlarr yet — add some below.</div>
      ) : (
        <div className="overflow-hidden rounded-(--radius-btn) border border-line">
          {installed.map((ix, i) => (
            <div
              key={ix.id}
              className={cx(
                "group flex items-center gap-2.5 px-3 py-2",
                i > 0 && "border-t border-line/60",
                !ix.enable && "opacity-55",
              )}
            >
              <label className="flex cursor-pointer items-center gap-2.5">
                <input
                  type="checkbox"
                  checked={ix.enable}
                  onChange={() => toggle(ix)}
                  className="size-3.5 accent-(--color-accent)"
                  title={ix.enable ? "Enabled — searched every query" : "Disabled"}
                />
                <span className="text-[12.5px] font-medium">{ix.name}</span>
              </label>
              <Badge tone={ix.privacy === "private" ? "kind" : "plain"} mono={false}>
                {ix.privacy ?? "public"}
              </Badge>
              <IconBtn
                title="Remove from Prowlarr"
                onClick={() => remove(ix)}
                danger
                reveal
                className="ml-auto"
              >
                <Trash2 size={13} />
              </IconBtn>
            </div>
          ))}
        </div>
      )}

      {/* curated quick-add */}
      <div className="mt-3">
        <div className="mb-1.5 text-[11.5px] font-medium text-dim">Quick add — reliable public indexers</div>
        <div className="flex flex-wrap gap-1.5">
          {[...CURATED, ...CURATED_ANIME].map((name) => {
            const have = installedNames.has(name);
            const anime = CURATED_ANIME.includes(name);
            return (
              <button
                key={name}
                type="button"
                disabled={have || busy === name}
                onClick={() => add(name)}
                className={cx(
                  "inline-flex cursor-pointer items-center gap-1 rounded-lg border px-2 py-1 text-[11.5px] transition-colors",
                  have
                    ? "border-line bg-bg2 text-faint opacity-50"
                    : "border-line2 bg-bg1 text-dim hover:border-accent/40 hover:text-ink",
                )}
                title={anime ? "Anime-focused" : undefined}
              >
                {busy === name ? <Loader2 size={11} className="spin" /> : <Plus size={11} />}
                {name}
                {anime && <span className="text-[9.5px] text-accent2">anime</span>}
              </button>
            );
          })}
        </div>
      </div>

      <FlaresolverrCard onUnlocked={() => void load()} />

      {/* browse all */}
      <button
        type="button"
        onClick={() => setBrowse(!browse)}
        className="mt-3 flex cursor-pointer items-center gap-1 text-[11.5px] font-medium text-dim hover:text-ink"
      >
        <ChevronDown size={13} className={cx("transition-transform", !browse && "-rotate-90")} />
        Browse the full catalog ({defs ? `${filteredDefs.length} available` : "…"})
      </button>
      {browse && (
        <div className="mt-2">
          <div className="mb-2">
            <TextInput value={filter} onChange={setFilter} placeholder="Filter definitions…" />
          </div>
          <div className="max-h-[220px] overflow-y-auto rounded-(--radius-btn) border border-line">
            {defs === null ? (
              <div className="flex items-center gap-2 px-3 py-3 text-[12px] text-faint">
                <RefreshCw size={12} className="spin" /> Fetching catalog…
              </div>
            ) : filteredDefs.length === 0 ? (
              <div className="px-3 py-3 text-[12px] text-faint">
                No definitions match "{filter}"
              </div>
            ) : (
              filteredDefs.map((d, i) => (
                <div
                  key={d.name}
                  className={cx("flex items-center gap-2 px-3 py-1.5", i > 0 && "border-t border-line/50")}
                >
                  <div className="min-w-0 flex-1">
                    <span className="text-[12px] font-medium">{d.name}</span>
                    <span className="ml-2 truncate text-[11px] text-faint">{d.description}</span>
                  </div>
                  <Button
                    busy={busy === d.name}
                    onClick={() => add(d.name)}
                    className="shrink-0 px-2 py-0.5 text-[11px]"
                  >
                    Add
                  </Button>
                </div>
              ))
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/** Opt-in Cloudflare unlock. The stack never needs this to run — it only
 *  adds 1337x/EZTV/ExtraTorrent for users who ask, via a managed FlareSolverr. */
function FlaresolverrCard({ onUnlocked }: { onUnlocked: () => void }) {
  const toast = useStore((st) => st.toast);
  const [status, setStatus] = useState<FlaresolverrStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [log, setLog] = useState<string | null>(null);
  const [pct, setPct] = useState<number | null>(null);
  const [confirmOff, setConfirmOff] = useState(false);

  useEffect(() => {
    void api.flaresolverrStatus().then(setStatus).catch(() => setStatus(null));
  }, []);

  // subscribe once on mount — Tauri's listen() registration is async, so a
  // busy-keyed subscription misses the first "Finding the latest release…"
  useEffect(() => {
    return onSetupStep((step) => {
      if (step.component !== "flaresolverr") return;
      if (step.kind === "log" && step.payload.message) {
        setLog(step.payload.message);
        setPct(null);
      }
      if (step.kind === "progress" && step.payload.pct !== undefined) setPct(step.payload.pct);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const unlock = async () => {
    setBusy(true);
    setLog("Starting…");
    setPct(null);
    try {
      const added = await api.setupFlaresolverr();
      setStatus({ installed: true, running: true, proxied: true });
      toast(
        added.length
          ? `Cloudflare indexers unlocked: ${added.join(", ")}`
          : "FlareSolverr is set up — protected indexers will now work",
        "ok",
      );
      onUnlocked();
    } catch (e) {
      toast(String(e), "bad");
      void api.flaresolverrStatus().then(setStatus).catch(() => {});
    } finally {
      setBusy(false);
      setLog(null);
      setPct(null);
    }
  };

  if (status?.proxied) {
    return (
      <div className="mt-2 flex items-center gap-1.5 text-[11px] text-faint">
        <ShieldCheck size={12} className="text-ok" />
        FlareSolverr is active — Cloudflare-protected indexers (1337x, EZTV…) work here.
        <button
          type="button"
          className="ml-1 cursor-pointer text-faint underline decoration-line2 underline-offset-2 hover:text-bad"
          onClick={async () => {
            if (!confirmOff) {
              setConfirmOff(true);
              window.setTimeout(() => setConfirmOff(false), 3000);
              return;
            }
            setConfirmOff(false);
            try {
              await api.disableFlaresolverr();
              setStatus({ installed: false, running: false, proxied: false });
              toast("FlareSolverr removed — protected indexers will stop working", "info");
            } catch (e) {
              toast(String(e), "bad");
            }
          }}
        >
          {confirmOff ? "really turn off?" : "turn off"}
        </button>
      </div>
    );
  }

  return (
    <div className="mt-3 rounded-(--radius-btn) border border-line bg-bg2/40 p-3">
      <div className="flex items-center gap-2">
        <ShieldCheck size={14} className="shrink-0 text-dim" />
        <div className="min-w-0 flex-1">
          <div className="text-[12px] font-medium">Some indexers sit behind Cloudflare</div>
          <div className="text-[11px] leading-snug text-faint">
            {isMac
              ? "1337x, EZTV and friends block server requests. Optional fix: Trawler can run FlareSolverr for you via Docker (needs Docker Desktop) and route protected indexers through it. Everything else works fine without this."
              : "1337x, EZTV and friends block server requests. Optional fix: Trawler can run FlareSolverr for you (~350 MB download — it bundles a browser) and route protected indexers through it. Everything else works fine without this."}
          </div>
        </div>
        <Button onClick={() => void unlock()} busy={busy} className="shrink-0 px-2.5 py-1.5 text-[11.5px]">
          {status?.installed ? "Finish setup" : "Unlock them"}
        </Button>
      </div>
      {busy && (
        <div className="mt-2">
          <div className="text-[11px] text-dim">{pct !== null ? `${log ?? "Downloading…"} ${pct}%` : log}</div>
          <div className="mt-1 h-[3px] overflow-hidden rounded-full bg-bg3">
            <div
              className={pct === null ? "progress-indeterminate h-full rounded-full bg-accent" : "h-full rounded-full bg-accent transition-[width] duration-300"}
              style={pct === null ? undefined : { width: `${pct}%` }}
            />
          </div>
        </div>
      )}
    </div>
  );
}
