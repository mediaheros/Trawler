import { useCallback, useEffect, useRef, useState } from "react";
import { Check, Loader2, Plus, RefreshCw, Tv, Search as SearchIcon, X } from "lucide-react";
import { api, type DiscoverEntry, type DiscoverRows, type TvmazeResult } from "../lib/api";
import { useStore } from "../store";
import { Button, cx } from "./ui";
import ShowPreview from "./ShowPreview";

export default function DiscoverView() {
  const [rows, setRows] = useState<DiscoverRows | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [previewId, setPreviewId] = useState<number | null>(null);
  const closePreview = useCallback(() => setPreviewId(null), []);
  // search beyond the featured panes: any show TVmaze knows, not just
  // what happens to be airing this week
  const [query, setQuery] = useState("");
  const [found, setFound] = useState<TvmazeResult[] | "loading" | null>(null);
  const searchSeq = useRef(0);

  useEffect(() => {
    const q = query.trim();
    if (!q) {
      searchSeq.current++;
      setFound(null);
      return;
    }
    const mine = ++searchSeq.current;
    setFound("loading");
    const t = setTimeout(() => {
      api
        .searchTvmaze(q)
        .then((r) => {
          if (mine === searchSeq.current) setFound(r);
        })
        .catch(() => {
          if (mine === searchSeq.current) setFound([]);
        });
    }, 350);
    return () => clearTimeout(t);
  }, [query]);

  const load = useCallback((force = false) => {
    setError(null);
    if (force) setRows(null);
    api.discover(force).then(setRows).catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const empty =
    rows !== null && rows.tonight.length + rows.premieres.length + rows.popular.length === 0;

  if (error || empty) {
    return (
      <div className="flex flex-col items-center gap-3 px-6 py-14 text-center">
        <div className="text-[13px] font-semibold text-dim">
          {error ? "Couldn't load the schedule" : "TVmaze returned an empty schedule"}
        </div>
        {error && <div className="max-w-[400px] text-[11.5px] text-faint">{error}</div>}
        <Button onClick={() => load(true)} className="text-[12px]">
          <RefreshCw size={12} /> Try again
        </Button>
      </div>
    );
  }
  if (!rows) {
    return (
      <div className="space-y-8 px-6 pb-6">
        {[0, 1, 2].map((r) => (
          <div key={r}>
            <div className="skeleton mb-3 h-4 w-40 rounded" />
            <div className="flex gap-3 overflow-hidden">
              {Array.from({ length: 8 }).map((_, i) => (
                <div key={i} className="skeleton aspect-[2/3] w-[132px] shrink-0 rounded-xl" />
              ))}
            </div>
          </div>
        ))}
      </div>
    );
  }

  return (
    <div className="min-h-0 flex-1 space-y-7 overflow-y-auto px-6 pb-6">
      <DiscoverSearch query={query} setQuery={setQuery} />
      {found !== null ? (
        <SearchResults found={found} query={query} onPreview={setPreviewId} />
      ) : (
        <>
          <Row title="Airing tonight" sub="what the world is watching today" entries={rows.tonight} showTime onPreview={setPreviewId} />
          <Row title="New & returning" sub="premieres in the next seven days" entries={rows.premieres} onPreview={setPreviewId} />
          <Row title="Popular this week" sub="the biggest shows airing right now" entries={rows.popular} onPreview={setPreviewId} />
          <p className="pb-2 text-center text-[10.5px] text-faint">
            Schedule and artwork from TVmaze · refreshed every few hours
          </p>
        </>
      )}
      {previewId !== null && (
        <ShowPreview tvmazeId={previewId} onClose={closePreview} />
      )}
    </div>
  );
}

function DiscoverSearch({ query, setQuery }: { query: string; setQuery: (q: string) => void }) {
  return (
    <div className="relative max-w-[420px]">
      <SearchIcon size={14} className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-faint" />
      <input
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        placeholder="Find any show — airing or not…"
        className="w-full rounded-xl border border-line2 bg-bg1 py-2 pl-9 pr-8 text-[12.5px] text-ink placeholder:text-faint transition-colors focus:border-accent/50"
        spellCheck={false}
      />
      {query && (
        <button
          type="button"
          onClick={() => setQuery("")}
          className="absolute right-2 top-1/2 -translate-y-1/2 cursor-pointer rounded p-0.5 text-faint hover:text-ink"
          title="Back to featured"
        >
          <X size={13} />
        </button>
      )}
    </div>
  );
}

function SearchResults({
  found,
  query,
  onPreview,
}: {
  found: TvmazeResult[] | "loading";
  query: string;
  onPreview: (id: number) => void;
}) {
  if (found === "loading") {
    return (
      <div className="flex flex-wrap gap-3">
        {Array.from({ length: 6 }).map((_, i) => (
          <div key={i} className="skeleton aspect-[2/3] w-[132px] rounded-xl" />
        ))}
      </div>
    );
  }
  if (found.length === 0) {
    return (
      <div className="py-8 text-center text-[12px] text-faint">
        TVmaze doesn't know a show called "{query.trim()}" — try another spelling.
      </div>
    );
  }
  return (
    <div>
      <div className="mb-3 text-[11.5px] text-faint">
        {found.length} show{found.length === 1 ? "" : "s"} · click one to preview and follow
      </div>
      <div className="flex flex-wrap gap-3">
        {found.map((r) => (
          <button
            key={r.id}
            type="button"
            onClick={() => onPreview(r.id)}
            className="group w-[132px] cursor-pointer text-left"
          >
            <div className="relative aspect-[2/3] overflow-hidden rounded-xl border border-line bg-bg2 transition-all group-hover:border-accent/50 group-hover:shadow-[0_8px_30px_-10px_rgba(0,0,0,0.8)]">
              {r.poster ? (
                <img src={r.poster} alt="" loading="lazy" className="h-full w-full object-cover" />
              ) : (
                <div className="flex h-full items-center justify-center px-2 text-center text-[11px] text-faint">
                  {r.name}
                </div>
              )}
              {r.followed && (
                <span className="absolute right-1.5 top-1.5 rounded-md bg-bg0/80 px-1.5 py-0.5 text-[9.5px] font-semibold text-accent backdrop-blur">
                  Following
                </span>
              )}
            </div>
            <div className="mt-1.5 truncate text-[11.5px] font-medium text-ink">{r.name}</div>
            <div className="truncate text-[10.5px] text-faint">
              {r.premiered ? r.premiered.slice(0, 4) : ""}
              {r.network ? ` · ${r.network}` : ""}
            </div>
          </button>
        ))}
      </div>
    </div>
  );
}

function Row({
  title,
  sub,
  entries,
  showTime,
  onPreview,
}: {
  title: string;
  sub: string;
  entries: DiscoverEntry[];
  showTime?: boolean;
  onPreview: (id: number) => void;
}) {
  if (entries.length === 0) return null;
  return (
    <div>
      <div className="mb-2.5 flex items-baseline gap-2">
        <h2 className="text-[14px] font-semibold tracking-tight">{title}</h2>
        <span className="text-[11px] text-faint">{sub}</span>
      </div>
      <div className="flex gap-3 overflow-x-auto pb-2" style={{ scrollbarWidth: "thin" }}>
        {entries.map((e) => (
          <DiscoverCard key={e.showId} e={e} showTime={showTime} onPreview={onPreview} />
        ))}
      </div>
    </div>
  );
}

function DiscoverCard({ e, showTime, onPreview }: { e: DiscoverEntry; showTime?: boolean; onPreview: (id: number) => void }) {
  const followShow = useStore((s) => s.followShow);
  // derived, never cached: the store's shows list is the truth
  const inLibrary = useStore((s) => s.shows.some((x) => x.tvmazeId === e.showId));
  const followed = inLibrary || e.followed;
  const [busy, setBusy] = useState(false);
  const state: "idle" | "busy" | "done" = busy ? "busy" : followed ? "done" : "idle";

  const follow = async () => {
    if (state !== "idle") return;
    setBusy(true);
    try {
      // one-click = forward-only; the Add-show dialog is the place for backfill
      await followShow(e.showId, false);
    } catch {
      // store already toasted
    } finally {
      setBusy(false);
    }
  };

  const airtime = e.airstamp
    ? new Date(e.airstamp).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" })
    : null;

  return (
    <div className="group w-[132px] shrink-0">
      <div
        role="button"
        tabIndex={0}
        onClick={() => onPreview(e.showId)}
        onKeyDown={(ev) => {
          // only the card itself — Enter/Space on the follow button must not open the modal
          if (ev.target !== ev.currentTarget) return;
          if (ev.key === "Enter" || ev.key === " ") {
            ev.preventDefault();
            onPreview(e.showId);
          }
        }}
        title={`About ${e.name}`}
        className="relative aspect-[2/3] cursor-pointer overflow-hidden rounded-xl border border-line bg-bg2 transition-all duration-200 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--color-accent) group-hover:border-line2 group-hover:shadow-[0_10px_30px_-12px_rgba(0,0,0,0.8)]"
      >
        {e.poster ? (
          <img src={e.poster} alt="" loading="lazy" className="size-full object-cover transition-transform duration-300 group-hover:scale-[1.04]" />
        ) : (
          <div className="flex size-full items-center justify-center text-faint">
            <Tv size={22} />
          </div>
        )}
        {/* follow control */}
        <button
          type="button"
          onClick={(ev) => {
            ev.stopPropagation();
            void follow();
          }}
          title={state === "done" ? "Following" : "Follow this show"}
          aria-label={state === "done" ? `Following ${e.name}` : `Follow ${e.name}`}
          className={cx(
            "absolute right-1.5 top-1.5 flex size-7 cursor-pointer items-center justify-center rounded-lg backdrop-blur-md transition-all focus-visible:opacity-100 focus-visible:outline-2 focus-visible:outline-(--color-accent)",
            state === "done"
              ? "bg-ok/85 text-bg0"
              : "bg-bg0/70 text-ink opacity-0 hover:bg-accent hover:text-bg0 group-hover:opacity-100",
          )}
        >
          {state === "busy" ? <Loader2 size={13} className="spin" /> : state === "done" ? <Check size={14} /> : <Plus size={14} />}
        </button>
        <div className="absolute inset-x-0 bottom-0 h-14 bg-gradient-to-t from-black/85 to-transparent" />
        <div className="absolute inset-x-0 bottom-0 p-2">
          <div className="truncate text-[11.5px] font-semibold leading-tight text-white">{e.name}</div>
          <div className="truncate text-[9.5px] text-white/60">
            {showTime && airtime ? `${airtime} · ` : ""}
            {e.network ?? ""}
            {e.season != null && e.number != null ? ` · ${e.season}×${String(e.number).padStart(2, "0")}` : ""}
          </div>
        </div>
      </div>
    </div>
  );
}
