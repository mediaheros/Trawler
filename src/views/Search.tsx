import { useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowDown,
  ArrowUp,
  Check,
  ExternalLink,
  Loader2,
  Magnet,
  Search as SearchIcon,
  Waves,
  X,
} from "lucide-react";
import { api, type Release } from "../lib/api";
import { fmtAge, fmtBytes } from "../lib/format";
import { useStore, type SortKey } from "../store";
import { Badge, Button, CenterMessage, Chip, SeederDot, Segmented, cx } from "../components/ui";

export default function SearchView() {
  const {
    query, setQuery, kind, setKind, sort, setSort, sortThen, setSortThen,
    searching, hasSearched, results, searchError, runSearch,
    showHidden, setShowHidden, filters, setFilter,
  } = useStore();
  const toast = useStore((s) => s.toast);
  const [addingStarters, setAddingStarters] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  const addStarters = async () => {
    setAddingStarters(true);
    try {
      const names = await api.setupStarterIndexers();
      toast(
        names.length > 0
          ? `Added ${names.length} indexer${names.length === 1 ? "" : "s"} — searching again`
          : "No indexers could be added — check Settings \u2192 Connections",
        names.length > 0 ? "ok" : "bad",
      );
      if (names.length > 0) void runSearch();
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setAddingStarters(false);
    }
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        inputRef.current?.focus();
        inputRef.current?.select();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const { sorted, hiddenCount, filteredCount } = useMemo(() => {
    let rel = results?.releases ? [...results.releases] : [];
    const hiddenCount = rel.filter((r) => !r.relevant).length;
    if (!showHidden) rel = rel.filter((r) => r.relevant);
    const before = rel.length;
    if (filters.resolution) rel = rel.filter((r) => r.parsed.resolution === filters.resolution);
    if (filters.codec) rel = rel.filter((r) => r.parsed.codec === filters.codec);
    if (filters.source) rel = rel.filter((r) => r.parsed.source === filters.source);
    const filteredCount = before - rel.length;
    const raw = (key: SortKey, r: Release): number => {
      switch (key) {
        case "seeders": return r.seeders ?? 0;
        case "size": return r.size;
        case "age": return -ageDays(r);
        default: return r.score;
      }
    };
    // With a secondary sort, the primary is BANDED (seeder health bands,
    // rough size classes, whole days) — otherwise exact values almost never
    // tie and the secondary would never speak.
    const band = (key: SortKey, r: Release): number => {
      switch (key) {
        case "seeders": return Math.floor(Math.log(Math.max(0, r.seeders ?? 0) + 1) / Math.log(3));
        case "size": return Math.floor(Math.log2(r.size / 5e8 + 1));
        case "age": return -Math.floor(ageDays(r));
        default: return r.score;
      }
    };
    const secondary = sort !== "score" && sortThen && sortThen !== sort ? sortThen : null;
    rel.sort((a, b) => {
      if (secondary) {
        const p = band(sort, b) - band(sort, a);
        if (p !== 0) return p;
        return raw(secondary, b) - raw(secondary, a);
      }
      return raw(sort, b) - raw(sort, a);
    });
    return { sorted: rel, hiddenCount, filteredCount };
  }, [results, sort, sortThen, showHidden, filters]);

  const landing = !hasSearched;

  return (
    <div className="flex h-full flex-col">
      {/* search bar — centered hero on landing, docked after first search */}
      <div
        className={cx(
          "flex flex-col items-center transition-all duration-300",
          landing ? "flex-1 justify-center gap-6 pb-24" : "gap-3 px-6 pt-5 pb-4",
        )}
      >
        {landing && (
          <div className="flex flex-col items-center gap-3 rise-in">
            <div className="flex size-14 items-center justify-center rounded-2xl bg-gradient-to-br from-accent/25 to-accent2/15 shadow-[0_0_40px_-8px_var(--color-accent)]">
              <Waves size={26} className="text-accent" />
            </div>
            <div className="text-[21px] font-semibold tracking-tight">Trawler</div>
            <div className="-mt-2 text-[12.5px] text-faint">
              Search your indexers. Send to qBittorrent. Done.
            </div>
          </div>
        )}

        <div className={cx("flex w-full items-center gap-2.5", landing ? "max-w-[620px] px-6" : "")}>
          <div
            className={cx(
              "group relative flex-1 transition-shadow duration-200",
              landing && "shadow-[0_8px_40px_-12px_rgba(0,0,0,0.7)]",
              "focus-within:shadow-[0_0_28px_-10px_var(--color-accent)]",
            )}
          >
            <SearchIcon
              size={15}
              className="pointer-events-none absolute left-3.5 top-1/2 -translate-y-1/2 text-faint group-focus-within:text-accent transition-colors"
            />
            <input
              ref={inputRef}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => (e.key === "Enter" || e.key === "Return") && runSearch()}
              placeholder="Search movies and TV shows…   (Ctrl+K)"
              spellCheck={false}
              className={cx(
                "w-full rounded-xl border border-line2 bg-bg2 pl-9.5 pr-9 text-ink outline-none",
                "placeholder:text-faint focus:border-accent/50 focus:bg-bg2 transition-all",
                landing ? "py-3 text-[14px]" : "py-2 text-[13px]",
              )}
            />
            {query && (
              <button
                type="button"
                onClick={() => { setQuery(""); inputRef.current?.focus(); }}
                className="absolute right-3 top-1/2 -translate-y-1/2 cursor-pointer text-faint hover:text-dim"
              >
                <X size={14} />
              </button>
            )}
          </div>
          {/* stays clickable while searching — a new search supersedes the old one */}
          <Button variant="primary" onClick={runSearch} className="px-4 py-2">
            {searching && <Loader2 size={13} className="spin" />}
            Search
          </Button>
        </div>

        {landing && (
          <Segmented
            value={kind}
            onChange={setKind}
            options={[
              { value: "all", label: "All" },
              { value: "movies", label: "Movies" },
              { value: "tv", label: "TV" },
            ]}
          />
        )}

        {!landing && (
          <div className="flex w-full flex-wrap items-center gap-1">
            <Segmented
              value={kind}
              onChange={setKind}
              options={[
                { value: "all", label: "All" },
                { value: "movies", label: "Movies" },
                { value: "tv", label: "TV" },
              ]}
            />
            <span className="mx-1.5 h-3 w-px bg-line" />
            {(
              [
                ["resolution", ["2160p", "1080p", "720p"]],
                ["codec", ["x265", "x264"]],
                ["source", ["WEB-DL", "BluRay", "HDTV", "Remux"]],
              ] as const
            ).map(([dim, values], i) => (
              <span key={dim} className="flex items-center gap-1">
                {i >= 0 && <span className="mx-1.5 h-3 w-px bg-line" />}
                {values.map((v) => (
                  <Chip key={v} active={filters[dim] === v} onClick={() => setFilter(dim, v)}>
                    {v}
                  </Chip>
                ))}
              </span>
            ))}
            {filteredCount > 0 && (
              <span className="text-[10.5px] text-faint">{filteredCount} filtered out</span>
            )}
          </div>
        )}

        {!landing && results && !searching && (
          <div className="flex w-full items-center justify-between text-[11.5px] text-faint">
            <div className="flex items-center gap-1.5">
              <span>
                {sorted.length} release{sorted.length === 1 ? "" : "s"}
                {results.totalBeforeDedupe > results.releases.length &&
                  ` · ${results.totalBeforeDedupe - results.releases.length} duplicates folded`}
              </span>
              {hiddenCount > 0 && (
                <button
                  type="button"
                  onClick={() => setShowHidden(!showHidden)}
                  className="cursor-pointer rounded-md bg-bg2 px-1.5 py-0.5 text-warn/80 transition-colors hover:bg-bg3 hover:text-warn"
                  title="Release names that don't contain your search terms — usually indexer filler"
                >
                  {showHidden
                    ? "hide weak matches"
                    : `show ${hiddenCount} weak match${hiddenCount === 1 ? "" : "es"}`}
                </button>
              )}
              <span className="mx-1 h-3 w-px bg-line2" />
              {results.indexers.map((ix) => (
                <span
                  key={ix.id}
                  className={cx(
                    "rounded-md px-1.5 py-0.5 font-mono text-[10.5px]",
                    ix.ok ? "bg-bg2 text-faint" : "bg-warn/10 text-warn/90",
                  )}
                  title={
                    ix.ok
                      ? `${ix.name}: ${ix.count} results in ${(ix.elapsedMs / 1000).toFixed(1)}s`
                      : `${ix.name} ${ix.timedOut ? `gave up after ${Math.round(ix.elapsedMs / 1000)}s — slow indexer, results may return on a retry` : "returned an error"}`
                  }
                >
                  {shortName(ix.name)}{" "}
                  {ix.ok ? ix.count : ix.timedOut ? "· timed out" : "· error"}
                </span>
              ))}
            </div>
            <div className="flex items-center gap-1.5">
              <span className="mr-0.5">Sort</span>
              <Segmented<SortKey>
                value={sort}
                onChange={setSort}
                options={[
                  { value: "score", label: "Best" },
                  { value: "seeders", label: "Seeds" },
                  { value: "size", label: "Size" },
                  { value: "age", label: "Newest" },
                ]}
              />
              {sort !== "score" && (
                <span className="flex items-center gap-1" title="Break near-ties in the first sort with a second one — e.g. Seeds then Newest = the freshest of the well-seeded">
                  <span className="text-faint">then</span>
                  {(
                    [
                      ["seeders", "Seeds"],
                      ["size", "Size"],
                      ["age", "Newest"],
                    ] as const
                  )
                    .filter(([k]) => k !== sort)
                    .map(([k, label]) => (
                      <button
                        key={k}
                        type="button"
                        onClick={() => setSortThen(sortThen === k ? null : k)}
                        className={cx(
                          "cursor-pointer rounded-md px-1.5 py-0.5 text-[11px] transition-colors",
                          sortThen === k
                            ? "bg-accent-soft text-accent"
                            : "bg-bg2 text-faint hover:bg-bg3 hover:text-dim",
                        )}
                      >
                        {label}
                      </button>
                    ))}
                </span>
              )}
            </div>
          </div>
        )}
      </div>

      {/* results */}
      {!landing && (
        <div className="min-h-0 flex-1 overflow-y-auto px-6 pb-6">
          {searching ? (
            <SkeletonRows />
          ) : searchError ? (
            <CenterMessage
              icon={<X size={28} />}
              title="Search failed"
              body={searchError}
            />
          ) : results && results.indexers.length === 0 ? (
            <CenterMessage
              icon={<Waves size={28} />}
              title="No indexers to search yet"
              body={
                <>
                  Trawler searches through Prowlarr's indexers, and none are set up.
                  Add a reliable starter set with one click — or manage them under
                  Settings → Indexers.
                  <span className="mt-4 block">
                    <Button variant="primary" busy={addingStarters} onClick={() => void addStarters()}>
                      Add starter indexers
                    </Button>
                  </span>
                </>
              }
            />
          ) : sorted.length === 0 ? (
            <CenterMessage
              icon={<Waves size={28} />}
              title="Nothing surfaced"
              body="No releases matched. Try fewer words — indexers match release names, so 'expanse s02' beats full titles with punctuation."
            />
          ) : (
            <div className="overflow-hidden rounded-(--radius-card) border border-line bg-bg1">
              {sorted.map((r, i) => (
                <ResultRow key={(r.guid ?? r.title) + i} r={r} first={i === 0} />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

const SHORT_NAMES: Record<string, string> = {
  "The Pirate Bay": "TPB",
  LimeTorrents: "Lime",
  TorrentGalaxy: "TGx",
};

function shortName(name: string): string {
  return SHORT_NAMES[name] ?? (name.length > 12 ? `${name.slice(0, 11)}…` : name);
}

function ageDays(r: Release): number {
  if (r.publishDate) {
    const d = (Date.now() - new Date(r.publishDate).getTime()) / 8.64e7;
    if (Number.isFinite(d)) return d;
  }
  return r.age;
}

function ResultRow({ r, first }: { r: Release; first: boolean }) {
  const grab = useStore((s) => s.grab);
  const state = useStore((s) => s.grabState[r.guid ?? r.title]);
  const p = r.parsed;

  return (
    <div
      className={cx(
        "group flex items-center gap-4 px-4 py-2.5 transition-colors hover:bg-bg2/70",
        !first && "border-t border-line/60",
        !r.relevant && "opacity-55",
      )}
    >
      {/* title + badges */}
      <div className="min-w-0 flex-1">
        <div className="cursor-text select-text truncate font-mono text-[12px] leading-snug text-ink" title={r.title}>
          {r.title}
        </div>
        <div className="mt-1 flex flex-wrap items-center gap-1">
          <Badge tone="kind" mono={false}>
            {r.kind === "tv" ? "TV" : r.kind === "movie" ? "Movie" : "Other"}
          </Badge>
          {p.resolution && <Badge tone="resolution">{p.resolution}</Badge>}
          {p.source && <Badge>{p.source}</Badge>}
          {p.codec && <Badge>{p.codec}</Badge>}
          {p.audio && <Badge>{p.audio}</Badge>}
          {p.hdr && <Badge tone="hdr">{p.hdr}</Badge>}
          {p.seasonPack && p.season != null && <Badge tone="kind">S{String(p.season).padStart(2, "0")} pack</Badge>}
          {p.proper && <Badge tone="proper">PROPER</Badge>}
          {r.dupeCount > 1 && (
            <span
              className="text-[10.5px] text-faint"
              title={r.alsoOn.length ? `Also on: ${r.alsoOn.join(", ")}` : undefined}
            >
              ×{r.dupeCount} sources
            </span>
          )}
        </div>
      </div>

      {/* size */}
      <div className="w-[72px] shrink-0 text-right font-mono text-[12px] text-dim">
        {fmtBytes(r.size)}
      </div>

      {/* seeders */}
      <div className="flex w-[92px] shrink-0 items-center justify-end gap-1.5 font-mono text-[12px]">
        <SeederDot seeders={r.seeders} />
        <span className="text-ink">{r.seeders ?? "?"}</span>
        <span className="flex items-center text-[10px] text-faint">
          <ArrowUp size={9} className="mr-px" />
          {r.leechers ?? "?"}
          <ArrowDown size={9} className="ml-px" />
        </span>
      </div>

      {/* indexer */}
      <div className="w-[110px] shrink-0 truncate text-right text-[11.5px] text-faint" title={r.indexer ?? undefined}>
        {r.indexer ?? "—"}
      </div>

      {/* age */}
      <div className="w-[42px] shrink-0 text-right font-mono text-[11.5px] text-faint">
        {fmtAge(r.publishDate, r.age)}
      </div>

      {/* actions */}
      <div className="flex w-[104px] shrink-0 items-center justify-end gap-1">
        {r.infoUrl && (
          <a
            href={r.infoUrl}
            target="_blank"
            rel="noreferrer"
            className="rounded-md p-1.5 text-faint opacity-0 transition-all hover:bg-bg3 hover:text-ink focus-visible:opacity-100 group-hover:opacity-100"
            title="Open on indexer"
          >
            <ExternalLink size={13} />
          </a>
        )}
        <Button
          variant={state === "done" ? "default" : "primary"}
          busy={state === "loading"}
          disabled={state === "done"}
          onClick={() => grab(r)}
          className="px-2.5 py-1 text-[11.5px]"
          title="Send to qBittorrent"
        >
          {state === "done" ? (
            <>
              <Check size={12} className="text-ok" /> Sent
            </>
          ) : (
            <>
              <Magnet size={12} /> Grab
            </>
          )}
        </Button>
      </div>
    </div>
  );
}

function SkeletonRows() {
  return (
    <div className="overflow-hidden rounded-(--radius-card) border border-line bg-bg1">
      {Array.from({ length: 9 }).map((_, i) => (
        <div key={i} className={cx("flex items-center gap-4 px-4 py-2.5", i > 0 && "border-t border-line/60")}>
          <div className="flex-1 space-y-2">
            <div className="skeleton h-3 rounded" style={{ width: `${88 - (i * 13) % 45}%` }} />
            <div className="skeleton h-2.5 w-1/3 rounded" />
          </div>
          <div className="skeleton h-3 w-[72px] rounded" />
          <div className="skeleton h-3 w-[92px] rounded" />
          <div className="skeleton h-3 w-[110px] rounded" />
          <div className="skeleton h-3 w-[42px] rounded" />
          <div className="flex w-[104px] justify-end">
            <div className="skeleton h-6 w-[72px] rounded-lg" />
          </div>
        </div>
      ))}
    </div>
  );
}
