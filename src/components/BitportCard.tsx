import { useEffect, useState } from "react";
import { AlertTriangle, Cloud } from "lucide-react";
import { api, type BitportStatus, type Config } from "../lib/api";
import { fmtBytes } from "../lib/format";
import { useStore } from "../store";
import { Button, Field, Segmented, TextInput, cx } from "./ui";

/** Settings card for the cloud backend: connect once, choose where grabs
 *  go, and decide what happens when Bitport finishes — by default the files
 *  come to this computer over HTTPS and the cloud copy is cleaned up. */
export default function BitportCard({
  draft,
  set,
}: {
  draft: Config;
  set: (patch: Partial<Config>) => void;
}) {
  const toast = useStore((s) => s.toast);
  const loadConfig = useStore((s) => s.loadConfig);
  const [status, setStatus] = useState<BitportStatus | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [waiting, setWaiting] = useState(false);
  const [manual, setManual] = useState(false);

  useEffect(() => {
    let alive = true;
    api
      .bitportStatus()
      .then((s) => {
        if (alive) setStatus(s);
      })
      // a failed probe must not masquerade as "not connected" — that would
      // invite a second OAuth flow on top of a working account
      .catch((e) => {
        if (alive) setStatusError(String(e));
      });
    return () => {
      alive = false;
    };
  }, []);

  const announce = (s: BitportStatus) => {
    setStatus(s);
    toast(`Bitport connected — ${s.quota ? fmtBytes(s.quota.diskAvailable) + " free in the cloud" : "ready"}`, "ok");
  };

  const connectFlow = async () => {
    setWaiting(true);
    try {
      const s = await api.bitportConnectFlow();
      await loadConfig(); // the zustand config is loaded once at startup — resync it
      announce(s);
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setWaiting(false);
    }
  };

  const connectManual = async () => {
    setBusy(true);
    try {
      const s = await api.bitportConnect(code);
      setCode("");
      await loadConfig();
      announce(s);
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setBusy(false);
    }
  };

  const disconnect = async () => {
    try {
      await api.bitportDisconnect();
      setStatus((s) => (s ? { ...s, connected: false, authFailed: false, quota: null } : s));
      set({ downloadBackend: "qbittorrent" });
      await loadConfig();
      toast("Bitport disconnected — grabs go to local qBittorrent", "info");
    } catch (e) {
      toast(String(e), "bad");
    }
  };

  const quota = status?.quota ?? null;
  const usedFrac = quota && quota.diskSize > 0 ? quota.diskUsed / quota.diskSize : 0;
  const expiry = quota?.planExpiration ? quota.planExpiration.slice(0, 10) : null;

  return (
    <Card
      title="Bitport cloud"
      sub="Optional — Bitport does the torrenting on its servers; Trawler brings the finished files here over plain HTTPS"
    >
      {statusError && !status ? (
        <div className="flex items-center gap-2.5 rounded-lg border border-warn/25 bg-warn/8 px-3 py-2 text-[11.5px] text-warn">
          <AlertTriangle size={14} className="shrink-0" />
          <span className="min-w-0 flex-1 truncate" title={statusError}>
            Couldn't read the Bitport connection state: {statusError}
          </span>
        </div>
      ) : status?.connected ? (
        <div className="space-y-3.5">
          {status.authFailed && (
            <div className="flex items-center gap-2.5 rounded-lg border border-bad/30 bg-bad/8 px-3 py-2 text-[11.5px] text-bad">
              <AlertTriangle size={14} className="shrink-0" />
              <span className="flex-1">
                Bitport no longer accepts Trawler's access. Cloud grabs are paused until you reconnect.
              </span>
              <Button variant="primary" busy={waiting} onClick={() => void connectFlow()} className="shrink-0 px-2.5 py-1 text-[11.5px]">
                {waiting ? "Waiting…" : "Reconnect"}
              </Button>
            </div>
          )}

          {quota && (
            <div>
              <div className="flex items-baseline justify-between text-[11.5px]">
                <span className="text-dim">
                  {quota.account && (
                    <span className="mr-2 font-mono text-ink" title="The Bitport account Trawler is connected to">
                      {quota.account}
                    </span>
                  )}
                  Plan <span className="font-medium text-ink">{quota.planName}</span>
                  {quota.planExpired ? (
                    <span className="ml-1.5 text-bad">expired</span>
                  ) : (
                    expiry && <span className="ml-1.5 text-faint">until {expiry}</span>
                  )}
                </span>
                {quota.diskSize > 0 && (
                  <span className="font-mono text-faint">
                    {fmtBytes(quota.diskUsed)} / {fmtBytes(quota.diskSize)} used
                  </span>
                )}
              </div>
              {quota.diskSize > 0 && (
                <div className="mt-1.5 h-[5px] overflow-hidden rounded-full bg-bg3">
                  <div
                    className={cx("h-full rounded-full", usedFrac > 0.9 ? "bg-bad" : usedFrac > 0.75 ? "bg-warn" : "bg-accent2")}
                    style={{ width: Math.min(100, usedFrac * 100) + "%" }}
                  />
                </div>
              )}
              {quota.planExpired && (
                <p className="mt-1.5 text-[11px] text-bad">
                  Bitport won't accept new transfers on an expired plan — renew it, or send grabs to qBittorrent below.
                </p>
              )}
            </div>
          )}

          {/* not a <label>: a label's click would land on the first button and flip the choice */}
          <div>
            <div className="mb-1 text-[11.5px] font-medium text-dim">Where do grabs go?</div>
            <Segmented
              value={draft.downloadBackend === "bitport" ? "bitport" : "qbittorrent"}
              onChange={(b) => set({ downloadBackend: b })}
              options={[
                { value: "qbittorrent", label: "Local qBittorrent" },
                { value: "bitport", label: "Bitport cloud" },
              ]}
            />
            <div className="mt-1 text-[11px] text-faint">Applies after Save — every grab path honors it</div>
          </div>

          <div className="space-y-2 rounded-lg border border-line bg-bg2/40 px-3 py-2.5">
            <div className="text-[11.5px] font-medium text-dim">When Bitport finishes a transfer</div>
            <label className="flex cursor-pointer items-start gap-2 text-[12.5px] text-dim">
              <input
                type="checkbox"
                checked={draft.bitportFetchToLocal}
                onChange={(e) => set({ bitportFetchToLocal: e.target.checked })}
                className="mt-0.5 size-3.5 accent-(--color-accent)"
              />
              <span>
                Bring the files to this computer
                <span className="block text-[11px] text-faint">
                  Downloaded over HTTPS into the TV or Movies save path, checksum-verified. Episodes count as downloaded
                  only once the files are here.
                </span>
              </span>
            </label>
            <label
              className={cx(
                "flex cursor-pointer items-start gap-2 text-[12.5px] text-dim",
                !draft.bitportFetchToLocal && "pointer-events-none opacity-45",
              )}
            >
              <input
                type="checkbox"
                checked={draft.bitportFetchToLocal && draft.bitportDeleteAfterFetch}
                disabled={!draft.bitportFetchToLocal}
                onChange={(e) => set({ bitportDeleteAfterFetch: e.target.checked })}
                className="mt-0.5 size-3.5 accent-(--color-accent)"
              />
              <span>
                Remove it from the cloud afterwards
                <span className="block text-[11px] text-faint">Keeps your Bitport quota free. Only after every file verified.</span>
              </span>
            </label>
            {draft.bitportFetchToLocal && (
              <Field label="Fallback download folder" hint="Used when a grab has no TV or Movies save path">
                <TextInput
                  mono
                  value={draft.bitportDownloadDir}
                  onChange={(v) => set({ bitportDownloadDir: v })}
                  placeholder={status.defaultDownloadDir}
                />
              </Field>
            )}
          </div>

          <button
            type="button"
            className="cursor-pointer text-[11px] text-faint underline decoration-line2 underline-offset-2 hover:text-bad"
            onClick={() => void disconnect()}
          >
            disconnect
          </button>
        </div>
      ) : (
        <div className="space-y-2.5">
          <p className="text-[11.5px] leading-snug text-faint">
            Connect once and Trawler can send grabs to your Bitport account instead of the local client — useful when
            your network dislikes BitTorrent. Bitport downloads the torrent; Trawler then pulls the finished files down
            over HTTPS and, by default, clears them from the cloud.
          </p>
          <div className="flex items-center gap-3">
            <Button variant="primary" busy={waiting} onClick={() => void connectFlow()} className="shrink-0 px-2.5 py-1.5 text-[11.5px]">
              <Cloud size={13} />
              {waiting ? "Waiting for your approval…" : "Connect Bitport"}
            </Button>
            {waiting && <span className="text-[11px] text-faint">Approve Trawler on the Bitport page that just opened.</span>}
          </div>
          {!waiting && (
            <button
              type="button"
              className="cursor-pointer text-[10.5px] text-faint underline decoration-line2 underline-offset-2 hover:text-dim"
              onClick={() => setManual((m) => !m)}
            >
              {manual ? "hide the manual option" : "browser on another machine? connect with a code"}
            </button>
          )}
          {manual && !waiting && (
            <div className="space-y-2">
              <p className="text-[11px] leading-snug text-faint">
                Sign in to Bitport on any device, open{" "}
                <a
                  href={status?.getAccessUrl ?? "https://bitport.io/get-access"}
                  target="_blank"
                  rel="noreferrer"
                  className="text-accent2 underline decoration-accent2/40 underline-offset-2"
                >
                  bitport.io/get-access
                </a>{" "}
                and paste the code it shows you.
              </p>
              <div className="flex items-center gap-2">
                <TextInput mono value={code} onChange={setCode} placeholder="paste the code" />
                <Button
                  variant="primary"
                  busy={busy}
                  disabled={!code.trim()}
                  onClick={() => void connectManual()}
                  className="shrink-0 px-2.5 py-1.5 text-[11.5px]"
                >
                  Link
                </Button>
              </div>
            </div>
          )}
        </div>
      )}
    </Card>
  );
}

/** Same frame as the other Settings cards (kept in step with Settings.tsx). */
function Card({ title, sub, children }: { title: string; sub: string; children: React.ReactNode }) {
  return (
    <div className="mt-5 rounded-(--radius-card) border border-line bg-bg1 p-4">
      <div className="mb-3">
        <div className="text-[13px] font-semibold">{title}</div>
        <div className="text-[11.5px] text-faint">{sub}</div>
      </div>
      {children}
    </div>
  );
}
