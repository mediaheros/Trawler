import { useEffect, useState } from "react";
import { FlaskConical, HardDriveDownload, Search, Settings, Sparkles, Tv, Waves } from "lucide-react";
import { inTauri } from "./lib/api";
import { ErrorBoundary } from "./components/ErrorBoundary";
import FirstRun from "./components/FirstRun";
import UpdateBanner from "./components/UpdateBanner";
import { ToastStack, cx } from "./components/ui";
import { useStore, type View } from "./store";
import SearchView from "./views/Search";
import AgentView from "./views/Agent";
import ShowsView from "./views/Shows";
import DownloadsView from "./views/Downloads";
import SettingsView from "./views/Settings";

const NAV: Array<{ view: View; icon: typeof Search; label: string }> = [
  { view: "search", icon: Search, label: "Search" },
  { view: "agent", icon: Sparkles, label: "Agent" },
  { view: "shows", icon: Tv, label: "Shows" },
  { view: "downloads", icon: HardDriveDownload, label: "Downloads" },
  { view: "settings", icon: Settings, label: "Settings" },
];

export default function App() {
  const view = useStore((s) => s.view);
  const setView = useStore((s) => s.setView);
  const goHome = useStore((s) => s.goHome);
  const status = useStore((s) => s.status);
  const config = useStore((s) => s.config);
  const loadConfig = useStore((s) => s.loadConfig);
  const refreshStatus = useStore((s) => s.refreshStatus);
  const [wizardDismissed, setWizardDismissed] = useState(false);
  const showWizard = config !== null && !config.setupCompleted && !wizardDismissed;

  // WebView2 swallows target="_blank" navigations (no new-window handler),
  // so every external link in the app would be a dead click. Route them
  // through the opener plugin instead.
  useEffect(() => {
    if (!inTauri) return;
    const onClick = (e: MouseEvent) => {
      const a = (e.target as HTMLElement | null)?.closest?.("a[target='_blank']") as
        | HTMLAnchorElement
        | null;
      if (!a || !/^https?:/i.test(a.href)) return;
      e.preventDefault();
      void import("@tauri-apps/plugin-opener")
        .then((m) => m.openUrl(a.href))
        .catch(() => useStore.getState().toast("Couldn't open that link", "bad"));
    };
    document.addEventListener("click", onClick);
    return () => document.removeEventListener("click", onClick);
  }, []);

  useEffect(() => {
    void loadConfig().then(refreshStatus);
    const t = setInterval(refreshStatus, 30_000);
    return () => clearInterval(t);
  }, [loadConfig, refreshStatus]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey && !e.shiftKey && !e.altKey) {
        if (e.key === "1") setView("search");
        if (e.key === "2") setView("agent");
        if (e.key === "3") setView("shows");
        if (e.key === "4") setView("downloads");
        if (e.key === "5") setView("settings");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [setView]);

  return (
    <div className="app-bg flex h-full">
      {/* rail */}
      <nav className="flex w-[54px] shrink-0 select-none flex-col items-center border-r border-line bg-bg1/60 py-3">
        <button
          type="button"
          onClick={goHome}
          title="Home"
          className="mb-4 flex size-9 cursor-pointer items-center justify-center rounded-xl bg-gradient-to-br from-accent/25 to-accent2/15 transition-all duration-150 hover:from-accent/40 hover:to-accent2/25 hover:shadow-[0_0_16px_-4px_var(--color-accent)] active:scale-95"
        >
          <Waves size={17} className="text-accent" />
        </button>

        <div className="flex flex-col gap-1">
          {NAV.map(({ view: v, icon: Icon, label }, i) => (
            <button
              key={v}
              type="button"
              onClick={() => setView(v)}
              title={`${label} (Ctrl+${i + 1})`}
              className={cx(
                "relative flex size-9 cursor-pointer items-center justify-center rounded-xl transition-all duration-150",
                view === v
                  ? "bg-bg3 text-accent shadow-[inset_0_0_0_1px_var(--color-line2)]"
                  : "text-faint hover:bg-bg2 hover:text-dim",
              )}
            >
              <Icon size={17} strokeWidth={view === v ? 2.2 : 1.9} />
              {view === v && (
                <span className="absolute -left-[9px] h-4 w-[3px] rounded-full bg-accent" />
              )}
            </button>
          ))}
        </div>

        <div className="mt-auto flex flex-col items-center gap-2 pb-1">
          <StatusDot
            ok={status?.prowlarrOk}
            label={status ? status.prowlarrDetail : "Prowlarr: checking…"}
          />
          <StatusDot
            ok={status?.qbitOk}
            label={status ? status.qbitDetail : "qBittorrent: checking…"}
          />
        </div>
      </nav>

      {/* main */}
      <main className="flex min-w-0 flex-1 flex-col">
        {!inTauri && !new URLSearchParams(window.location.search).has("shot") && (
          <div className="flex items-center justify-center gap-2 border-b border-warn/25 bg-warn/10 px-4 py-1.5 text-[12px] font-medium text-warn">
            <FlaskConical size={13} />
            Demo mode — this is a browser preview with fake data. Real searches happen in the
            Trawler desktop window.
          </div>
        )}
        <div key={view} className="rise-in min-h-0 flex-1">
          <ErrorBoundary label={`The ${NAV.find((n) => n.view === view)?.label ?? "current"} view`}>
            {view === "search" && <SearchView />}
            {view === "agent" && <AgentView />}
            {view === "shows" && <ShowsView />}
            {view === "downloads" && <DownloadsView />}
            {view === "settings" && <SettingsView />}
          </ErrorBoundary>
        </div>
      </main>

      {showWizard && <FirstRun onDone={() => { setWizardDismissed(true); void loadConfig(); }} />}
      <UpdateBanner />
      <ToastStack />
    </div>
  );
}

function StatusDot({ ok, label }: { ok: boolean | undefined; label: string }) {
  return (
    <span title={label} className="flex size-3 items-center justify-center">
      <span
        className={cx(
          "size-[7px] rounded-full transition-colors",
          ok === undefined ? "bg-bg4" : ok ? "bg-ok" : "bg-bad",
        )}
      />
    </span>
  );
}
