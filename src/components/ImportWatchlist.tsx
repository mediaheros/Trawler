import { useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Check,
  CheckCircle2,
  ClipboardList,
  CloudOff,
  HelpCircle,
  Loader2,
  Sparkles,
  Tv,
} from "lucide-react";
import { api, type ImportRow } from "../lib/api";
import { useStore } from "../store";
import { Button, CloseBtn, cx } from "./ui";

type RowState = ImportRow & {
  /** the agent settled this ambiguous row */
  agentPicked?: boolean;
  /** the user picked manually — the agent must never override this */
  userChose?: boolean;
  /** per-row follow progress */
  progress?: "pending" | "following" | "done" | "error";
};

const chosenMatch = (r: RowState) => r.matches.find((m) => m.id === r.chosen);
const chosenFollowed = (r: RowState) => chosenMatch(r)?.followed ?? false;

/** Paste names / an IMDb list URL / an IMDb CSV → review matches → follow. */
export default function ImportWatchlist({ onClose }: { onClose: () => void }) {
  const loadShows = useStore((s) => s.loadShows);
  const toast = useStore((s) => s.toast);
  const agentEnabled = useStore((s) => s.config?.agentEnabled ?? false);

  const [input, setInput] = useState("");
  const [parsing, setParsing] = useState(false);
  const [rows, setRows] = useState<RowState[] | null>(null);
  const [totalFound, setTotalFound] = useState(0);
  const [disambiguating, setDisambiguating] = useState(false);
  const [backfill, setBackfill] = useState(false);
  const [estimate, setEstimate] = useState<
    { eps: number; gb: number; failed: number } | "loading" | null
  >(null);
  const [importing, setImporting] = useState(false);
  const busyRef = useRef(false);
  /** discards stale disambiguation responses after re-parse / start over */
  const parseGen = useRef(0);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busyRef.current) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const parse = async () => {
    setParsing(true);
    const gen = ++parseGen.current;
    try {
      const parsed = await api.importLookup(input);
      if (gen !== parseGen.current) return;
      setRows(parsed.rows);
      setTotalFound(parsed.totalFound);
      // fire the agent at ambiguous rows in the background — non-blocking,
      // and the user can always override the pick
      const ambiguous = parsed.rows.filter(
        (r) => r.confidence === "ambiguous" && r.matches.length > 1,
      );
      if (agentEnabled && ambiguous.length > 0) {
        setDisambiguating(true);
        api
          .importDisambiguate(
            ambiguous.map((r) => ({
              input: r.input,
              candidates: r.matches.map((m) => ({
                id: m.id,
                name: m.name,
                year: m.premiered?.slice(0, 4) ?? null,
                network: m.network,
                status: m.status,
              })),
            })),
          )
          .then((picks) => {
            if (gen !== parseGen.current) return; // superseded parse
            const pickByIdx = new Map<number, number>();
            ambiguous.forEach((a, i) => {
              const p = picks[i];
              if (p != null) pickByIdx.set(a.idx, p);
            });
            setRows((cur) =>
              cur === null
                ? null
                : cur.map((row) => {
                    const pick = pickByIdx.get(row.idx);
                    return pick != null && !row.userChose && row.confidence === "ambiguous"
                      ? { ...row, chosen: pick, agentPicked: true }
                      : row;
                  }),
            );
          })
          .catch(() => {
            /* ambiguity survives; the dropdowns still work */
          })
          .finally(() => {
            if (gen === parseGen.current) setDisambiguating(false);
          });
      }
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      if (gen === parseGen.current) setParsing(false);
    }
  };

  const followable = useMemo(
    () =>
      (rows ?? []).filter(
        (r) => r.chosen != null && !chosenFollowed(r) && r.progress !== "done",
      ),
    [rows],
  );
  // stable content key so progress-only row updates don't refire the estimate
  const followableIds = useMemo(
    () => followable.map((r) => r.chosen as number).join(","),
    [followable],
  );

  // cost estimate whenever backfill is on and the selection settles
  useEffect(() => {
    if (!backfill || followableIds.length === 0 || importing) {
      if (!backfill) setEstimate(null);
      return;
    }
    let alive = true;
    setEstimate("loading");
    const ids = followableIds.split(",").map(Number);
    const t = setTimeout(() => {
      api
        .importEstimate(ids)
        .then((e) => alive && setEstimate({ eps: e.airedEpisodes, gb: e.estGb, failed: e.failed }))
        .catch(() => alive && setEstimate(null));
    }, 400);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [backfill, followableIds, importing]);

  const runImport = async () => {
    setImporting(true);
    busyRef.current = true;
    let ok = 0;
    let failed = 0;
    for (const row of followable) {
      setRows((cur) =>
        (cur ?? []).map((r) => (r.idx === row.idx ? { ...r, progress: "following" } : r)),
      );
      try {
        await api.followShow(row.chosen as number, backfill, null);
        ok++;
        setRows((cur) =>
          (cur ?? []).map((r) => (r.idx === row.idx ? { ...r, progress: "done" } : r)),
        );
      } catch {
        failed++;
        setRows((cur) =>
          (cur ?? []).map((r) => (r.idx === row.idx ? { ...r, progress: "error" } : r)),
        );
      }
    }
    await loadShows();
    busyRef.current = false;
    setImporting(false);
    toast(
      failed === 0
        ? `Following ${ok} new show${ok === 1 ? "" : "s"}`
        : `Followed ${ok}, ${failed} failed — press Follow again to retry the red rows`,
      failed === 0 ? "ok" : "bad",
    );
    if (failed === 0) onClose();
  };

  const alreadyCount = (rows ?? []).filter((r) => r.chosen != null && chosenFollowed(r)).length;
  const unmatchedCount = (rows ?? []).filter((r) => r.chosen == null).length;

  return (
    <div
      className="fixed inset-0 z-40 flex items-start justify-center bg-black/60 pt-[8vh] backdrop-blur-[2px]"
      onMouseDown={(e) => e.target === e.currentTarget && !busyRef.current && onClose()}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Import watchlist"
        className="toast-in flex max-h-[80vh] w-[640px] max-w-[94vw] flex-col overflow-hidden rounded-(--radius-card) border border-line2 bg-bg1 shadow-[0_24px_80px_-20px_rgba(0,0,0,0.9)]"
      >
        <div className="flex items-center gap-2.5 border-b border-line px-4 py-3">
          <ClipboardList size={15} className="text-accent" />
          <span className="text-[13.5px] font-semibold">Import a watchlist</span>
          {rows && (
            <button
              type="button"
              onClick={() => {
                parseGen.current++;
                setRows(null);
                setEstimate(null);
                setDisambiguating(false);
              }}
              className="cursor-pointer text-[11.5px] text-faint transition-colors hover:text-dim disabled:opacity-40"
              disabled={importing}
            >
              ← start over
            </button>
          )}
          {!importing && <CloseBtn onClick={onClose} className="ml-auto" />}
        </div>

        {!rows ? (
          <>
            <div className="p-4">
              <textarea
                autoFocus
                value={input}
                onChange={(e) => setInput(e.target.value)}
                placeholder={
                  "One show per line:\n  Severance\n  Battlestar Galactica (2004)\n  Dark\n\n…or paste a public IMDb list URL, or the CSV from IMDb's Export button."
                }
                spellCheck={false}
                rows={9}
                className="w-full resize-none rounded-(--radius-btn) border border-line2 bg-bg0/60 p-3 font-mono text-[12px] leading-relaxed text-ink outline-none placeholder:text-faint focus:border-accent/50"
              />
              <p className="mt-2 text-[11px] text-faint">
                Up to 50 shows per import. Names are matched on TVmaze; you review every match
                before anything is followed.
              </p>
            </div>
            <div className="flex items-center justify-end gap-2 border-t border-line bg-bg2/40 px-4 py-3">
              <Button variant="ghost" onClick={onClose}>Cancel</Button>
              <Button variant="primary" busy={parsing} disabled={!input.trim()} onClick={parse}>
                Find matches
              </Button>
            </div>
          </>
        ) : (
          <>
            {totalFound > rows.length && (
              <div className="border-b border-line bg-warn/5 px-4 py-2 text-[11.5px] text-warn">
                Showing the first {rows.length} of {totalFound} titles — import the rest in a
                second pass.
              </div>
            )}
            {disambiguating && (
              <div className="flex items-center gap-2 border-b border-line bg-accent/5 px-4 py-2 text-[11.5px] text-dim">
                <Sparkles size={12} className="text-accent" />
                The agent is settling ambiguous matches — you can override any pick below.
              </div>
            )}
            <div className="min-h-0 flex-1 overflow-y-auto">
              {rows.map((row) => (
                <ImportRowLine
                  key={row.idx}
                  row={row}
                  disabled={importing}
                  onChoose={(id) =>
                    setRows((cur) =>
                      cur === null
                        ? null
                        : cur.map((r) =>
                            r.idx === row.idx
                              ? { ...r, chosen: id, agentPicked: false, userChose: true }
                              : r,
                          ),
                    )
                  }
                />
              ))}
            </div>
            <div className="border-t border-line bg-bg2/40 px-4 py-3">
              <label className="flex cursor-pointer items-center gap-2 text-[12px] text-dim">
                <input
                  type="checkbox"
                  checked={backfill}
                  onChange={(e) => setBackfill(e.target.checked)}
                  disabled={importing}
                  className="size-3.5 accent-(--color-accent)"
                />
                Also grab episodes that already aired
                {estimate === "loading" ? (
                  <span className="inline-flex items-center gap-1 text-[11px] text-faint">
                    <Loader2 size={10} className="spin" /> estimating…
                  </span>
                ) : estimate ? (
                  <span className="text-[11px] text-warn">
                    ≈ {estimate.eps}
                    {estimate.failed > 0 ? "+" : ""} episodes · roughly {Math.round(estimate.gb)} GB
                    {estimate.failed > 0 && (
                      <span className="text-faint">
                        {" "}
                        (partial — {estimate.failed} show{estimate.failed === 1 ? "" : "s"} couldn't
                        be checked)
                      </span>
                    )}
                  </span>
                ) : null}
              </label>
              <div className="mt-2.5 flex items-center gap-2">
                <span className="text-[11.5px] text-faint">
                  {followable.length} new · {alreadyCount} already followed · {unmatchedCount}{" "}
                  unmatched
                </span>
                <span className="ml-auto" />
                <Button variant="ghost" onClick={onClose} disabled={importing}>Cancel</Button>
                <Button
                  variant="primary"
                  busy={importing}
                  disabled={followable.length === 0}
                  onClick={runImport}
                >
                  Follow {followable.length} show{followable.length === 1 ? "" : "s"}
                </Button>
              </div>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

const CONFIDENCE: Record<
  ImportRow["confidence"],
  { icon: typeof Check; cls: string; label: string }
> = {
  exact: { icon: CheckCircle2, cls: "text-ok", label: "Exact match" },
  good: { icon: Check, cls: "text-accent", label: "Good match" },
  ambiguous: { icon: HelpCircle, cls: "text-warn", label: "Several shows share this name — check the pick" },
  none: { icon: AlertTriangle, cls: "text-bad", label: "Nothing found on TVmaze" },
};

function ImportRowLine({
  row,
  disabled,
  onChoose,
}: {
  row: RowState;
  disabled: boolean;
  onChoose: (id: number) => void;
}) {
  const conf = CONFIDENCE[row.confidence];
  const Icon = row.lookupFailed ? CloudOff : row.agentPicked ? Sparkles : conf.icon;
  const iconCls = row.lookupFailed ? "text-bad" : row.agentPicked ? "text-accent" : conf.cls;
  const title = row.lookupFailed
    ? "The TVmaze lookup failed (rate limit or network) — start over to retry"
    : row.agentPicked
      ? "The agent picked this match — override if it's wrong"
      : conf.label;
  const chosen = chosenMatch(row);

  return (
    <div
      className={cx(
        "flex items-center gap-3 border-b border-line/50 px-4 py-2 last:border-b-0",
        row.progress === "error" && "bg-bad/5",
      )}
    >
      <span title={title} className={cx("shrink-0", iconCls)}>
        <Icon size={14} />
      </span>
      <div className="w-[190px] shrink-0 truncate text-[12px] text-dim" title={row.input}>
        {row.input}
      </div>
      <div className="min-w-0 flex-1">
        {row.lookupFailed ? (
          <span className="text-[11.5px] text-bad/80">
            Lookup failed — not "no match". Start over to retry this one.
          </span>
        ) : row.matches.length === 0 ? (
          <span className="text-[11.5px] text-faint">
            No TVmaze entry — is it a movie, or spelled differently?
          </span>
        ) : row.matches.length === 1 && chosen ? (
          <span className="flex items-center gap-1.5 text-[12px]">
            <Tv size={11} className="shrink-0 text-faint" />
            <span className="truncate">{chosen.name}</span>
            <span className="shrink-0 text-[10.5px] text-faint">
              {chosen.premiered?.slice(0, 4) ?? "—"}
              {chosen.network ? ` · ${chosen.network}` : ""}
            </span>
          </span>
        ) : (
          <select
            value={row.chosen ?? ""}
            onChange={(e) => onChoose(Number(e.target.value))}
            disabled={disabled}
            className="w-full cursor-pointer rounded-(--radius-btn) border border-line2 bg-bg1 px-1.5 py-1 text-[11.5px] text-ink focus:border-accent/50"
          >
            {row.chosen == null && <option value="">Pick the right show…</option>}
            {row.matches.map((m) => (
              <option key={m.id} value={m.id}>
                {m.name} ({m.premiered?.slice(0, 4) ?? "?"}
                {m.network ? `, ${m.network}` : ""})
                {m.followed ? " — already following" : ""}
              </option>
            ))}
          </select>
        )}
      </div>
      <span className="w-[74px] shrink-0 text-right">
        {row.progress === "following" ? (
          <Loader2 size={13} className="spin inline text-accent" />
        ) : row.progress === "done" ? (
          <Check size={13} className="inline text-ok" />
        ) : row.progress === "error" ? (
          <span className="text-[10.5px] text-bad">failed</span>
        ) : row.chosen != null && chosenFollowed(row) ? (
          <span className="text-[10.5px] text-faint">following</span>
        ) : null}
      </span>
    </div>
  );
}
