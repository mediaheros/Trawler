import { useEffect, useRef, useState } from "react";
import { Check, ExternalLink, Loader2, Star, Tv } from "lucide-react";
import { api, type ShowPreview as Preview } from "../lib/api";
import { useStore } from "../store";
import { Button, CloseBtn, StatusPill } from "./ui";

export default function ShowPreview({
  tvmazeId,
  onClose,
}: {
  tvmazeId: number;
  onClose: () => void;
}) {
  const followShow = useStore((s) => s.followShow);
  const inLibrary = useStore((s) => s.shows.some((x) => x.tvmazeId === tvmazeId));
  const [data, setData] = useState<Preview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [backfill, setBackfill] = useState(false);
  const [busy, setBusy] = useState(false);
  const cardRef = useRef<HTMLDivElement | null>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

  // fetch keyed on the show alone — a re-rendered onClose must not re-fetch
  useEffect(() => {
    let alive = true;
    setData(null);
    setError(null);
    api
      .showPreview(tvmazeId)
      .then((d) => alive && setData(d))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [tvmazeId]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && closeRef.current();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // focus management: move focus into the dialog on open, hand it back on close
  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    cardRef.current?.focus();
    return () => prev?.focus?.();
  }, []);

  const followed = inLibrary || data?.followed === true;

  const follow = async () => {
    setBusy(true);
    try {
      await followShow(tvmazeId, backfill);
    } catch {
      /* store toasted */
    } finally {
      setBusy(false);
    }
  };

  const years = data
    ? `${data.premiered?.slice(0, 4) ?? "—"}${
        data.ended ? `–${data.ended.slice(0, 4)}` : data.status === "Running" ? "–" : ""
      }`
    : "";

  return (
    <div
      className="fixed inset-0 z-40 flex items-center justify-center bg-black/65 p-6 backdrop-blur-[3px]"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <div
        ref={cardRef}
        role="dialog"
        aria-modal="true"
        aria-label={data?.name ?? "Show details"}
        tabIndex={-1}
        className="toast-in relative w-[720px] max-w-[94vw] overflow-hidden rounded-(--radius-card) border border-line2 bg-bg1 shadow-[0_32px_100px_-24px_rgba(0,0,0,0.95),0_0_80px_-40px_var(--color-accent)] outline-none">
        {/* ambient backdrop from the poster */}
        {data?.poster && (
          <img
            src={data.poster}
            alt=""
            className="pointer-events-none absolute inset-0 size-full scale-125 object-cover opacity-[0.08] blur-2xl"
          />
        )}
        <CloseBtn onClick={onClose} className="absolute right-3 top-3 z-10" />

        {error ? (
          <div className="p-10 text-center text-[12.5px] text-faint">{error}</div>
        ) : !data ? (
          <div className="flex items-center justify-center gap-2 p-14 text-[12.5px] text-faint">
            <Loader2 size={14} className="spin" /> Fetching show details…
          </div>
        ) : (
          <div className="relative flex gap-6 p-6">
            {/* poster */}
            <div className="w-[210px] shrink-0">
              <div className="aspect-[2/3] overflow-hidden rounded-xl border border-line2 shadow-[0_16px_50px_-16px_rgba(0,0,0,0.9)]">
                {data.poster ? (
                  <img src={data.poster} alt="" className="size-full object-cover" />
                ) : (
                  <div className="flex size-full items-center justify-center bg-bg2 text-faint">
                    <Tv size={30} />
                  </div>
                )}
              </div>
              <div className="mt-3 space-y-2">
                <Button
                  variant={followed ? "default" : "primary"}
                  disabled={followed}
                  busy={busy}
                  onClick={follow}
                  className="w-full justify-center text-[13px]"
                >
                  {followed ? (
                    <>
                      <Check size={13} className="text-ok" /> Following
                    </>
                  ) : (
                    "Follow this show"
                  )}
                </Button>
                {!followed && data.airedEpisodeCount > 0 && (
                  <label className="flex cursor-pointer items-start gap-2 px-0.5 text-[11px] leading-snug text-faint">
                    <input
                      type="checkbox"
                      checked={backfill}
                      onChange={(e) => setBackfill(e.target.checked)}
                      className="mt-0.5 size-3 accent-(--color-accent)"
                    />
                    Also grab the {data.airedEpisodeCount} episode
                    {data.airedEpisodeCount === 1 ? "" : "s"} that already aired
                  </label>
                )}
              </div>
            </div>

            {/* details */}
            <div className="min-w-0 flex-1 py-1">
              <h2 className="pr-8 text-[22px] font-bold leading-tight tracking-tight">{data.name}</h2>
              <div className="mt-2 flex flex-wrap items-center gap-2 text-[12px] text-dim">
                <StatusPill status={data.status} />
                <span>{years}</span>
                {data.network && <span>· {data.network}</span>}
                {data.runtime && <span>· {data.runtime} min</span>}
                {data.rating != null && (
                  <span className="flex items-center gap-1 text-warn">
                    <Star size={11} fill="currentColor" /> {data.rating.toFixed(1)}
                  </span>
                )}
              </div>

              {data.genres.length > 0 && (
                <div className="mt-2.5 flex flex-wrap gap-1.5">
                  {data.genres.map((g) => (
                    <span key={g} className="rounded-md border border-line2 bg-bg2 px-2 py-0.5 text-[10.5px] text-dim">
                      {g}
                    </span>
                  ))}
                </div>
              )}

              <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1 text-[11.5px] text-faint">
                <span>
                  <span className="text-dim">{data.seasonCount}</span> season{data.seasonCount === 1 ? "" : "s"} ·{" "}
                  <span className="text-dim">{data.episodeCount}</span> episodes
                </span>
                {data.scheduleDays && (
                  <span>
                    {data.scheduleDays}
                    {data.scheduleTime ? ` at ${data.scheduleTime}` : ""}
                  </span>
                )}
                {data.imdb && (
                  <a
                    href={`https://www.imdb.com/title/${data.imdb}/`}
                    target="_blank"
                    rel="noreferrer"
                    className="inline-flex items-center gap-1 text-dim hover:text-accent"
                  >
                    IMDb <ExternalLink size={10} />
                  </a>
                )}
              </div>

              {data.summary && (
                <p className="mt-3.5 max-h-[150px] select-text overflow-y-auto pr-2 text-[12.5px] leading-relaxed text-dim">
                  {data.summary}
                </p>
              )}

              {data.cast.length > 0 && (
                <div className="mt-4">
                  <div className="mb-2 text-[10.5px] font-semibold uppercase tracking-wide text-faint">Cast</div>
                  <div className="flex gap-3 overflow-x-auto pb-1">
                    {data.cast.map((c) => (
                      <div key={c.person} className="w-[64px] shrink-0 text-center">
                        <div className="mx-auto size-11 overflow-hidden rounded-full border border-line2 bg-bg2">
                          {c.image ? (
                            <img src={c.image} alt="" loading="lazy" className="size-full object-cover" />
                          ) : (
                            <div className="flex size-full items-center justify-center text-[13px] font-semibold text-faint">
                              {c.person.slice(0, 1)}
                            </div>
                          )}
                        </div>
                        <div className="mt-1 truncate text-[9.5px] text-dim" title={c.person}>{c.person}</div>
                        {c.character && (
                          <div className="truncate text-[8.5px] text-faint" title={c.character}>{c.character}</div>
                        )}
                      </div>
                    ))}
                  </div>
                </div>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
