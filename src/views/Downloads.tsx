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
import { api, type BitportTransfer, type CloudItem, type DownloadsView as DL, type QbitTorrent } from "../lib/api";
import { fmtBytes, fmtEta, fmtSpeed, qbitStateLabel, stateKind } from "../lib/format";
import { useStore } from "../store";
import { Button, CenterMessage, Segmented, cx } from "../components/ui";
import { Cloud, CloudDownload } from "lucide-react";
import { CloudGrabCard, OtherCloudCard } from "../components/CloudCards";

export default function DownloadsView() {
  const [data, setData] = useState<DL | null>(null);
  const [error, setError] = useState<string | null>(null);
  // removed optimistically: a poll already in flight when the user clicked
  // remove can still carry the dead entry — hide it briefly. Keys are
  // "t:<token>" for foreign cloud transfers and "l:<ledgerId>" for grabs.
  const removedRef = useRef<Map<string, number>>(new Map());
  const hidden = (key: string) => {
    const t = removedRef.current.get(key);
    if (t && Date.now() - t < 15_000) return true;
    if (t) removedRef.current.delete(key);
    return false;
  };
  const [scope, setScope] = useState<"trawler" | "all">("trawler");
  const timer = useRef<ReturnType<typeof setInterval> | null>(null);
  const toast = useStore((s) => s.toast);
  const config = useStore((s) => s.config);

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

  const retryCloud = async (item: CloudItem) => {
    try {
      const n = await api.cloudRetry(item.ledgerId);
      toast(
        n === 0
          ? "Nothing left to retry"
          : item.filesFailed > 0
            ? `Retrying ${item.filesFailed} file${item.filesFailed === 1 ? "" : "s"}`
            : "Trying again — Bitport will be re-read on the next check",
        "info",
      );
    } catch (e) {
      toast(String(e), "bad");
    }
  };

  const removeCloud = async (item: CloudItem, deleteCloud: boolean) => {
    // a finished grab either drops its kept cloud copy (the card stays) or
    // leaves the list; anything unfinished is taken out of the cloud too
    const dropCloudCopyOnly = item.phase === "done" && deleteCloud;
    try {
      await api.cloudRemove(item.ledgerId, deleteCloud);
      if (dropCloudCopyOnly) {
        setData((d) =>
          d ? { ...d, cloud: { ...d.cloud, items: d.cloud.items.map((x) => (x.ledgerId === item.ledgerId ? { ...x, cloudCopy: false } : x)) } } : d,
        );
        toast(`Deleted the cloud copy of ${item.title}`, "info");
        return;
      }
      removedRef.current.set(`l:${item.ledgerId}`, Date.now());
      setData((d) => (d ? { ...d, cloud: { ...d.cloud, items: d.cloud.items.filter((x) => x.ledgerId !== item.ledgerId) } } : d));
      toast(
        item.phase === "done"
          ? `Hid ${item.title}`
          : item.cloudCopy && item.token
            ? `Removed ${item.title} from Trawler and Bitport`
            : `Removed ${item.title}`,
        "info",
      );
    } catch (e) {
      toast(String(e), "bad");
    }
  };

  const deleteOther = async (t: BitportTransfer) => {
    try {
      await api.bitportDelete(t.token);
      removedRef.current.set(`t:${t.token}`, Date.now());
      setData((d) => (d ? { ...d, cloud: { ...d.cloud, others: d.cloud.others.filter((x) => x.token !== t.token) } } : d));
      toast("Deleted from your Bitport cloud", "info");
    } catch (e) {
      toast(String(e), "bad");
    }
  };

  const cloudItems = data?.cloud.items.filter((i) => !hidden(`l:${i.ledgerId}`)) ?? [];
  const cloudOthers = data?.cloud.others.filter((t) => !hidden(`t:${t.token}`)) ?? [];
  const showOthers = scope === "all" && cloudOthers.length > 0;
  const nothingAtAll = !!data && data.torrents.length === 0 && cloudItems.length === 0 && !showOthers;
  // a cloud-first setup may have no qBittorrent at all: its silence is a
  // note, not an alarm (the local list is unknown, not known-empty)
  const cloudOnly = config?.downloadBackend === "bitport" && !!data?.cloud.connected;

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
          {data && data.cloud.fetchSpeed > 0 && (
            <span className="flex items-center gap-1 font-mono text-[11.5px] text-faint" title="Coming down from your Bitport cloud">
              <CloudDownload size={11} className="text-accent2" />
              {fmtSpeed(data.cloud.fetchSpeed)}
            </span>
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
        {(error ?? data?.qbitError) && data && !cloudOnly && (
          <div className="mb-3 flex items-center gap-2 rounded-lg border border-warn/25 bg-warn/8 px-3 py-1.5 text-[11.5px] text-warn">
            qBittorrent didn't answer the last check — the local list may be stale
          </div>
        )}
        {(error ?? data?.qbitError) && data && cloudOnly && (
          <div className="mb-3 flex items-center gap-2 rounded-lg border border-line bg-bg1 px-3 py-1.5 text-[11.5px] text-faint">
            <HardDrive size={12} /> qBittorrent isn't reachable — local torrents, if any, aren't listed
          </div>
        )}
        {data?.cloud.connected && data.cloud.authFailed && (
          <div className="mb-3 flex items-center gap-2 rounded-lg border border-bad/30 bg-bad/8 px-3 py-1.5 text-[11.5px] text-bad">
            <Cloud size={13} /> Bitport no longer accepts Trawler's access — reconnect it under Settings → Connections
          </div>
        )}
        {data?.cloud.connected && !data.cloud.authFailed && data.cloud.error && (
          <div className="mb-3 flex items-center gap-2 rounded-lg border border-warn/25 bg-warn/8 px-3 py-1.5 text-[11.5px] text-warn">
            <Cloud size={13} /> Bitport didn't answer the last check — the cloud list may be stale
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
        ) : data.qbitError && nothingAtAll && !cloudOnly ? (
          <CenterMessage icon={<HardDrive size={28} />} title="Can't reach qBittorrent" body={data.qbitError} />
        ) : nothingAtAll ? (
          <CenterMessage
            icon={cloudOnly ? <Cloud size={28} /> : <HardDrive size={28} />}
            title={scope === "trawler" ? "Nothing grabbed yet" : "No torrents"}
            body={
              scope === "trawler"
                ? cloudOnly
                  ? data.cloud.fetchToLocal
                    ? "Releases you grab go to your Bitport cloud and land here on their way to this computer."
                    : "Releases you grab go to your Bitport cloud and are listed here; the files stay in the cloud."
                  : "Releases you grab land here, tagged with the trawler category."
                : undefined
            }
          />
        ) : (
          <div className="space-y-2">
            {data.torrents.map((t) => (
              <TorrentCard key={t.hash} t={t} onAction={act} />
            ))}
            {cloudItems.length > 0 && (
              <>
                <div className={cx("flex items-center gap-1.5 pb-1 text-[11.5px] font-medium text-dim", data.torrents.length > 0 && "pt-3")}>
                  <Cloud size={13} className="text-accent2" /> Through your Bitport cloud
                  {!data.cloud.fetchToLocal && (
                    <span className="font-normal text-faint">· files stay in the cloud (Settings → Connections)</span>
                  )}
                </div>
                {cloudItems.map((item) => (
                  <CloudGrabCard key={item.ledgerId} item={item} onRetry={retryCloud} onRemove={removeCloud} />
                ))}
              </>
            )}
            {scope === "trawler" && cloudOthers.length > 0 && (
              <button
                type="button"
                onClick={() => setScope("all")}
                className="mt-3 cursor-pointer text-[11px] text-faint underline decoration-line2 underline-offset-2 hover:text-dim"
              >
                {cloudOthers.length} other transfer{cloudOthers.length === 1 ? "" : "s"} in your Bitport cloud — show everything
              </button>
            )}
            {showOthers && (
              <>
                <div className="flex items-center gap-1.5 pt-3 pb-1 text-[11.5px] font-medium text-dim">
                  <Cloud size={13} className="text-faint" /> Also in your Bitport cloud
                  <span className="font-normal text-faint">· other transfers in the account</span>
                </div>
                {cloudOthers.map((t) => (
                  <OtherCloudCard key={t.token} t={t} onDelete={deleteOther} />
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

