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

export default function DownloadsView() {
  const [data, setData] = useState<DL | null>(null);
  const [error, setError] = useState<string | null>(null);
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
        {error && data && (
          <div className="mb-3 flex items-center gap-2 rounded-lg border border-warn/25 bg-warn/8 px-3 py-1.5 text-[11.5px] text-warn">
            qBittorrent didn't answer the last check — showing the previous state
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
        ) : data.torrents.length === 0 ? (
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
                void import("@tauri-apps/plugin-opener").then((m) =>
                  m.revealItemInDir(t.content_path).catch(() => m.openPath(t.save_path)),
                );
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
