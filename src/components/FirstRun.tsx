import { useCallback, useEffect, useRef, useState } from "react";
import {
  Check,
  Download,
  Play,
  Plus,
  Radar,
  RefreshCw,
  Sparkles,
  Waves,
  Wrench,
} from "lucide-react";
import { api, onSetupStep, type SetupStatus } from "../lib/api";
import { useStore } from "../store";
import { Button, cx } from "./ui";

export default function FirstRun({ onDone }: { onDone: () => void }) {
  const toast = useStore((s) => s.toast);
  const refreshStatus = useStore((s) => s.refreshStatus);
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [progress, setProgress] = useState<number | null>(null);
  const [logLine, setLogLine] = useState<string | null>(null);
  const [addedIndexers, setAddedIndexers] = useState<string[] | null>(null);
  const [keyDraft, setKeyDraft] = useState("");
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const busyRef = useRef(false);

  const poll = useCallback(async () => {
    try {
      setStatus(await api.setupStatus());
    } catch {
      /* keep last */
    }
  }, []);

  useEffect(() => {
    void poll();
    pollRef.current = setInterval(() => void poll(), 3000);
    const un = onSetupStep((s) => {
      if (s.kind === "progress") setProgress(s.payload.pct ?? null);
      if (s.kind === "log") { setLogLine(s.payload.message ?? null); setProgress(null); }
      if (s.kind === "done") { setLogLine(null); setProgress(null); }
      if (s.kind === "error") { setLogLine(null); setProgress(null); }
    });
    return () => {
      if (pollRef.current) clearInterval(pollRef.current);
      un();
    };
  }, [poll]);

  const run = async (key: string, fn: () => Promise<unknown>, okMsg?: string) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(key);
    try {
      await fn();
      if (okMsg) toast(okMsg, "ok");
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setBusy(null);
      setProgress(null);
      setLogLine(null);
      busyRef.current = false;
      void poll();
    }
  };

  const finish = async () => {
    if (busyRef.current) return;
    try {
      await api.setupFinish();
    } catch {
      /* mock mode */
    }
    void refreshStatus();
    onDone();
  };

  const ready = status?.qbit === "ok" && status?.prowlarr === "ok";

  return (
    <div className="app-bg fixed inset-0 z-50 overflow-y-auto">
      <div className="mx-auto flex min-h-full max-w-[600px] flex-col justify-center px-6 py-12">
        {/* header */}
        <div className="msg-in text-center">
          <div className="mx-auto flex size-14 items-center justify-center rounded-2xl bg-gradient-to-br from-accent/25 to-accent2/15 shadow-[0_0_44px_-10px_var(--color-accent)]">
            <Waves size={24} className="text-accent" />
          </div>
          <h1 className="mt-5 text-[22px] font-bold tracking-tight">Welcome to Trawler</h1>
          <p className="mx-auto mt-2 max-w-[420px] text-[13px] leading-relaxed text-dim">
            Trawler works with two free apps — let's get them running.
            Mostly automatic, about two minutes.
          </p>
        </div>

        {/* component cards */}
        <div className="mt-8 space-y-3">
          <SetupCard
            icon={<Download size={16} />}
            name="qBittorrent"
            role="downloads everything Trawler grabs"
            ok={status?.qbit === "ok"}
            statusLine={
              status === null ? "Checking…"
              : status.qbit === "ok" ? "Connected and ready"
              : status.qbit === "running_no_webui" ? "Running, but its Web UI is off"
              : status.qbit === "installed_stopped" ? "Installed but not running"
              : "Not installed"
            }
            indeterminate={busy === "qbit-install"}
            logLine={busy === "qbit-install" ? logLine : null}
          >
            {status?.qbit === "missing" && (
              <Button variant="primary" busy={busy === "qbit-install"} disabled={busy !== null} onClick={() => run("qbit-install", api.setupInstallQbit, "qBittorrent installed — one more click to enable its Web UI")} className="text-[12px]">
                <Download size={13} /> Install automatically
              </Button>
            )}
            {(status?.qbit === "installed_stopped" || status?.qbit === "running_no_webui") && (
              <Button variant="primary" busy={busy === "qbit-cfg"} disabled={busy !== null} onClick={() => run("qbit-cfg", api.setupConfigureQbit, "qBittorrent configured and launched")} className="text-[12px]">
                <Wrench size={13} /> {status.qbit === "running_no_webui" ? "Restart with Web UI enabled" : "Enable Web UI & launch"}
              </Button>
            )}
          </SetupCard>

          <SetupCard
            icon={<Radar size={16} />}
            name="Prowlarr"
            role="talks to your torrent indexers"
            ok={status?.prowlarr === "ok"}
            statusLine={
              status === null ? "Checking…"
              : status.prowlarr === "ok"
                ? status.prowlarrHasIndexers ? "Running, connected, indexers ready" : "Running and connected — no indexers yet"
              : status.prowlarr === "needs_key" ? "Running, but Trawler can't log in — paste its API key below"
              : status.prowlarr === "managed_stopped" ? "Installed by Trawler, currently stopped"
              : "Not installed"
            }
            progress={busy === "prowlarr-install" ? progress : null}
            logLine={busy === "prowlarr-install" ? logLine : null}
          >
            {status?.prowlarr === "missing" && (
              <Button variant="primary" busy={busy === "prowlarr-install"} disabled={busy !== null} onClick={() => run("prowlarr-install", api.setupInstallProwlarr, "Prowlarr installed and connected")} className="text-[12px]">
                <Download size={13} /> Install automatically
              </Button>
            )}
            {status?.prowlarr === "managed_stopped" && (
              <Button variant="primary" busy={busy === "prowlarr-start"} disabled={busy !== null} onClick={() => run("prowlarr-start", api.setupStartProwlarr, "Prowlarr is up")} className="text-[12px]">
                <Play size={13} /> Start Prowlarr
              </Button>
            )}
            {status?.prowlarr === "needs_key" && (
              <div className="flex w-full flex-wrap items-center gap-2">
                <input
                  value={keyDraft}
                  onChange={(e) => setKeyDraft(e.target.value)}
                  placeholder="Prowlarr API key"
                  spellCheck={false}
                  className="min-w-[220px] flex-1 rounded-(--radius-btn) border border-line2 bg-bg0/60 px-2.5 py-1.5 font-mono text-[11.5px] text-ink outline-none placeholder:text-faint focus:border-accent/50"
                />
                <Button
                  variant="primary"
                  busy={busy === "prowlarr-key"}
                  disabled={!keyDraft.trim() || busy !== null}
                  onClick={() => run("prowlarr-key", () => api.setupSaveProwlarrKey(keyDraft), "Connected to Prowlarr")}
                  className="text-[12px]"
                >
                  Connect
                </Button>
                <span className="w-full text-[10.5px] text-faint">
                  Prowlarr → Settings → General → Security → API Key
                </span>
              </div>
            )}
            {status?.prowlarr === "ok" && !status.prowlarrHasIndexers && (
              <Button
                variant="primary"
                busy={busy === "indexers"}
                disabled={busy !== null}
                onClick={() =>
                  run("indexers", async () => {
                    const names = await api.setupStarterIndexers();
                    setAddedIndexers(names);
                    toast(
                      names.length > 0
                        ? `Added ${names.length} indexer${names.length === 1 ? "" : "s"}`
                        : "Nothing new to add — Prowlarr already has these",
                      "ok",
                    );
                  })
                }
                className="text-[12px]"
              >
                <Plus size={13} /> Add starter indexers
              </Button>
            )}
            {addedIndexers && addedIndexers.length > 0 && (
              <span className="text-[11px] text-ok">Added {addedIndexers.join(", ")}</span>
            )}
          </SetupCard>

          <SetupCard
            icon={<Sparkles size={16} />}
            name="AI agent"
            role="optional — hunts what episode-tracking can't name"
            ok={status?.agent === "ok"}
            dimWhenMissing
            statusLine={
              status === null ? "Checking…"
              : status.agent === "ok" ? "Model endpoint reachable"
              : "Optional — point Settings → Agent at an Ollama endpoint anytime"
            }
          />
        </div>

        {/* footer */}
        <div className="msg-in mt-8 flex items-center justify-center gap-3">
          <Button variant="ghost" disabled={busy !== null} onClick={finish} className="text-[12.5px]">
            Skip — I'll set up manually
          </Button>
          <Button variant="primary" disabled={!ready || busy !== null} onClick={finish} className="px-5 text-[13px]">
            {ready ? "Start hunting" : "Waiting for the crew…"}
          </Button>
        </div>
        {!ready && (
          <button
            type="button"
            disabled={busy !== null}
            onClick={() => void poll()}
            className="mx-auto mt-3 flex cursor-pointer items-center gap-1.5 text-[11px] text-faint hover:text-dim disabled:pointer-events-none disabled:opacity-50"
          >
            <RefreshCw size={10} /> re-check now
          </button>
        )}
      </div>
    </div>
  );
}

function SetupCard({
  icon,
  name,
  role,
  ok,
  statusLine,
  progress,
  logLine,
  indeterminate,
  dimWhenMissing,
  children,
}: {
  icon: React.ReactNode;
  name: string;
  role: string;
  ok: boolean;
  statusLine: string;
  progress?: number | null;
  logLine?: string | null;
  indeterminate?: boolean;
  dimWhenMissing?: boolean;
  children?: React.ReactNode;
}) {
  return (
    <div
      className={cx(
        "msg-in rounded-(--radius-card) border bg-bg1 p-4 transition-colors",
        ok ? "border-ok/25" : dimWhenMissing ? "border-line opacity-75" : "border-line",
      )}
    >
      <div className="flex items-start gap-3">
        <div
          className={cx(
            "flex size-9 shrink-0 items-center justify-center rounded-xl",
            ok ? "bg-ok/12 text-ok" : "bg-bg3 text-dim",
          )}
        >
          {ok ? <Check size={16} /> : icon}
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-baseline gap-2">
            <span className="text-[13.5px] font-semibold">{name}</span>
            <span className="truncate text-[11px] text-faint">{role}</span>
          </div>
          <div className={cx("mt-0.5 text-[12px]", ok ? "text-ok" : "text-dim")}>{statusLine}</div>
          {typeof progress === "number" ? (
            <div className="mt-2 h-[4px] overflow-hidden rounded-full bg-bg3">
              <div
                className="h-full rounded-full bg-gradient-to-r from-accent to-accent2 transition-[width] duration-300"
                style={{ width: `${progress}%` }}
              />
            </div>
          ) : indeterminate ? (
            <div className="mt-2 h-[4px] overflow-hidden rounded-full bg-bg3">
              <div className="progress-indeterminate h-full rounded-full bg-gradient-to-r from-accent to-accent2" />
            </div>
          ) : null}
          {logLine && <div className="mt-1.5 text-[11px] text-faint">{logLine}</div>}
          {children && <div className="mt-2.5 flex flex-wrap items-center gap-2">{children}</div>}
        </div>
      </div>
    </div>
  );
}
