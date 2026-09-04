import { useEffect, useMemo, useRef, useState } from "react";
import { Check, Loader2, Search as SearchIcon, Tv } from "lucide-react";
import { api, type TvmazeResult } from "../lib/api";
import { useStore } from "../store";
import { Button, CloseBtn, StatusPill } from "./ui";

export default function AddShow({ onClose }: { onClose: () => void }) {
  const followShow = useStore((s) => s.followShow);
  // NOTE: zustand selectors must return stable references — deriving arrays
  // inside the selector loops the render. Select raw state, derive with memo.
  const shows = useStore((s) => s.shows);
  const followedIds = useMemo(() => shows.map((x) => x.tvmazeId), [shows]);
  const [q, setQ] = useState("");
  const [results, setResults] = useState<TvmazeResult[]>([]);
  const [busy, setBusy] = useState(false);
  const [backfill, setBackfill] = useState(true);
  const [following, setFollowing] = useState<number | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const seq = useRef(0);
  const followingRef = useRef(false);

  // onClose is an inline closure recreated on every parent render, and the
  // parent re-renders on every 15 s poll. Keying this effect on it re-ran
  // the cleanup each tick: the sequence bump orphaned any TVmaze search in
  // flight (spinner forever, results never shown) and the focus() call
  // yanked the keyboard away from the backfill checkbox and Follow button.
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;
  useEffect(() => {
    inputRef.current?.focus();
    const onKey = (e: KeyboardEvent) =>
      e.key === "Escape" && !followingRef.current && onCloseRef.current();
    window.addEventListener("keydown", onKey);
    const seqRef = seq;
    return () => {
      // unmount only: drop any search still in flight
      ++seqRef.current;
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  // debounced search-as-you-type against TVmaze
  useEffect(() => {
    if (!q.trim()) {
      ++seq.current;
      setResults([]);
      setBusy(false);
      return;
    }
    const mySeq = ++seq.current;
    const t = setTimeout(async () => {
      setBusy(true);
      try {
        const r = await api.searchTvmaze(q.trim());
        if (seq.current === mySeq) setResults(r);
      } catch {
        if (seq.current === mySeq) setResults([]);
      } finally {
        if (seq.current === mySeq) setBusy(false);
      }
    }, 350);
    return () => clearTimeout(t);
  }, [q]);

  const follow = async (r: TvmazeResult) => {
    if (followingRef.current) return;
    followingRef.current = true;
    setFollowing(r.id);
    try {
      await followShow(r.id, backfill);
      onClose();
    } catch {
      // toast already shown by the store; stay open so the user can retry
    } finally {
      followingRef.current = false;
      setFollowing(null);
    }
  };

  return (
    <div
      className="fixed inset-0 z-40 flex items-start justify-center bg-black/60 pt-[10vh] backdrop-blur-[2px]"
      onMouseDown={(e) =>
        e.target === e.currentTarget && !followingRef.current && onClose()
      }
    >
      <div className="toast-in w-[560px] max-w-[92vw] overflow-hidden rounded-(--radius-card) border border-line2 bg-bg1 shadow-[0_24px_80px_-20px_rgba(0,0,0,0.9)]">
        <div className="group relative border-b border-line transition-colors focus-within:border-accent/40">
          <SearchIcon
            size={15}
            className="pointer-events-none absolute left-4 top-1/2 -translate-y-1/2 text-faint transition-colors group-focus-within:text-accent"
          />
          <input
            ref={inputRef}
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Search TV shows on TVmaze…"
            spellCheck={false}
            className="w-full bg-transparent py-3.5 pl-10.5 pr-11 text-[14px] text-ink outline-none placeholder:text-faint"
          />
          {busy ? (
            <Loader2 size={15} className="spin absolute right-4 top-1/2 -translate-y-1/2 text-faint" />
          ) : (
            <CloseBtn
              onClick={onClose}
              disabled={following !== null}
              className="absolute right-2.5 top-1/2 -translate-y-1/2"
            />
          )}
        </div>

        <div className="max-h-[52vh] overflow-y-auto">
          {results.length === 0 ? (
            <div className="px-5 py-8 text-center text-[12px] text-faint">
              {q.trim() ? (busy ? "Searching…" : "No shows found") : "Type a show name to search TVmaze."}
            </div>
          ) : (
            results.map((r) => {
              const already = r.followed || followedIds.includes(r.id);
              return (
                <div
                  key={r.id}
                  className="flex items-center gap-3 border-b border-line/50 px-4 py-2.5 last:border-b-0 hover:bg-bg2/60"
                >
                  <div className="h-[72px] w-[48px] shrink-0 overflow-hidden rounded-md bg-bg3">
                    {r.poster ? (
                      <img src={r.poster} alt="" loading="lazy" className="size-full object-cover" />
                    ) : (
                      <div className="flex size-full items-center justify-center text-faint">
                        <Tv size={16} />
                      </div>
                    )}
                  </div>
                  <div className="min-w-0 flex-1">
                    <div className="flex items-baseline gap-2">
                      <span className="truncate text-[13px] font-semibold">{r.name}</span>
                      <span className="shrink-0 text-[11px] text-faint">
                        {r.premiered?.slice(0, 4) ?? "—"}
                      </span>
                    </div>
                    <div className="mt-0.5 flex items-center gap-1.5 text-[11px] text-faint">
                      <StatusPill status={r.status} />
                      {r.network && <span className="truncate">{r.network}</span>}
                      {r.genres.length > 0 && (
                        <span className="truncate">· {r.genres.slice(0, 2).join(", ")}</span>
                      )}
                    </div>
                  </div>
                  <Button
                    variant={already ? "default" : "primary"}
                    disabled={already || following !== null}
                    busy={following === r.id}
                    onClick={() => follow(r)}
                    className="shrink-0 px-2.5 py-1 text-[11.5px]"
                  >
                    {already ? (
                      <>
                        <Check size={12} className="text-ok" /> Following
                      </>
                    ) : (
                      "Follow"
                    )}
                  </Button>
                </div>
              );
            })
          )}
        </div>

        <label className="flex cursor-pointer items-center gap-2 border-t border-line bg-bg2/40 px-4 py-2.5 text-[12px] text-dim">
          <input
            type="checkbox"
            checked={backfill}
            disabled={following !== null}
            onChange={(e) => setBackfill(e.target.checked)}
            className="size-3.5 accent-(--color-accent)"
          />
          Also grab episodes that already aired — otherwise only new ones from now on
        </label>
      </div>
    </div>
  );
}
