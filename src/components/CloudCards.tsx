import { useState } from "react";
import { AlertTriangle, CheckCircle2, Cloud, CloudDownload, FolderOpen, RotateCcw, Trash2 } from "lucide-react";
import type { BitportTransfer, CloudItem } from "../lib/api";
import { fmtBytes, fmtEta, fmtSpeed } from "../lib/format";
import { useStore } from "../store";
import { Button, cx } from "../components/ui";

/** Bitport's status words, in Trawler's voice. */
const CLOUD_STATUS_LABEL: Record<string, string> = {
  queued: "queued on Bitport",
  downloading: "Bitport is downloading",
  finished: "finished in the cloud",
  seeding: "finished in the cloud",
  error: "Bitport gave up",
  waiting: "waiting for Bitport to list it",
};

/** One of Trawler's own cloud grabs, from "sent" through "on this disk". */
export function CloudGrabCard({
  item,
  onRetry,
  onRemove,
}: {
  item: CloudItem;
  onRetry: (item: CloudItem) => void;
  onRemove: (item: CloudItem) => void;
}) {
  const [confirmRemove, setConfirmRemove] = useState(false);
  const phase = item.phase;
  // one bar, two meanings: Bitport's progress while it torrents, ours while
  // the files come down — the label says which
  const pct =
    phase === "done"
      ? 100
      : phase === "fetching"
        ? item.bytesTotal > 0
          ? Math.min(100, (item.bytesDone / item.bytesTotal) * 100)
          : 0
        : phase === "error"
          ? 0
          : Math.min(100, item.cloudProgress);
  const eta = phase === "fetching" && item.speed > 0 ? Math.round((item.bytesTotal - item.bytesDone) / item.speed) : -1;
  const Icon = phase === "done" ? CheckCircle2 : phase === "error" ? AlertTriangle : phase === "fetching" ? CloudDownload : Cloud;
  const tone =
    phase === "done" ? "text-ok" : phase === "error" ? "text-bad" : phase === "fetching" ? "text-accent" : "text-accent2";

  return (
    <div className="group rounded-(--radius-card) border border-line bg-bg1 px-4 py-3 transition-colors hover:border-line2">
      <div className="flex items-center gap-3">
        <Icon size={15} className={cx("shrink-0", tone)} />
        <div className="min-w-0 flex-1">
          <div className="cursor-text select-text truncate font-mono text-[12px] text-ink" title={item.title}>
            {item.title}
          </div>
          <div className="mt-1 flex items-center gap-2.5 text-[11px] text-faint">
            <span className={cx("font-medium", tone)}>
              {phase === "sending" && "sending to Bitport…"}
              {phase === "queued" && "queued on Bitport"}
              {phase === "cloud" && (CLOUD_STATUS_LABEL[item.cloudStatus] ?? item.cloudStatus)}
              {phase === "fetching" && (item.speed > 0 ? "bringing the files here" : "waiting to fetch")}
              {phase === "done" && (item.localPath ? "on this computer" : "finished in the cloud")}
              {phase === "error" && (item.error ?? "failed")}
            </span>
            {phase === "cloud" && item.bytesTotal > 0 && <span className="font-mono">{fmtBytes(item.bytesTotal)}</span>}
            {phase === "fetching" && (
              <>
                <span className="font-mono">
                  {fmtBytes(item.bytesDone)} / {fmtBytes(item.bytesTotal)}
                </span>
                {item.filesTotal > 1 && (
                  <span className="font-mono">
                    {item.filesDone}/{item.filesTotal} files
                  </span>
                )}
                {item.speed > 0 && (
                  <>
                    <span className="font-mono text-accent">{fmtSpeed(item.speed)}</span>
                    <span className="font-mono">eta {fmtEta(eta)}</span>
                  </>
                )}
              </>
            )}
            {phase === "done" && item.localPath && (
              <span className="hidden truncate font-mono xl:inline" title={item.localPath}>
                {item.localPath}
              </span>
            )}
            {phase === "error" && item.filesFailed > 0 && (
              <span className="font-mono">
                {item.filesFailed} of {item.filesTotal} files failed
              </span>
            )}
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-1 opacity-0 transition-opacity group-hover:opacity-100">
          {phase === "done" && item.localPath && (
            <Button
              variant="ghost"
              className="px-2 py-1.5"
              title="Open folder"
              onClick={() => {
                void import("@tauri-apps/plugin-opener")
                  .then((m) => m.revealItemInDir(item.localPath as string))
                  .catch(() => useStore.getState().toast("Couldn't open that folder — it may have been moved", "bad"));
              }}
            >
              <FolderOpen size={14} />
            </Button>
          )}
          {phase === "error" && item.filesFailed > 0 && (
            <Button variant="ghost" className="px-2 py-1.5" title="Try the failed files again" onClick={() => onRetry(item)}>
              <RotateCcw size={14} />
            </Button>
          )}
          <Button
            variant="ghost"
            className="px-2 py-1.5 hover:text-bad"
            title={
              phase === "done"
                ? confirmRemove
                  ? "Click again to hide (keeps the files)"
                  : "Hide (keeps the files)"
                : confirmRemove
                  ? "Click again to remove from Trawler and from Bitport"
                  : "Remove from Trawler and from Bitport"
            }
            onClick={() => {
              if (!confirmRemove) {
                setConfirmRemove(true);
                window.setTimeout(() => setConfirmRemove(false), 2500);
                return;
              }
              setConfirmRemove(false);
              onRemove(item);
            }}
          >
            {confirmRemove ? <span className="text-[10.5px] font-semibold text-bad">sure?</span> : <Trash2 size={14} />}
          </Button>
        </div>

        <div className="w-[52px] shrink-0 text-right font-mono text-[12.5px] font-medium">
          {phase === "error" ? "—" : pct >= 100 ? "100%" : `${pct.toFixed(phase === "fetching" ? 1 : 0)}%`}
        </div>
      </div>

      <div className="mt-2.5 h-[4px] overflow-hidden rounded-full bg-bg3">
        <div
          className={cx(
            "h-full rounded-full transition-[width] duration-700",
            phase === "done"
              ? "bg-ok/70"
              : phase === "error"
                ? "bg-bad"
                : phase === "fetching"
                  ? "bg-gradient-to-r from-accent to-accent2"
                  : "bg-accent2/70",
          )}
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}

/** A transfer in the account that Trawler did not create — shown so the
 *  user can see and free what is taking up their quota. */
export function OtherCloudCard({ t, onDelete }: { t: BitportTransfer; onDelete: (t: BitportTransfer) => void }) {
  const [confirm, setConfirm] = useState(false);
  const done = t.status === "finished" || t.status === "seeding";
  const failed = t.status === "error";
  const pct = done ? 100 : t.progress;
  return (
    <div className="group rounded-(--radius-card) border border-line bg-bg1 px-4 py-2.5">
      <div className="flex items-center gap-3">
        <Cloud size={14} className={cx("shrink-0", done ? "text-dim" : failed ? "text-bad" : "text-accent2")} />
        <div className="min-w-0 flex-1">
          <div className="truncate font-mono text-[12px] text-dim" title={t.name}>
            {t.name}
          </div>
          <div className="mt-0.5 flex items-center gap-2 text-[10.5px] text-faint">
            <span>{failed ? (t.message ?? "Bitport gave up") : CLOUD_STATUS_LABEL[t.status] ?? t.substatus ?? t.status}</span>
            {!done && !failed && <span className="font-mono">{pct.toFixed(0)}%</span>}
          </div>
        </div>
        <a
          href="https://bitport.io/my-files"
          target="_blank"
          rel="noreferrer"
          className="shrink-0 rounded-md bg-bg2 px-2 py-1 text-[11px] text-dim opacity-0 transition-all group-hover:opacity-100 hover:bg-bg3 hover:text-ink"
        >
          Open in Bitport
        </a>
        <Button
          variant="ghost"
          className="px-2 py-1.5 opacity-0 transition-opacity group-hover:opacity-100 hover:text-bad"
          title={confirm ? "Click again to delete from Bitport" : "Delete from Bitport"}
          onClick={() => {
            if (!confirm) {
              setConfirm(true);
              window.setTimeout(() => setConfirm(false), 2500);
              return;
            }
            setConfirm(false);
            onDelete(t);
          }}
        >
          {confirm ? <span className="text-[10.5px] font-semibold text-bad">sure?</span> : <Trash2 size={14} />}
        </Button>
      </div>
      {!done && !failed && (
        <div className="mt-2 h-[3px] overflow-hidden rounded-full bg-bg3">
          <div className="h-full rounded-full bg-accent2/60 transition-[width] duration-500" style={{ width: pct + "%" }} />
        </div>
      )}
    </div>
  );
}
