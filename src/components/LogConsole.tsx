import { useEffect, useMemo, useRef, useState } from "react";
import { ClipboardCopy, Pause, Play } from "lucide-react";
import { api, onAppLog, type LogEntry } from "../lib/api";
import { useStore } from "../store";
import { Button, cx } from "./ui";

const LEVELS = ["all", "info", "warn", "error"] as const;
type LevelFilter = (typeof LEVELS)[number];

// stable unique ids assigned at ingest: content-based keys collide on
// identical same-second lines, and index-based keys shift with the
// 500-row window and remount the whole list on every flush
let logSeq = 0;
type ConsoleEntry = LogEntry & { seq: number };

/** Live diagnostic console: what the app is actually doing, copyable in one
 *  click as a support bundle. The answer to "it's hanging". */
export default function LogConsole() {
  const toast = useStore((s) => s.toast);
  const [entries, setEntries] = useState<ConsoleEntry[]>([]);
  const [level, setLevel] = useState<LevelFilter>("all");
  const [paused, setPaused] = useState(false);
  const [copying, setCopying] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const pausedRef = useRef(false);
  const liveRevision = useRef(0);
  const droppedWhilePaused = useRef(0);
  pausedRef.current = paused;

  // incoming events buffer in a ref and flush once per frame — a burst of
  // log lines must not mean a 2000-element copy per line
  const inbox = useRef<LogEntry[]>([]);
  useEffect(() => {
    let active = true;
    const revisionAtLoad = liveRevision.current;
    void api
      .logsRecent()
      .then((rows) => {
        if (!active) return;
        setEntries((current) => {
          if (liveRevision.current === revisionAtLoad) {
            return rows.map((e) => ({ ...e, seq: logSeq++ }));
          }
          // Live events arrived while the snapshot was in flight. Preserve
          // them and prepend only strictly older history, avoiding a stale
          // wholesale replacement (and avoiding duplicate recent lines).
          const oldestLiveTs = current[0]?.ts ?? Number.POSITIVE_INFINITY;
          const history = rows
            .filter((entry) => entry.ts < oldestLiveTs)
            .map((entry) => ({ ...entry, seq: logSeq++ }));
          const next = [...history, ...current];
          return next.length > 2000 ? next.slice(next.length - 2000) : next;
        });
      })
      .catch(() => {});
    let raf = 0;
    const flush = () => {
      raf = 0;
      if (inbox.current.length === 0) return;
      const batch = inbox.current;
      inbox.current = [];
      setEntries((prev) => {
        const next = [...prev, ...batch.map((e) => ({ ...e, seq: logSeq++ }))];
        return next.length > 2000 ? next.slice(next.length - 2000) : next;
      });
    };
    const un = onAppLog((e) => {
      ++liveRevision.current;
      inbox.current.push(e);
      if (inbox.current.length > 2000) {
        const overflow = inbox.current.length - 2000;
        inbox.current.splice(0, overflow);
        droppedWhilePaused.current += overflow;
      }
      // paused = frozen VIEW; the buffer keeps collecting so resume shows
      // everything (a pause that DROPS entries is the opposite of a pause)
      if (!pausedRef.current && raf === 0) raf = requestAnimationFrame(flush);
    });
    return () => {
      active = false;
      un();
      if (raf) cancelAnimationFrame(raf);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!paused && inbox.current.length > 0) {
      const batch = inbox.current;
      inbox.current = [];
      if (droppedWhilePaused.current > 0) {
        batch.unshift({
          ts: Date.now() / 1000,
          level: "warn",
          area: "app",
          message: `${droppedWhilePaused.current} older live log lines were discarded while the view was paused`,
        });
        droppedWhilePaused.current = 0;
      }
      setEntries((prev) => {
        const next = [...prev, ...batch.map((e) => ({ ...e, seq: logSeq++ }))];
        return next.length > 2000 ? next.slice(next.length - 2000) : next;
      });
    }
  }, [paused]);

  useEffect(() => {
    const el = scrollRef.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  }, [entries]);

  const visible = useMemo(() => {
    const filtered = level === "all" ? entries : entries.filter((e) => e.level === level);
    // cap the DOM: the ring holds 2000, the screen needs the recent few hundred
    return filtered.length > 500 ? filtered.slice(filtered.length - 500) : filtered;
  }, [entries, level]);

  const copyBundle = async () => {
    setCopying(true);
    try {
      const bundle = await api.logsSupportBundle();
      await navigator.clipboard.writeText(bundle);
      toast("Support bundle copied — paste it anywhere", "ok");
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setCopying(false);
    }
  };

  const fmtTime = (ts: number) => {
    const d = new Date(ts * 1000);
    return d.toTimeString().slice(0, 8);
  };

  return (
    <div>
      <div className="mb-2 flex items-center gap-1.5">
        {LEVELS.map((l) => (
          <button
            key={l}
            type="button"
            onClick={() => setLevel(l)}
            className={cx(
              "cursor-pointer rounded-md px-2 py-0.5 text-[11px] transition-colors",
              level === l ? "bg-accent-soft text-accent" : "bg-bg2 text-faint hover:bg-bg3 hover:text-dim",
            )}
          >
            {l}
          </button>
        ))}
        <button
          type="button"
          onClick={() => setPaused(!paused)}
          className="ml-1 cursor-pointer rounded-md bg-bg2 p-1 text-faint transition-colors hover:bg-bg3 hover:text-dim"
          title={paused ? "Resume live tail" : "Pause live tail"}
        >
          {paused ? <Play size={11} /> : <Pause size={11} />}
        </button>
        <div className="ml-auto">
          <Button onClick={() => void copyBundle()} busy={copying} className="px-2.5 py-1 text-[11.5px]">
            <ClipboardCopy size={12} /> Copy for support
          </Button>
        </div>
      </div>
      <div
        ref={scrollRef}
        onScroll={(e) => {
          const el = e.currentTarget;
          pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
        }}
        className="h-[420px] overflow-y-auto rounded-(--radius-btn) border border-line bg-bg0/60 p-2 font-mono text-[11px] leading-[1.7]"
      >
        {visible.length === 0 ? (
          <div className="px-1 py-2 text-faint">Nothing logged yet at this level.</div>
        ) : (
          visible.map((e) => (
            <div key={e.seq} className="flex gap-2 whitespace-pre-wrap break-all px-1">
              <span className="shrink-0 text-faint">{fmtTime(e.ts)}</span>
              <span
                className={cx(
                  "w-[68px] shrink-0 truncate",
                  e.level === "error" ? "text-bad" : e.level === "warn" ? "text-warn" : "text-faint",
                )}
              >
                {e.area}
              </span>
              <span className={cx(e.level === "error" ? "text-bad" : e.level === "warn" ? "text-warn/90" : "text-dim")}>
                {e.message}
              </span>
            </div>
          ))
        )}
      </div>
      <p className="mt-1.5 text-[10.5px] text-faint">
        Everything Trawler does — searches, grabs, scheduler decisions, setup steps. Keys and passkeys are scrubbed
        before logging. "Copy for support" adds app + qBittorrent health context.
      </p>
    </div>
  );
}
