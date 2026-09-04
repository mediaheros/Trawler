import { useEffect, useRef, useState } from "react";
import {
  ArrowDown,
  ArrowUp,
  FolderOpen,
  HardDrive,
  Pause,
  Play,
  Trash2,
} from "lucide-react";
import { api, type DownloadsView as DL, type QbitTorrent } from "../lib/api";
import { fmtBytes, fmtEta, fmtSpeed, qbitStateLabel, stateKind } from "../lib/format";
import { useStore } from "../store";
import { Button, CenterMessage, Segmented, cx } from "../components/ui";
import { Cloud } from "lucide-react";

export default function DownloadsView() {
  const [data, setData] = useState<DL | null>(null);
  const [error, setError] = useState<string | null>(null);
  // tokens removed optimistically: a poll already in flight when the user
  // clicked remove can still carry the dead transfer — hide it briefly
  const removedRef = useRef<Map<string, number>>(new Map());
  const [scope, setScope] = useState<"trawler" | "all">("trawler");
  const timer = useRef<ReturnType<typeof setInterval> | null>(null);
  const toast = useStore((s) => s.toast);

  useEffect(() => {
    let alive = true;
    let inFlight = false;
    let seq = 0;
    const tick = async () => {
      if (inFlight) return; // a slow qBt response must not stack requests
      inFlight = true;
      const mine = ++seq;
      try {
        const d = await api.downloads(scope === "all");
        if (alive && mine === seq) {
          setData(d);
          setError(null);
        }
      } catch (e) {
        if (alive && mine === seq) setError(String(e));
      } finally {
        inFlight = false;
      }
    };
    void tick();
    timer.current = setInterval(tick, 2000);
    return () => {
      alive = false;
      if (timer.current) clearInterval(timer.current);
    };
  }, [scope]);

  const act = async (action: string, t: QbitTorrent) => {
    try {
      await api.torrentAction(action, t.hash);
      if (action.startsWith("delete")) toast(`Removed ${t.name}`, "info");
    } catch (e) {
      toast(String(e), "bad");
    }
  };

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center justify-between px-6 pt-5 pb-4">
        <div className="flex items-baseline gap-3">
          <h1 className="text-[16px] font-semibold tracking-tight">Downloads</h1>
          {data?.transfer && (
            <div className="flex items-center gap-3 font-mono text-[11.5px] text-faint">
              <span className="flex items-center gap-1">
                <ArrowDown size={11} className="text-accent" />
                {fmtSpeed(data.transfer.dl_info_speed)}
              </span>
              <span className="flex items-center gap-1">
                <ArrowUp size={11} />
                {fmtSpeed(data.transfer.up_info_speed)}
              </span>
            </div>
          )}
        </div>
        <Segmented
          value={scope}
          onChange={setScope}
          options={[
            { value: "trawler", label: "Trawler" },
            { value: "all", label: "Everything" },
          ]}
        />
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-6 pb-6">
        {(error ?? data?.qbitError) && data && (
          <div className="mb-3 flex items-center gap-2 rounded-lg border border-warn/25 bg-warn/8 px-3 py-1.5 text-[11.5px] text-warn">
            qBittorrent didn't answer the last check — the local list may be stale
          </div>
        )}
        {error && !data ? (
          <CenterMessage icon={<HardDrive size={28} />} title="Can't reach qBittorrent" body={error} />
        ) : !data ? (
          <div className="space-y-2">
            {Array.from({ length: 4 }).map((_, i) => (
              <div key={i} className="skeleton h-[74px] rounded-(--radius-card)" />
            ))}
          </div>
        ) : data.qbitError && data.torrents.length === 0 && data.cloud.length === 0 ? (
          <CenterMessage icon={<HardDrive size={28} />} title="Can't reach qBittorrent" body={data.qbitError} />
        ) : data.torrents.length === 0 && data.cloud.length === 0 ? (
          <CenterMessage
            icon={<HardDrive size={28} />}
            title={scope === "trawler" ? "Nothing grabbed yet" : "No torrents"}
            body={
              scope === "trawler"
                ? "Releases you grab land here, tagged with the trawler category."
                : undefined
            }
          />
        ) : (
          <div className="space-y-2">
            {data.torrents.map((t) => (
              <TorrentCard key={t.hash} t={t} onAction={act} />
            ))}
            {data.cloud.some((c) => {
              const t = removedRef.current.get(c.token);
              return !t || Date.now() - t >= 15_000;
            }) && (
              <>
                <div className="flex items-center gap-1.5 pt-3 pb-1 text-[11.5px] font-medium text-dim">
                  <Cloud size={13} className="text-accent2" /> In your Bitport cloud
                </div>
                {data.cloud
                  .filter((c) => {
                    const t = removedRef.current.get(c.token);
                    if (t && Date.now() - t < 15_000) return false;
                    if (t) removedRef.current.delete(c.token);
                    return true;
                  })
                  .map((c) => (
                    <CloudCard
                      key={c.token}
                      c={c}
                      onDeleted={() => {
                        removedRef.current.set(c.token, Date.now());
                        setData((d) => (d ? { ...d, cloud: d.cloud.filter((x) => x.token !== c.token) } : d));
                      }}
                    />
                  ))}
              </>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

function TorrentCard({
  t,
  onAction,
}: {
  t: QbitTorrent;
  onAction: (action: string, t: QbitTorrent) => void;
}) {
  const kind = stateKind(t.state);
  const pct = Math.min(100, t.progress * 100);
  const [confirmRemove, setConfirmRemove] = useState(false);
  const stopped = t.state.startsWith("paused") || t.state.startsWith("stopped");

  return (
    <div className="group rounded-(--radius-card) border border-line bg-bg1 px-4 py-3 transition-colors hover:border-line2">
      <div className="flex items-center gap-3">
        <div className="min-w-0 flex-1">
          <div className="cursor-text select-text truncate font-mono text-[12px] text-ink" title={t.name}>
            {t.name}
          </div>
          <div className="mt-1 flex items-center gap-2.5 text-[11px] text-faint">
            <span
              className={cx(
                "font-medium",
                kind === "active" && "text-accent",
                kind === "done" && "text-ok",
                kind === "error" && "text-bad",
                kind === "paused" && "text-warn",
              )}
            >
              {qbitStateLabel[t.state] ?? t.state}
            </span>
            <span className="font-mono">
              {fmtBytes(t.size * t.progress)} / {fmtBytes(t.size)}
            </span>
            {kind === "active" && t.dlspeed > 0 && (
              <>
                <span className="flex items-center gap-0.5 font-mono">
                  <ArrowDown size={10} className="text-accent" />
                  {fmtSpeed(t.dlspeed)}
                </span>
                <span className="font-mono">eta {fmtEta(t.eta)}</span>
              </>
            )}
            {kind === "done" && (
              <span className="font-mono">ratio {t.ratio.toFixed(2)}</span>
            )}
            <span className="hidden truncate font-mono xl:inline" title={t.save_path}>
              {t.save_path}
            </span>
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-1 opacity-0 transition-opacity group-hover:opacity-100">
          {t.content_path && (
            <Button
              variant="ghost"
              className="px-2 py-1.5"
              title="Open folder"
              onClick={() => {
                // reveal is the only opener permission granted; a path that
                // is not on disk yet (metadata-only torrent, moved files)
                // must say so instead of failing silently
                void import("@tauri-apps/plugin-opener")
                  .then((m) => m.revealItemInDir(t.content_path))
                  .catch(() => useStore.getState().toast("Couldn't open that folder — it may not exist yet", "bad"));
              }}
            >
              <FolderOpen size={14} />
            </Button>
          )}
          <Button
            variant="ghost"
            className="px-2 py-1.5"
            title={stopped ? "Resume" : "Pause"}
            onClick={() => onAction(stopped ? "start" : "stop", t)}
          >
            {stopped ? <Play size={14} /> : <Pause size={14} />}
          </Button>
          <Button
            variant="ghost"
            className="px-2 py-1.5 hover:text-bad"
            title={confirmRemove ? "Click again to remove (keeps files)" : "Remove (keeps files)"}
            onClick={() => {
              if (!confirmRemove) {
                setConfirmRemove(true);
                window.setTimeout(() => setConfirmRemove(false), 2500);
                return;
              }
              setConfirmRemove(false);
              onAction("delete", t);
            }}
          >
            {confirmRemove ? <span className="text-[10.5px] font-semibold text-bad">sure?</span> : <Trash2 size={14} />}
          </Button>
        </div>

        <div className="w-[52px] shrink-0 text-right font-mono text-[12.5px] font-medium">
          {pct >= 100 ? "100%" : `${pct.toFixed(1)}%`}
        </div>
      </div>

      {/* progress bar */}
      <div className="mt-2.5 h-[4px] overflow-hidden rounded-full bg-bg3">
        <div
          className={cx(
            "h-full rounded-full transition-[width] duration-700",
            kind === "done"
              ? "bg-ok/70"
              : kind === "error"
                ? "bg-bad"
                : kind === "paused"
                  ? "bg-warn/60"
                  : "bg-gradient-to-r from-accent to-accent2",
          )}
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}

/** A transfer living on Bitport's servers — progress is theirs, bytes arrive
 *  over HTTPS whenever the user wants them locally. */
function CloudCard({ c, onDeleted }: { c: import("../lib/api").BitportTransfer; onDeleted: () => void }) {
  const toast = useStore((s) => s.toast);
  const [confirm, setConfirm] = useState(false);
  const done = c.status === "finished";
  const pct = done ? 100 : c.progress;
  return (
    <div className="rounded-(--radius-card) border border-line bg-bg1 px-4 py-3">
      <div className="flex items-center gap-2.5">
        <Cloud size={14} className={done ? "shrink-0 text-ok" : "shrink-0 text-accent2"} />
        <div className="min-w-0 flex-1">
          <div className="truncate font-mono text-[12px] text-ink">{c.name}</div>
          <div className="mt-0.5 flex items-center gap-2 text-[10.5px] text-faint">
            <span>{done ? "finished — in your cloud" : c.substatus || c.status}</span>
            {!done && <span>{pct.toFixed(0)}%</span>}
          </div>
        </div>
        <a
          href="https://bitport.io/my-files"
          target="_blank"
          rel="noreferrer"
          className="shrink-0 rounded-md bg-bg2 px-2 py-1 text-[11px] text-dim transition-colors hover:bg-bg3 hover:text-ink"
        >
          Open in Bitport
        </a>
        <button
          type="button"
          className="shrink-0 cursor-pointer rounded-md px-2 py-1 text-[11px] text-faint transition-colors hover:text-bad"
          onClick={async () => {
            if (!confirm) {
              setConfirm(true);
              window.setTimeout(() => setConfirm(false), 2500);
              return;
            }
            try {
              await api.bitportDelete(c.token);
              onDeleted();
              toast("Removed from your cloud", "info");
            } catch (e) {
              toast(String(e), "bad");
            }
          }}
        >
          {confirm ? "sure?" : "remove"}
        </button>
      </div>
      {!done && (
        <div className="mt-2 h-[3px] overflow-hidden rounded-full bg-bg3">
          <div className="h-full rounded-full bg-accent2 transition-[width] duration-500" style={{ width: pct + "%" }} />
        </div>
      )}
    </div>
  );
}
