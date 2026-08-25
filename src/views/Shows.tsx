import { useEffect, useState } from "react";
import { Activity, ClipboardList, Plus, RadioTower, Tv } from "lucide-react";
import { useStore } from "../store";
import { api, type ShowRow } from "../lib/api";
import { Button, CenterMessage, Segmented, StatusPill, cx } from "../components/ui";
export { statusLabel } from "../components/ui";
import AddShow from "../components/AddShow";
import ImportWatchlist from "../components/ImportWatchlist";
import ShowDetail from "../components/ShowDetail";
import CalendarView from "../components/CalendarView";
import DiscoverView from "../components/DiscoverView";

type ShowsMode = "library" | "calendar" | "discover";

export default function ShowsView() {
  // per-field selectors: a whole-store subscription re-renders this tree on
  // every keystroke and toast (AddShow documents the same trap)
  const shows = useStore((s) => s.shows);
  const showsLoaded = useStore((s) => s.showsLoaded);
  const loadShows = useStore((s) => s.loadShows);
  const selectedShow = useStore((s) => s.selectedShow);
  const selectShow = useStore((s) => s.selectShow);
  const activity = useStore((s) => s.activity);
  const loadActivity = useStore((s) => s.loadActivity);
  const toast = useStore((s) => s.toast);
  const [adding, setAdding] = useState(false);
  const [importing, setImporting] = useState(false);
  const [showActivity, setShowActivity] = useState(false);
  const [checking, setChecking] = useState(false);
  const [mode, setMode] = useState<ShowsMode>("library");

  const checkNow = async () => {
    setChecking(true);
    try {
      await api.runSchedulerNow();
      toast("Scheduler cycle started — watch the activity log", "ok");
      setShowActivity(true);
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setTimeout(() => setChecking(false), 3000);
    }
  };

  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const poll = async () => {
      await Promise.all([loadShows(), loadActivity()]);
      if (active) timer = setTimeout(poll, 15_000);
    };
    void poll();
    return () => {
      active = false;
      if (timer !== null) clearTimeout(timer);
    };
  }, [loadShows, loadActivity]);

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center justify-between px-6 pt-5 pb-4">
        <div className="flex items-center gap-4">
          <h1 className="text-[16px] font-semibold tracking-tight">Shows</h1>
          <Segmented<ShowsMode>
            value={mode}
            onChange={setMode}
            options={[
              { value: "library", label: "Library" },
              { value: "calendar", label: "Calendar" },
              { value: "discover", label: "Discover" },
            ]}
          />
        </div>
        {mode === "library" && (
          <div className="flex items-center gap-2">
            <Button
              variant="ghost"
              onClick={() => setShowActivity(!showActivity)}
              className={cx("px-2.5", showActivity && "bg-bg3 text-ink")}
              title="Activity log"
            >
              <Activity size={14} />
            </Button>
            <Button onClick={checkNow} busy={checking} title="Run a scheduler cycle right now">
              <RadioTower size={13} /> Check now
            </Button>
            <Button onClick={() => setImporting(true)} title="Follow many shows at once — paste names, an IMDb list URL, or a CSV">
              <ClipboardList size={13} /> Import
            </Button>
            <Button variant="primary" onClick={() => setAdding(true)}>
              <Plus size={14} /> Add show
            </Button>
          </div>
        )}
      </div>

      {mode === "calendar" && <CalendarView />}
      {mode === "discover" && <DiscoverView />}

      {mode === "library" && (
      <div className="flex min-h-0 flex-1">
        <div className="min-h-0 flex-1 overflow-y-auto px-6 pb-6">
          {!showsLoaded ? (
            <div className="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-4">
              {Array.from({ length: 8 }).map((_, i) => (
                <div key={i} className="skeleton aspect-[2/3] rounded-(--radius-card)" />
              ))}
            </div>
          ) : shows.length === 0 ? (
            <CenterMessage
              icon={<Tv size={28} />}
              title="No shows followed yet"
              body={
                <>
                  Add a show and Trawler will fetch its episode list, download the back
                  catalog, and keep watching for new episodes until the show ends.
                </>
              }
            />
          ) : (
            <div className="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-4">
              {shows.map((s) => (
                <ShowCard key={s.tvmazeId} s={s} onClick={() => selectShow(s)} />
              ))}
            </div>
          )}
        </div>

        {showActivity && (
          <div className="slide-in-right flex w-[340px] shrink-0 flex-col border-l border-line bg-bg1/60">
            <div className="flex items-center gap-2 border-b border-line px-4 py-3">
              <Activity size={14} className="text-accent" />
              <span className="text-[13px] font-semibold">Activity</span>
            </div>
            <div className="min-h-0 flex-1 overflow-y-auto p-3">
              {activity.length === 0 ? (
                <div className="px-1 py-4 text-[11.5px] text-faint">Nothing yet.</div>
              ) : (
                <div className="space-y-2.5">
                  {activity.map((a) => (
                    <div key={a.id} className="px-1 text-[11.5px] leading-snug">
                      <div className="text-faint">{timeAgo(a.ts)}</div>
                      <div className={cx("select-text text-dim", a.kind === "error" && "text-bad", a.kind === "complete" && "text-ok")}>
                        {a.message}
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </div>
        )}
      </div>
      )}

      {adding && <AddShow onClose={() => setAdding(false)} />}
      {importing && <ImportWatchlist onClose={() => setImporting(false)} />}
      {selectedShow && <ShowDetail show={selectedShow} onClose={() => selectShow(null)} />}
    </div>
  );
}

function timeAgo(ts: number): string {
  const s = Date.now() / 1000 - ts;
  if (s < 90) return "just now";
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

function ShowCard({ s, onClick }: { s: ShowRow; onClick: () => void }) {
  const done = s.total > 0 ? s.downloaded / s.total : 0;
  const next = s.nextAirstamp ? nextAirText(s.nextAirstamp) : null;

  return (
    <button
      type="button"
      onClick={onClick}
      className="group cursor-pointer overflow-hidden rounded-(--radius-card) border border-line bg-bg1 text-left transition-all duration-150 hover:border-line2 hover:shadow-[0_8px_30px_-12px_rgba(0,0,0,0.8)]"
    >
      <div className="relative aspect-[2/3] w-full overflow-hidden bg-bg2">
        {s.posterUrl ? (
          <img
            src={s.posterUrl}
            alt=""
            loading="lazy"
            className="size-full object-cover transition-transform duration-300 group-hover:scale-[1.03]"
          />
        ) : (
          <div className="flex size-full items-center justify-center text-faint">
            <Tv size={28} />
          </div>
        )}
        <StatusPill status={s.status} className="absolute left-2 top-2 backdrop-blur-sm" />
        {s.wanted > 0 && (
          <span className="absolute right-2 top-2 rounded-md bg-warn/90 px-1.5 py-0.5 text-[10px] font-bold text-bg0">
            {s.wanted}
          </span>
        )}
        <div className="absolute inset-x-0 bottom-0 h-16 bg-gradient-to-t from-black/85 to-transparent" />
        <div className="absolute inset-x-0 bottom-0 p-2.5">
          <div className="truncate text-[12.5px] font-semibold text-white">{s.name}</div>
          <div className="text-[10.5px] text-white/60">
            {s.downloaded}/{s.total} eps
            {next && <> · {next}</>}
          </div>
        </div>
      </div>
      <div className="h-[3px] bg-bg3">
        <div
          className={cx("h-full", done >= 1 ? "bg-ok/80" : "bg-gradient-to-r from-accent to-accent2")}
          style={{ width: `${Math.min(100, done * 100)}%` }}
        />
      </div>
    </button>
  );
}

function nextAirText(iso: string): string | null {
  const d = new Date(iso).getTime() - Date.now();
  if (!Number.isFinite(d)) return null;
  const days = d / 8.64e7;
  if (days < 0) return null;
  if (days < 1) return "new today";
  if (days < 2) return "new tomorrow";
  return `next in ${Math.round(days)}d`;
}
