import { useEffect, useState } from "react";
import {
  AppWindow,
  Bell,
  BellRing,
  CheckCircle2,
  Magnet,
  Plug,
  PlugZap,
  Plus,
  Radar,
  RefreshCw,
  Sparkles,
  Tv,
  Wand2,
  X,
  XCircle,
} from "lucide-react";
import { api, inTauri, type Config, type NotifyTestOutcome, type QualityProfile } from "../lib/api";
import { sameProfile } from "../lib/format";
import { checkForUpdate, currentVersion } from "../lib/updater";
import { isMac } from "../lib/platform";
import { useStore } from "../store";
import { Button, Chip, Field, NumInput, TextInput, cx } from "../components/ui";
import IndexerManager from "../components/IndexerManager";

type TabId = "connections" | "indexers" | "grabbing" | "following" | "notifications" | "agent" | "app";

const TABS: Array<{ id: TabId; label: string; icon: typeof Plug }> = [
  { id: "connections", label: "Connections", icon: Plug },
  { id: "indexers", label: "Indexers", icon: Radar },
  { id: "grabbing", label: "Grabbing", icon: Magnet },
  { id: "following", label: "Following", icon: Tv },
  { id: "notifications", label: "Notifications", icon: Bell },
  { id: "agent", label: "Agent", icon: Sparkles },
  { id: "app", label: "App", icon: AppWindow },
];

/** Mirror of the backend's config::default_presets() — used only to restore
 *  built-ins a user deleted, since a saved [] persists in config.json. */
const DEFAULT_PRESETS: Array<{ name: string; profile: QualityProfile }> = [
  { name: "Best available", profile: { resolutions: ["2160p", "1080p"], codec: "prefer-x265", maxSizeGb: 0, allowSeasonPacks: true } },
  { name: "1080p quality", profile: { resolutions: ["1080p"], codec: "prefer-x265", maxSizeGb: 8, allowSeasonPacks: true } },
  { name: "1080p balanced", profile: { resolutions: ["1080p", "720p"], codec: "prefer-x265", maxSizeGb: 3, allowSeasonPacks: true } },
  { name: "Space saver", profile: { resolutions: ["720p"], codec: "prefer-x265", maxSizeGb: 1.5, allowSeasonPacks: true } },
];

export default function SettingsView() {
  const config = useStore((s) => s.config);
  const saveConfig = useStore((s) => s.saveConfig);
  const [draft, setDraft] = useState<Config | null>(config);
  const [tab, setTab] = useState<TabId>("connections");
  const [testing, setTesting] = useState(false);
  const [test, setTest] = useState<{ pOk: boolean; p: string; qOk: boolean; q: string } | null>(null);
  const [notifTesting, setNotifTesting] = useState(false);
  const [notifTest, setNotifTest] = useState<NotifyTestOutcome | null>(null);
  const [notifError, setNotifError] = useState<string | null>(null);

  useEffect(() => {
    setDraft(config);
  }, [config]);

  // a test verdict only describes the credentials it actually tested
  useEffect(() => {
    setNotifTest(null);
    setNotifError(null);
  }, [tab, draft?.discordWebhook, draft?.telegramBotToken, draft?.telegramChatId]);

  if (!draft) return null;

  const set = (patch: Partial<Config>) => setDraft({ ...draft, ...patch });
  const dirty = JSON.stringify(draft) !== JSON.stringify(config);

  const runTest = async () => {
    setTesting(true);
    setTest(null);
    try {
      // Test with the *draft* values by saving first if dirty — simplest honest behavior.
      if (dirty && !(await saveConfig(draft))) return; // toast already shown
      const s = await api.testConnections();
      setTest({ pOk: s.prowlarrOk, p: s.prowlarrDetail, qOk: s.qbitOk, q: s.qbitDetail });
    } catch (e) {
      setTest({ pOk: false, p: String(e), qOk: false, q: String(e) });
    } finally {
      setTesting(false);
    }
  };

  const runNotifTest = async () => {
    setNotifTesting(true);
    setNotifTest(null);
    setNotifError(null);
    try {
      if (dirty && !(await saveConfig(draft))) return; // toast already shown
      setNotifTest(await api.notifyTest());
    } catch (e) {
      setNotifError(String(e));
    } finally {
      setNotifTesting(false);
    }
  };

  return (
    <div className="flex h-full flex-col">
      <div className="px-6 pt-5 pb-3">
        <h1 className="text-[16px] font-semibold tracking-tight">Settings</h1>
        <p className="mt-0.5 text-[12px] text-faint">
          Stored locally in{" "}
          <span className="font-mono text-[11px]">
            {isMac ? "~/Library/Application Support/trawler/config.json" : "%APPDATA%\\trawler\\config.json"}
          </span>
        </p>
      </div>

      {/* tabs */}
      <div className="flex gap-0.5 border-b border-line px-6">
        {TABS.map(({ id, label, icon: Icon }) => {
          const active = tab === id;
          return (
            <button
              key={id}
              type="button"
              onClick={() => setTab(id)}
              className={cx(
                "relative flex cursor-pointer items-center gap-1.5 px-3 py-2 text-[12.5px] font-medium transition-colors",
                active ? "text-ink" : "text-faint hover:text-dim",
              )}
            >
              <Icon size={13} className={cx("transition-colors", active && "text-accent")} />
              {label}
              {active && (
                <span className="absolute inset-x-2.5 -bottom-px h-[2px] rounded-full bg-accent shadow-[0_0_8px_var(--color-accent)]" />
              )}
            </button>
          );
        })}
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="max-w-[680px] px-6 pb-6">
        {tab === "connections" && (<>
        <Card title="Prowlarr" sub="Indexer aggregator — Trawler searches through it">
          <div className="grid grid-cols-[1fr_1fr] gap-3">
            <Field label="URL">
              <TextInput mono value={draft.prowlarrUrl} onChange={(v) => set({ prowlarrUrl: v })} placeholder="http://127.0.0.1:9696" />
            </Field>
            <Field label="API key" hint="Prowlarr → Settings → General → API Key">
              <TextInput mono type="password" value={draft.prowlarrApiKey} onChange={(v) => set({ prowlarrApiKey: v })} />
            </Field>
          </div>
          {test && <TestLine ok={test.pOk} text={test.p} />}
        </Card>

        <Card title="qBittorrent" sub="Where grabbed releases are sent">
          <div className="grid grid-cols-[1fr_1fr] gap-3">
            <Field label="WebUI URL">
              <TextInput mono value={draft.qbitUrl} onChange={(v) => set({ qbitUrl: v })} placeholder="http://127.0.0.1:8080" />
            </Field>
            <div className="grid grid-cols-2 gap-3">
              <Field label="Username" hint="Blank if localhost auth is bypassed">
                <TextInput value={draft.qbitUsername} onChange={(v) => set({ qbitUsername: v })} />
              </Field>
              <Field label="Password">
                <TextInput type="password" value={draft.qbitPassword} onChange={(v) => set({ qbitPassword: v })} />
              </Field>
            </div>
          </div>
          {test && <TestLine ok={test.qOk} text={test.q} />}
        </Card>
        </>)}

        {tab === "indexers" && (
        <Card title="Indexers" sub="What Trawler searches — managed inside your Prowlarr">
          <IndexerManager />
        </Card>
        )}

        {tab === "grabbing" && (
        <Card title="Grabbing" sub="How releases are handed over">
          <div className="grid grid-cols-2 gap-3">
            <Field label="Category tag" hint="Lets the Downloads view show only Trawler's grabs">
              <TextInput mono value={draft.qbitCategory} onChange={(v) => set({ qbitCategory: v })} />
            </Field>
            <div className="flex items-end pb-1">
              <label className="flex cursor-pointer items-center gap-2 text-[12.5px] text-dim">
                <input
                  type="checkbox"
                  checked={draft.addPaused}
                  onChange={(e) => set({ addPaused: e.target.checked })}
                  className="size-3.5 accent-(--color-accent)"
                />
                Add torrents paused
              </label>
            </div>
            <Field label="Movies save path" hint="Blank = qBittorrent default">
              <TextInput mono value={draft.savePathMovies} onChange={(v) => set({ savePathMovies: v })} placeholder={isMac ? "/Volumes/Media/Movies" : "D:\\Media\\Movies"} />
            </Field>
            <Field label="TV save path" hint="Blank = qBittorrent default">
              <TextInput mono value={draft.savePathTv} onChange={(v) => set({ savePathTv: v })} placeholder={isMac ? "/Volumes/Media/TV" : "D:\\Media\\TV"} />
            </Field>
            <Field
              label="Seeding after download"
              hint={draft.seedPolicy === "none" ? "Note: not seeding hurts your ratio on private trackers" : undefined}
            >
              <select
                value={draft.seedPolicy}
                onChange={(e) => set({ seedPolicy: e.target.value as Config["seedPolicy"] })}
                className="w-full cursor-pointer rounded-(--radius-btn) border border-line2 bg-bg1 px-2 py-1.5 text-[12.5px] text-ink focus:border-accent/50"
              >
                <option value="default">Seed per qBittorrent's settings</option>
                <option value="ratio">Seed to a ratio, then stop</option>
                <option value="none">Don't seed — stop when complete</option>
              </select>
            </Field>
            {draft.seedPolicy === "ratio" && (
              <Field label="Stop at ratio" hint="1.0 = give back what you took">
                <NumInput value={draft.seedRatio} min={0} onCommit={(n) => set({ seedRatio: n })} />
              </Field>
            )}
          </div>
        </Card>
        )}

        {tab === "following" && (
        <Card title="Following" sub="How followed shows are grabbed by the scheduler">
          <PresetsRow
            presets={draft.qualityPresets}
            current={draft.defaultQuality}
            onApply={(profile) => set({ defaultQuality: profile })}
            onSave={(name) =>
              set({
                qualityPresets: [
                  ...draft.qualityPresets.filter((p) => p.name !== name),
                  { name, profile: draft.defaultQuality },
                ],
              })
            }
            onDelete={(name) =>
              set({ qualityPresets: draft.qualityPresets.filter((p) => p.name !== name) })
            }
            onRestore={() =>
              set({
                qualityPresets: [
                  ...draft.qualityPresets,
                  ...DEFAULT_PRESETS.filter((d) => !draft.qualityPresets.some((p) => p.name === d.name)),
                ],
              })
            }
          />
          <div className="mb-3">
            <div className="mb-1.5 text-[11.5px] font-medium text-dim">Allowed resolutions</div>
            <div className="flex gap-1.5">
              {["2160p", "1080p", "720p", "480p"].map((r) => {
                const active = draft.defaultQuality.resolutions.includes(r);
                return (
                  <Chip
                    key={r}
                    active={active}
                    onClick={() =>
                      set({
                        defaultQuality: {
                          ...draft.defaultQuality,
                          resolutions: active
                            ? draft.defaultQuality.resolutions.filter((x) => x !== r)
                            : [...draft.defaultQuality.resolutions, r],
                        },
                      })
                    }
                  >
                    {r}
                  </Chip>
                );
              })}
            </div>
            <div className="mt-1 text-[11px] text-faint">
              Releases with an unrecognized resolution still qualify; ranking prefers the good ones.
            </div>
          </div>
          <div className="grid grid-cols-3 gap-3">
            <Field label="Codec preference">
              <select
                value={draft.defaultQuality.codec}
                onChange={(e) =>
                  set({ defaultQuality: { ...draft.defaultQuality, codec: e.target.value as Config["defaultQuality"]["codec"] } })
                }
                className="w-full cursor-pointer rounded-(--radius-btn) border border-line2 bg-bg1 px-2 py-1.5 text-[12.5px] text-ink focus:border-accent/50"
              >
                <option value="prefer-x265">Prefer x265</option>
                <option value="prefer-x264">Prefer x264</option>
                <option value="any">No preference</option>
              </select>
            </Field>
            <Field label="Max size per episode" hint="0 = no limit. Packs get size × episodes.">
              <NumInput
                value={draft.defaultQuality.maxSizeGb}
                min={0}
                onCommit={(n) => set({ defaultQuality: { ...draft.defaultQuality, maxSizeGb: n } })}
              />
            </Field>
            <Field label="Check every (minutes)" hint="5 minutes minimum">
              <NumInput
                value={draft.schedulerMinutes}
                integer
                min={5}
                max={1440}
                onCommit={(n) => set({ schedulerMinutes: n })}
              />
            </Field>
          </div>
          <div className="mt-3 grid grid-cols-2 items-end gap-3 rounded-(--radius-card) border border-line bg-bg2/50 p-3">
            <label className="flex cursor-pointer items-center gap-2 text-[12.5px] text-dim">
              <input
                type="checkbox"
                checked={draft.rssEnabled}
                onChange={(e) => set({ rssEnabled: e.target.checked })}
                className="size-3.5 accent-(--color-accent)"
              />
              <span>
                <span className="font-medium text-ink">Instant grabs (RSS sweep)</span>
                <span className="block text-[11px] text-faint">
                  Watch every indexer's newest uploads and grab wanted episodes and brief
                  matches within minutes of release
                </span>
              </span>
            </label>
            <Field label="Sweep every (minutes)" hint="10 minimum — be kind to indexers">
              <NumInput
                value={draft.rssMinutes}
                integer
                min={10}
                max={120}
                onCommit={(n) => set({ rssMinutes: n })}
              />
            </Field>
          </div>
          <div className="mt-3 grid grid-cols-2 items-start gap-3 rounded-(--radius-card) border border-line bg-bg2/50 p-3">
            <label className="flex cursor-pointer items-start gap-2 text-[12.5px] text-dim">
              <input
                type="checkbox"
                checked={draft.upgradeScoutEnabled}
                onChange={(e) => set({ upgradeScoutEnabled: e.target.checked })}
                className="mt-0.5 size-3.5 accent-(--color-accent)"
              />
              <span>
                <span className="font-medium text-ink">Upgrade scout</span>
                <span className="block text-[11px] text-faint">
                  Once a week, look for better-quality copies of recent downloads.
                  Proposes only — never grabs on its own.
                </span>
              </span>
            </label>
            {draft.upgradeScoutEnabled && (
              <div className="space-y-2">
                <Field label="Look back (days)" hint="1–365 · how recent a download must be to qualify">
                  <NumInput
                    value={draft.upgradeWindowDays}
                    integer
                    min={1}
                    max={365}
                    onCommit={(n) => set({ upgradeWindowDays: n })}
                  />
                </Field>
                <ScanNowButton />
              </div>
            )}
          </div>
          <div className="mt-3 flex items-center gap-6">
            <label className="flex cursor-pointer items-center gap-2 text-[12.5px] text-dim">
              <input
                type="checkbox"
                checked={draft.defaultQuality.allowSeasonPacks}
                onChange={(e) =>
                  set({ defaultQuality: { ...draft.defaultQuality, allowSeasonPacks: e.target.checked } })
                }
                className="size-3.5 accent-(--color-accent)"
              />
              Prefer season packs when several episodes are missing
            </label>
          </div>
        </Card>
        )}

        {tab === "notifications" && (<>
        <Card title={isMac ? "macOS" : "Windows"} sub="Native notifications from the app itself">
          <label className="flex cursor-pointer items-center gap-2 text-[12.5px] text-dim">
            <input
              type="checkbox"
              checked={draft.notifyOnGrab}
              onChange={(e) => set({ notifyOnGrab: e.target.checked })}
              className="size-3.5 accent-(--color-accent)"
            />
            System notification when something is grabbed
          </label>
        </Card>

        <Card title="Discord" sub="Push every event to a channel via webhook">
          <Field label="Webhook URL" hint="Server Settings → Integrations → Webhooks → New Webhook, then copy its URL">
            <TextInput
              mono
              type="password"
              value={draft.discordWebhook}
              onChange={(v) => set({ discordWebhook: v })}
              placeholder="https://discord.com/api/webhooks/…"
            />
          </Field>
          {notifTest?.discord != null && <TestLine ok={notifTest.discordOk} text={notifTest.discord} />}
        </Card>

        <Card title="Telegram" sub="Messages from your own bot, straight to your phone">
          <div className="grid grid-cols-[1fr_1fr] gap-3">
            <Field label="Bot token" hint="Create a bot with @BotFather — it replies with the token">
              <TextInput
                mono
                type="password"
                value={draft.telegramBotToken}
                onChange={(v) => set({ telegramBotToken: v })}
                placeholder="123456789:AA…"
              />
            </Field>
            <Field label="Chat ID" hint="Message your bot once, then open api.telegram.org/bot<token>/getUpdates">
              <TextInput
                mono
                value={draft.telegramChatId}
                onChange={(v) => set({ telegramChatId: v })}
                placeholder="123456789"
              />
            </Field>
          </div>
          {notifTest?.telegram != null && <TestLine ok={notifTest.telegramOk} text={notifTest.telegram} />}
        </Card>

        <Card title="Events" sub="What Discord and Telegram get told about">
          <div className="grid grid-cols-2 gap-x-6 gap-y-2.5">
            {(
              [
                ["notifyGrabs", "Grabs", "A release was sent to qBittorrent"],
                ["notifyCompletions", "Completed downloads", "Something finished downloading"],
                ["notifyProposals", "Proposals", "The agent wants your approval on a release"],
                ["notifyErrors", "Problems", "Dead swarms and failed grabs"],
              ] as const
            ).map(([key, label, hint]) => (
              <label key={key} className="flex cursor-pointer items-start gap-2 text-[12.5px] text-dim">
                <input
                  type="checkbox"
                  checked={draft[key]}
                  onChange={(e) => set({ [key]: e.target.checked })}
                  className="mt-0.5 size-3.5 accent-(--color-accent)"
                />
                <span>
                  <span className="font-medium text-ink">{label}</span>
                  <span className="block text-[11px] text-faint">{hint}</span>
                </span>
              </label>
            ))}
          </div>
        </Card>
        </>)}

        {tab === "agent" && (
        <Card title="Agent" sub="The AI that runs briefs and chat — your own model endpoint">
          <div className="grid grid-cols-[1fr_1fr] gap-3">
            <Field label="OpenAI-compatible endpoint" hint="Ollama works out of the box">
              <TextInput mono value={draft.agentBaseUrl} onChange={(v) => set({ agentBaseUrl: v })} placeholder="http://127.0.0.1:11434" />
            </Field>
            <Field label="Model">
              <ModelSelect value={draft.agentModel} onChange={(v) => set({ agentModel: v })} />
            </Field>
          </div>
          <div className="mt-3 grid grid-cols-2 gap-3">
            <Field
              label="Dead-swarm medic"
              hint="When a grab sits peerless for an hour, find a live release of the same content"
            >
              <select
                value={draft.medicMode}
                onChange={(e) => set({ medicMode: e.target.value as Config["medicMode"] })}
                className="w-full cursor-pointer rounded-(--radius-btn) border border-line2 bg-bg1 px-2 py-1.5 text-[12.5px] text-ink focus:border-accent/50"
              >
                <option value="propose">Propose a replacement for approval</option>
                <option value="auto">Swap automatically</option>
                <option value="off">Off — just pause and tell me</option>
              </select>
            </Field>
            <Field label="Free-disk floor" hint="Agent refuses to grab below this">
              <div className="flex items-center gap-1.5">
                <NumInput value={draft.agentMinFreeDiskGb} min={0} onCommit={(n) => set({ agentMinFreeDiskGb: n })} />
                <span className="shrink-0 text-[12px] text-dim">GB</span>
              </div>
            </Field>
          </div>
          <label className="mt-3 flex cursor-pointer items-center gap-2 text-[12.5px] text-dim">
            <input
              type="checkbox"
              checked={draft.agentEnabled}
              onChange={(e) => set({ agentEnabled: e.target.checked })}
              className="size-3.5 accent-(--color-accent)"
            />
            Agent enabled (chat, scheduled briefs, medic)
          </label>
        </Card>
        )}

        {tab === "app" && (
        <Card title="App" sub="Tray behavior and startup">
          <div className="flex items-center gap-6">
            <label className="flex cursor-pointer items-center gap-2 text-[12.5px] text-dim">
              <input
                type="checkbox"
                checked={draft.closeToTray}
                onChange={(e) => set({ closeToTray: e.target.checked })}
                className="size-3.5 accent-(--color-accent)"
              />
              Closing the window hides to tray (keeps the scheduler running)
            </label>
            <AutostartToggle />
          </div>
          <div className="mt-4 border-t border-line/60 pt-3">
            <UpdateCheckRow />
          </div>
          <div className="mt-3 border-t border-line/60 pt-3">
            <Button
              onClick={async () => {
                await saveConfig({ ...draft, setupCompleted: false });
              }}
              className="text-[12px]"
              title="Re-check qBittorrent and Prowlarr, install anything missing"
            >
              <Wand2 size={12} /> Run the setup wizard again
            </Button>
          </div>
        </Card>
        )}
        </div>
      </div>

      {/* pinned action bar */}
      <div className="border-t border-line bg-bg1/60 px-6 py-3">
        <div className="flex max-w-[680px] items-center gap-2.5">
          {tab === "indexers" ? (
            <span className="text-[11.5px] text-faint">
              Indexer changes talk to Prowlarr directly and apply instantly — nothing to save here.
            </span>
          ) : (
          <Button variant="primary" disabled={!dirty} onClick={() => saveConfig(draft)}>
            Save changes
          </Button>
          )}
          {tab === "connections" && (
            <Button onClick={runTest} busy={testing}>
              <PlugZap size={13} />
              {dirty ? "Save & test connections" : "Test connections"}
            </Button>
          )}
          {tab === "notifications" && (
            <Button
              onClick={runNotifTest}
              busy={notifTesting}
              disabled={!draft.discordWebhook.trim() && !(draft.telegramBotToken.trim() && draft.telegramChatId.trim())}
              title="Sends a test message to every configured channel"
            >
              <BellRing size={13} />
              {dirty ? "Save & send test" : "Send test notification"}
            </Button>
          )}
          {tab === "notifications" && notifError && (
            <span className="min-w-0 truncate text-[11.5px] text-bad" title={notifError}>{notifError}</span>
          )}
          {dirty && <span className="ml-auto text-[11.5px] text-warn">Unsaved changes</span>}
        </div>
      </div>
    </div>
  );
}

function ScanNowButton() {
  const toast = useStore((s) => s.toast);
  const [busy, setBusy] = useState(false);
  return (
    <Button
      variant="ghost"
      busy={busy}
      disabled={!inTauri}
      className="text-[11.5px]"
      title={inTauri ? "Run one scout pass right now" : "Only meaningful in the desktop app"}
      onClick={async () => {
        setBusy(true);
        try {
          await api.upgradeScanNow();
          toast("Upgrade scan started — proposals land in the Agent view", "ok");
        } catch (e) {
          toast(String(e), "bad");
        } finally {
          setBusy(false);
        }
      }}
    >
      Scan for upgrades now
    </Button>
  );
}

function UpdateCheckRow() {
  const setPendingUpdate = useStore((s) => s.setPendingUpdate);
  const setUpdateDismissedVersion = useStore((s) => s.setUpdateDismissedVersion);
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const [version, setVersion] = useState<string>("…");

  useEffect(() => {
    void currentVersion().then(setVersion);
  }, []);

  const check = async () => {
    setChecking(true);
    setResult(null);
    try {
      const u = await checkForUpdate();
      if (u) {
        // hand it to the banner and un-dismiss so the card really appears
        setPendingUpdate(u);
        setUpdateDismissedVersion(null);
        setResult(`Trawler ${u.version} is available — see the update card`);
      } else {
        setResult("You're on the latest version");
      }
    } catch (e) {
      setResult(`Couldn't check: ${e}`);
    } finally {
      setChecking(false);
    }
  };

  return (
    <div className="flex items-center gap-3">
      <Button onClick={check} busy={checking} className="text-[12px]" disabled={!inTauri}>
        <RefreshCw size={12} /> Check for updates
      </Button>
      <span className="text-[11.5px] text-faint">
        {result ?? `Version ${version}${inTauri ? "" : " · updates apply in the desktop app"}`}
      </span>
    </div>
  );
}

function ModelSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const [models, setModels] = useState<string[] | null>(null);
  useEffect(() => {
    api.agentModels().then(setModels).catch(() => setModels(null));
  }, []);
  if (!models || !models.length) {
    return <TextInput mono value={value} onChange={onChange} placeholder="model name" />;
  }
  const options = models.includes(value) ? models : [value, ...models];
  return (
    <select
      value={value}
      onChange={(e) => onChange(e.target.value)}
      className="w-full cursor-pointer rounded-(--radius-btn) border border-line2 bg-bg1 px-2 py-1.5 font-mono text-[12px] text-ink focus:border-accent/50"
    >
      {options.map((m) => (
        <option key={m} value={m}>{m}</option>
      ))}
    </select>
  );
}

function AutostartToggle() {
  const toast = useStore((s) => s.toast);
  const [enabled, setEnabled] = useState<boolean | null>(null);

  useEffect(() => {
    api.getAutostart().then(setEnabled).catch(() => setEnabled(false));
  }, []);

  const flip = async (want: boolean) => {
    try {
      await api.setAutostart(want);
      setEnabled(want);
      toast(want ? (isMac ? "Trawler will start at login" : "Trawler will start with Windows") : "Autostart disabled", "ok");
    } catch (e) {
      toast(String(e), "bad");
    }
  };

  return (
    <label
      className={cx(
        "flex cursor-pointer items-center gap-2 text-[12.5px] text-dim",
        enabled === null && "opacity-50",
      )}
      title={inTauri ? undefined : "Only meaningful in the desktop app"}
    >
      <input
        type="checkbox"
        checked={enabled ?? false}
        disabled={enabled === null}
        onChange={(e) => flip(e.target.checked)}
        className="size-3.5 accent-(--color-accent)"
      />
      {isMac ? "Start at login" : "Start with Windows"}
    </label>
  );
}

function PresetsRow({
  presets,
  current,
  onApply,
  onSave,
  onDelete,
  onRestore,
}: {
  presets: Array<{ name: string; profile: QualityProfile }>;
  current: QualityProfile;
  onApply: (p: QualityProfile) => void;
  onSave: (name: string) => void;
  onDelete: (name: string) => void;
  onRestore: () => void;
}) {
  const [naming, setNaming] = useState(false);
  const [name, setName] = useState("");
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const anyActive = presets.some((p) => sameProfile(p.profile, current));
  const missingDefaults = DEFAULT_PRESETS.filter((d) => !presets.some((p) => p.name === d.name));

  const commit = () => {
    const trimmed = name.trim();
    if (trimmed) onSave(trimmed.slice(0, 30));
    setNaming(false);
    setName("");
  };

  return (
    <div className="mb-4">
      <div className="mb-1.5 flex items-baseline gap-2">
        <span className="text-[11.5px] font-medium text-dim">Presets</span>
        <span className="text-[10.5px] text-faint">one click sets everything below</span>
      </div>
      <div className="flex flex-wrap items-center gap-1.5">
        {presets.map((p) => {
          const active = sameProfile(p.profile, current);
          return (
            <span key={p.name} className="group/preset relative">
              <Chip active={active} onClick={() => onApply(p.profile)} className="pr-5">
                {p.name}
              </Chip>
              <button
                type="button"
                aria-label={pendingDelete === p.name ? `Confirm deleting ${p.name}` : `Delete preset ${p.name}`}
                title={pendingDelete === p.name ? "Click again to delete" : `Delete preset ${p.name}`}
                onClick={() => {
                  if (pendingDelete === p.name) {
                    onDelete(p.name);
                    setPendingDelete(null);
                  } else {
                    setPendingDelete(p.name);
                  }
                }}
                onBlur={() => setPendingDelete((cur) => (cur === p.name ? null : cur))}
                className={cx(
                  "absolute right-1 top-1/2 -translate-y-1/2 cursor-pointer rounded p-0.5 transition-all focus-visible:opacity-100 group-hover/preset:opacity-100",
                  pendingDelete === p.name ? "scale-125 text-bad opacity-100" : "text-faint opacity-0 hover:text-bad",
                )}
              >
                <X size={9} />
              </button>
            </span>
          );
        })}
        {missingDefaults.length > 0 && (
          <button
            type="button"
            onClick={onRestore}
            title="Bring back the built-in presets you deleted"
            className="cursor-pointer rounded-full px-2 py-1 text-[10.5px] text-faint transition-colors hover:text-dim"
          >
            restore defaults
          </button>
        )}
        {naming ? (
          <span className="flex items-center gap-1">
            <input
              autoFocus
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commit();
                if (e.key === "Escape") { setNaming(false); setName(""); }
              }}
              placeholder="Preset name"
              className="w-[130px] rounded-(--radius-btn) border border-line2 bg-bg1 px-2 py-1 text-[11.5px] text-ink outline-none focus:border-accent/50"
            />
            <Button onClick={commit} className="px-2 py-1 text-[11px]">Save</Button>
          </span>
        ) : (
          !anyActive && (
            <button
              type="button"
              onClick={() => setNaming(true)}
              className="flex cursor-pointer items-center gap-1 rounded-full border border-dashed border-line2 px-2.5 py-1 text-[11px] text-faint transition-colors hover:border-accent/50 hover:text-dim"
            >
              <Plus size={10} /> Save current as preset
            </button>
          )
        )}
      </div>
    </div>
  );
}

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

function TestLine({ ok, text }: { ok: boolean; text: string }) {
  return (
    <div
      className={cx(
        "mt-3 flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-[12px]",
        ok ? "bg-ok/8 text-ok" : "bg-bad/8 text-bad",
      )}
    >
      {ok ? <CheckCircle2 size={13} /> : <XCircle size={13} />}
      <span className="min-w-0 truncate" title={text}>{text}</span>
    </div>
  );
}
