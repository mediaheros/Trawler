import { useEffect, useState } from "react";
import { ArrowDownCircle, RefreshCw, X } from "lucide-react";
import { inTauri } from "../lib/api";
import { checkForUpdate, relaunchApp } from "../lib/updater";
import { useStore } from "../store";
import { Button } from "./ui";

type Phase = "idle" | "downloading" | "downloaded" | "installing";

/** Silent startup check; a quiet bottom-right card when a new version exists.
 *  Download and install are separate steps: on Windows install() hands off to
 *  the installer and the process exits (the installer relaunches the app), so
 *  everything after it only runs on Linux. */
export default function UpdateBanner() {
  const toast = useStore((s) => s.toast);
  const update = useStore((s) => s.pendingUpdate);
  const dismissedVersion = useStore((s) => s.updateDismissedVersion);
  const setDismissedVersion = useStore((s) => s.setUpdateDismissedVersion);
  const [phase, setPhase] = useState<Phase>("idle");
  const [pct, setPct] = useState<number | null>(null);

  // a re-check replaces the update object (and frees the old one's bytes) —
  // phase must not survive that or "Install & restart" acts on a dead handle
  useEffect(() => {
    setPhase("idle");
    setPct(null);
  }, [update]);

  useEffect(() => {
    if (!inTauri) return;
    // no once-guard: the clearTimeout cleanup keeps StrictMode double-mount safe
    const t = setTimeout(() => {
      checkForUpdate()
        .then((u) => {
          if (u) useStore.getState().setPendingUpdate(u);
        })
        .catch(() => {
          /* offline or endpoint down — stay quiet, retry next launch */
        });
    }, 15_000);
    return () => clearTimeout(t);
  }, []);

  if (!update || (dismissedVersion === update.version && phase === "idle")) return null;

  const download = async () => {
    setPhase("downloading");
    setPct(null);
    try {
      await update.download((done, total) => {
        if (total && total > 0) setPct(Math.min(100, Math.round((done / total) * 100)));
      });
      setPhase("downloaded");
    } catch (e) {
      setPhase("idle");
      toast(`Update download failed: ${e}`, "bad");
    }
  };

  const install = async () => {
    setPhase("installing");
    try {
      // Windows: never returns — the installer takes over and relaunches.
      await update.install();
      // Linux AppImage: swap happened in place, restart into it.
      await relaunchApp();
    } catch (e) {
      setPhase("downloaded");
      toast(
        `Update install failed: ${e} — if you installed via deb/rpm, update through your package manager`,
        "bad",
      );
    }
  };

  const dismiss = () => setDismissedVersion(update.version);

  return (
    <div className="toast-in fixed bottom-4 right-4 z-30 w-[300px] rounded-(--radius-card) border border-line2 bg-bg1 p-3.5 shadow-[0_16px_50px_-12px_rgba(0,0,0,0.9),0_0_40px_-20px_var(--color-accent)]">
      <div className="flex items-start gap-2.5">
        <ArrowDownCircle size={16} className="mt-0.5 shrink-0 text-accent" />
        <div className="min-w-0 flex-1">
          <div className="text-[12.5px] font-semibold">
            {phase === "downloaded" || phase === "installing"
              ? "Update downloaded"
              : `Trawler ${update.version} is available`}
          </div>
          <div className="mt-0.5 text-[11px] leading-snug text-faint">
            {phase === "installing"
              ? "Installing — the app will restart by itself."
              : phase === "downloaded"
                ? "Ready to install. The app restarts as part of it."
                : phase === "downloading"
                  ? pct !== null
                    ? `Downloading… ${pct}%`
                    : "Downloading…"
                  : "Signed release, verified before install."}
          </div>
          {phase === "downloading" && (
            <div className="mt-2 h-[3px] overflow-hidden rounded-full bg-bg3">
              <div
                className={
                  pct === null
                    ? "progress-indeterminate h-full rounded-full bg-accent"
                    : "h-full rounded-full bg-accent transition-[width] duration-300"
                }
                style={pct === null ? undefined : { width: `${pct}%` }}
              />
            </div>
          )}
          <div className="mt-2.5 flex gap-2">
            {phase === "idle" && (
              <>
                <Button variant="primary" onClick={download} className="px-2.5 py-1 text-[11.5px]">
                  Update now
                </Button>
                <Button variant="ghost" onClick={dismiss} className="px-2 py-1 text-[11.5px]">
                  Later
                </Button>
              </>
            )}
            {phase === "downloaded" && (
              <Button variant="primary" onClick={install} className="px-2.5 py-1 text-[11.5px]">
                <RefreshCw size={12} /> Install & restart
              </Button>
            )}
          </div>
        </div>
        {phase === "idle" && (
          <button
            type="button"
            aria-label="Dismiss update notice"
            onClick={dismiss}
            className="cursor-pointer rounded p-0.5 text-faint transition-colors hover:text-dim"
          >
            <X size={13} />
          </button>
        )}
      </div>
    </div>
  );
}
