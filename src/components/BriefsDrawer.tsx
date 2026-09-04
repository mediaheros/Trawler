import { useEffect, useRef, useState } from "react";
import {
  ChevronDown,
  ClipboardList,
  Loader2,
  Pencil,
  Play,
  Plus,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-react";
import { api, type BriefRow, type HuntPlan } from "../lib/api";
import { useStore } from "../store";
import { Button, Chip, CloseBtn, Field, NumInput, TextInput, cx } from "./ui";

export default function BriefsDrawer({ onClose }: { onClose: () => void }) {
  // per-field selectors: whole-store subscription re-renders on every toast
  const briefs = useStore((s) => s.briefs);
  const loadBriefs = useStore((s) => s.loadBriefs);
  const toast = useStore((s) => s.toast);
  const [editing, setEditing] = useState<BriefRow | "new" | null>(null);
  const [running, setRunning] = useState<number | null>(null);
  const [toggling, setToggling] = useState<number | null>(null);

  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const poll = async () => {
      await loadBriefs();
      if (active) timer = setTimeout(poll, 20_000);
    };
    void poll();
    return () => {
      active = false;
      if (timer !== null) clearTimeout(timer);
    };
  }, [loadBriefs]);

  const runNow = async (b: BriefRow) => {
    setRunning(b.id);
    try {
      await api.briefRunNow(b.id);
      toast(`Running "${b.name}" — proposals will appear in the Agent chat`, "info");
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setTimeout(() => {
        setRunning(null);
        void loadBriefs();
      }, 4000);
    }
  };

  const toggle = async (b: BriefRow) => {
    // two quick clicks both computed !enabled from the same stale row and
    // saved the same value twice - one save at a time per brief
    if (toggling === b.id) return;
    setToggling(b.id);
    try {
      await api.briefSave({
        id: b.id, name: b.name, prompt: b.prompt, plan: JSON.parse(b.planJson) as HuntPlan,
        cadenceMinutes: b.cadenceMinutes, mode: b.mode, maxGrabsPerRun: b.maxGrabsPerRun,
        maxGbPerRun: b.maxGbPerRun, maxGbPerDay: b.maxGbPerDay, enabled: !b.enabled,
      });
      await loadBriefs();
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setToggling(null);
    }
  };

  return (
    <div className="flex w-[340px] shrink-0 flex-col border-l border-line bg-bg1/60">
      <div className="flex items-center justify-between border-b border-line px-4 py-3">
        <div className="flex items-center gap-2">
          <ClipboardList size={14} className="text-accent" />
          <span className="text-[13px] font-semibold">Standing briefs</span>
        </div>
        <div className="flex items-center gap-1">
          <Button variant="primary" onClick={() => setEditing("new")} className="px-2 py-1 text-[11.5px]">
            <Plus size={12} /> New
          </Button>
          <CloseBtn onClick={onClose} />
        </div>
      </div>

      <div className="min-h-0 flex-1 space-y-2 overflow-y-auto p-3">
        {briefs.length === 0 ? (
          <div className="px-2 py-8 text-center text-[12px] leading-relaxed text-faint">
            A brief is a hunt that runs on a schedule:{" "}
            <span className="text-dim">"UFC numbered events, main card, 1080p, under 6 GB."</span>
            <br />
            <br />
            Create one here, or just describe it in the chat.
          </div>
        ) : (
          briefs.map((b) => (
            <BriefCard
              key={b.id}
              b={b}
              running={running === b.id}
              toggling={toggling === b.id}
              onRun={() => runNow(b)}
              onEdit={() => setEditing(b)}
              onToggle={() => toggle(b)}
            />
          ))
        )}
      </div>

      {editing && (
        <BriefEditor
          existing={editing === "new" ? null : editing}
          onClose={() => {
            setEditing(null);
            void loadBriefs();
          }}
        />
      )}
    </div>
  );
}

function BriefCard({
  b,
  running,
  toggling,
  onRun,
  onEdit,
  onToggle,
}: {
  b: BriefRow;
  running: boolean;
  toggling: boolean;
  onRun: () => void;
  onEdit: () => void;
  onToggle: () => void;
}) {
  const [open, setOpen] = useState(false);
  const nextRun = b.enabled
    ? Math.max(0, b.lastRunAt + b.cadenceMinutes * 60 - Date.now() / 1000)
    : null;

  return (
    <div className={cx("rounded-(--radius-card) border border-line bg-bg1 transition-colors hover:border-line2", !b.enabled && "opacity-60")}>
      <div className="px-3 pt-2.5">
        <div className="flex items-center gap-2">
          <span className="min-w-0 flex-1 truncate text-[12.5px] font-semibold">{b.name}</span>
          <span
            className={cx(
              "rounded-[5px] px-1.5 py-[1px] text-[10.5px] font-medium uppercase tracking-wide",
              b.mode === "auto" ? "bg-accent/15 text-accent" : "bg-warn/12 text-warn",
            )}
          >
            {b.mode}
          </span>
        </div>
        <div className="mt-1 flex items-center gap-2 text-[10.5px] text-faint">
          <span>every {b.cadenceMinutes >= 60 ? `${Math.round(b.cadenceMinutes / 60)}h` : `${b.cadenceMinutes}m`}</span>
          <span>· ≤{b.maxGrabsPerRun} grabs · ≤{b.maxGbPerRun} GB/run</span>
          {nextRun !== null && (
            <span className="ml-auto">{nextRun < 60 ? "due now" : `next in ${Math.round(nextRun / 60)}m`}</span>
          )}
        </div>
        {b.pausedReason && (
          <div className="mt-1.5 flex items-center gap-1.5 rounded-lg bg-bad/8 px-2 py-1 text-[10.5px] text-bad">
            <TriangleAlert size={11} /> {b.pausedReason}
          </div>
        )}
        {b.failStreak >= 3 && !b.pausedReason && (
          <div className="mt-1.5 flex items-center gap-1.5 rounded-lg bg-warn/8 px-2 py-1 text-[10.5px] text-warn">
            <TriangleAlert size={11} /> {b.failStreak} failed runs — cooling off
          </div>
        )}
        {b.lastReport && (
          <button
            type="button"
            onClick={() => setOpen(!open)}
            className="mt-1.5 flex w-full cursor-pointer items-start gap-1 text-left text-[11px] leading-snug text-dim hover:text-ink"
          >
            <ChevronDown size={11} className={cx("mt-0.5 shrink-0 transition-transform", !open && "-rotate-90")} />
            <span className={cx(!open && "line-clamp-1")}>{b.lastReport}</span>
          </button>
        )}
      </div>
      <div className="mt-2 flex items-center gap-0.5 border-t border-line/60 px-2 py-1">
        <button
          type="button"
          onClick={onRun}
          disabled={running}
          title="Run now"
          className="cursor-pointer rounded-md p-1.5 text-faint hover:bg-bg3 hover:text-accent disabled:opacity-40"
        >
          {running ? <Loader2 size={12.5} className="spin" /> : <Play size={12.5} />}
        </button>
        <button type="button" onClick={onEdit} title="Edit" className="cursor-pointer rounded-md p-1.5 text-faint hover:bg-bg3 hover:text-ink">
          <Pencil size={12.5} />
        </button>
        <label className="ml-auto flex cursor-pointer items-center gap-1.5 pr-1 text-[10.5px] text-faint">
          <input type="checkbox" checked={b.enabled} disabled={toggling} onChange={onToggle} className="size-3 accent-(--color-accent)" />
          Enabled
        </label>
      </div>
    </div>
  );
}

// ---------------- editor ----------------

function BriefEditor({ existing, onClose }: { existing: BriefRow | null; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [name, setName] = useState(existing?.name ?? "");
  const [prompt, setPrompt] = useState(existing?.prompt ?? "");
  const [plan, setPlan] = useState<HuntPlan | null>(() => {
    if (!existing) return null;
    try {
      return JSON.parse(existing.planJson) as HuntPlan;
    } catch {
      return null; // a corrupt plan re-compiles from the prompt instead of crashing
    }
  });
  const [compiledPrompt, setCompiledPrompt] = useState<string | null>(
    existing?.planJson ? existing.prompt.trim() : null,
  );
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [cadence, setCadence] = useState(existing?.cadenceMinutes ?? 180);
  const [mode, setMode] = useState<"propose" | "auto">(existing?.mode ?? "propose");
  const [maxGrabs, setMaxGrabs] = useState(existing?.maxGrabsPerRun ?? 2);
  const [maxGb, setMaxGb] = useState(existing?.maxGbPerRun ?? 12);
  const [compiling, setCompiling] = useState(false);
  const [saving, setSaving] = useState(false);
  const promptRef = useRef(prompt);
  promptRef.current = prompt;
  const compileSeq = useRef(0);
  const savingRef = useRef(false);

  const compile = async () => {
    const source = prompt.trim();
    if (!source) return;
    const mine = ++compileSeq.current;
    setCompiling(true);
    try {
      const compiled = await api.compileBrief(source);
      if (mine === compileSeq.current && promptRef.current.trim() === source) {
        setPlan(compiled);
        setCompiledPrompt(source);
      }
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      if (mine === compileSeq.current) setCompiling(false);
    }
  };

  const save = async () => {
    const source = prompt.trim();
    if (!plan || compiledPrompt !== source || compiling || savingRef.current) return;
    savingRef.current = true;
    setSaving(true);
    try {
      await api.briefSave({
        id: existing?.id ?? null,
        name: name.trim() || prompt.slice(0, 40),
        prompt: source,
        plan,
        cadenceMinutes: cadence,
        mode,
        maxGrabsPerRun: maxGrabs,
        maxGbPerRun: maxGb,
        maxGbPerDay: existing?.maxGbPerDay ?? maxGb * 2.5,
        enabled: true,
      });
      toast(existing ? "Brief updated" : `Brief created — it starts in ${mode} mode`, "ok");
      onClose();
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div
      className="fixed inset-0 z-40 flex items-start justify-center bg-black/60 pt-[10vh] backdrop-blur-[2px]"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
      role="dialog"
      aria-modal="true"
    >
      <div className="toast-in w-[560px] max-w-[92vw] overflow-hidden rounded-(--radius-card) border border-line2 bg-bg1 shadow-[0_24px_80px_-20px_rgba(0,0,0,0.9)]">
        <div className="border-b border-line px-4 py-3 text-[13px] font-semibold">
          {existing ? "Edit brief" : "New standing brief"}
        </div>
        <div className="max-h-[64vh] space-y-3 overflow-y-auto p-4">
          <Field label="What should the agent watch for?">
            <textarea
              value={prompt}
              onChange={(e) => {
                ++compileSeq.current;
                setCompiling(false);
                setPrompt(e.target.value);
                setPlan(null);
                setCompiledPrompt(null);
              }}
              rows={2}
              placeholder={'e.g. "UFC numbered events, main card only, 1080p, under 6 GB"'}
              spellCheck={false}
              className="w-full resize-none rounded-(--radius-btn) border border-line2 bg-bg1 px-2.5 py-2 text-[12.5px] text-ink outline-none placeholder:text-faint focus:border-accent/50"
            />
          </Field>
          <div className="flex items-center gap-2">
            <Button onClick={compile} busy={compiling} disabled={!prompt.trim()} className="text-[12px]">
              {plan ? "Recompile plan" : "Compile plan"}
            </Button>
            <span className="text-[11px] text-faint">
              The agent turns your words into hard rules you confirm below.
            </span>
          </div>

          {plan && (
            <div className="rise-in rounded-(--radius-card) border border-accent/20 bg-accent-soft/40 p-3">
              <div className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-accent">
                Compiled plan — enforced on every grab
              </div>
              <PlanEditor plan={plan} onChange={setPlan} />
            </div>
          )}

          <div className="grid grid-cols-2 gap-3">
            <Field label="Name">
              <TextInput value={name} onChange={setName} placeholder="UFC main cards" />
            </Field>
            <Field label="Check every">
              <select
                value={cadence}
                onChange={(e) => setCadence(Number(e.target.value))}
                className="w-full cursor-pointer rounded-(--radius-btn) border border-line2 bg-bg1 px-2 py-1.5 text-[12.5px] text-ink focus:border-accent/50"
              >
                <option value={60}>hour</option>
                <option value={180}>3 hours</option>
                <option value={360}>6 hours</option>
                <option value={720}>12 hours</option>
                <option value={1440}>day</option>
              </select>
            </Field>
            <Field label="Max grabs per run">
              <NumInput value={maxGrabs} integer min={1} max={10} onCommit={setMaxGrabs} />
            </Field>
            <Field label="Max GB per run">
              <NumInput value={maxGb} min={1} max={200} onCommit={setMaxGb} />
            </Field>
          </div>

          <div className="flex items-center gap-3 rounded-(--radius-card) border border-line bg-bg2/50 p-3">
            <div className="flex flex-1 flex-col gap-0.5">
              <span className="text-[12px] font-medium">
                {mode === "propose" ? "Propose mode" : "Auto mode"}
              </span>
              <span className="text-[11px] text-faint">
                {mode === "propose"
                  ? "The agent asks — proposals appear in the chat for one-click approval."
                  : "The agent grabs qualifying releases directly, within the caps."}
              </span>
            </div>
            <button
              type="button"
              onClick={() => setMode(mode === "propose" ? "auto" : "propose")}
              className={cx(
                "relative h-5 w-9 cursor-pointer rounded-full transition-colors",
                mode === "auto" ? "bg-accent" : "bg-bg4",
              )}
            >
              <span
                className={cx(
                  "absolute top-0.5 size-4 rounded-full bg-white transition-all",
                  mode === "auto" ? "left-[18px]" : "left-0.5",
                )}
              />
            </button>
          </div>
        </div>
        <div className="flex items-center justify-between border-t border-line px-4 py-3">
          {existing ? (
            <Button
              variant="danger"
              onClick={async () => {
                if (!confirmDelete) {
                  setConfirmDelete(true);
                  return;
                }
                setDeleting(true);
                try {
                  await api.briefDelete(existing.id);
                  onClose();
                } catch (e) {
                  toast(String(e), "bad");
                  setConfirmDelete(false);
                } finally {
                  setDeleting(false);
                }
              }}
              className="px-2.5 py-1 text-[11.5px]"
            >
              <Trash2 size={12} /> {deleting ? "Deleting…" : confirmDelete ? "Really delete?" : "Delete"}
            </Button>
          ) : (
            <span />
          )}
          <div className="flex gap-2">
            <Button variant="ghost" onClick={onClose} className="text-[12px]">Cancel</Button>
            <Button
              variant="primary"
              onClick={save}
              busy={saving}
              disabled={!plan || compiledPrompt !== prompt.trim() || compiling}
              className="text-[12px]"
            >
              {existing ? "Save" : "Create brief"}
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}

function PlanEditor({ plan, onChange }: { plan: HuntPlan; onChange: (p: HuntPlan) => void }) {
  return (
    <div className="space-y-2 text-[12px]">
      <PlanRow label="Searches">
        <TokenList tokens={plan.queries} onChange={(queries) => onChange({ ...plan, queries })} mono />
      </PlanRow>
      <PlanRow label="Must contain">
        <TokenList tokens={plan.include} onChange={(include) => onChange({ ...plan, include })} tone="ok" />
      </PlanRow>
      <PlanRow label="Must NOT contain">
        <TokenList tokens={plan.exclude} onChange={(exclude) => onChange({ ...plan, exclude })} tone="bad" />
      </PlanRow>
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5 pt-0.5">
        <span className="text-[11px] text-dim">
          Resolutions:{" "}
          {["2160p", "1080p", "720p"].map((r) => (
            <Chip
              key={r}
              active={plan.resolutions.includes(r)}
              className="ml-1"
              onClick={() =>
                onChange({
                  ...plan,
                  resolutions: plan.resolutions.includes(r)
                    ? plan.resolutions.filter((x) => x !== r)
                    : [...plan.resolutions, r],
                })
              }
            >
              {r}
            </Chip>
          ))}
        </span>
        <label className="flex items-center gap-1 text-[11px] text-dim">
          Max size
          <input
            value={plan.maxSizeGb || ""}
            onChange={(e) => onChange({ ...plan, maxSizeGb: Number(e.target.value) || 0 })}
            placeholder="∞"
            className="w-12 rounded-md border border-line2 bg-bg1 px-1.5 py-0.5 text-center font-mono text-[11px] text-ink outline-none focus:border-accent/50"
          />
          GB
        </label>
        <label className="flex items-center gap-1 text-[11px] text-dim">
          Min seeders
          <input
            value={plan.minSeeders}
            onChange={(e) =>
              // the backend field is an integer; "2.5" would fail deserialization with a raw serde message
              onChange({ ...plan, minSeeders: Math.max(0, Math.floor(Number(e.target.value) || 0)) })
            }
            className="w-10 rounded-md border border-line2 bg-bg1 px-1.5 py-0.5 text-center font-mono text-[11px] text-ink outline-none focus:border-accent/50"
          />
        </label>
      </div>
      {plan.notes && <div className="pt-0.5 text-[11px] italic text-faint">"{plan.notes}"</div>}
    </div>
  );
}

function PlanRow({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-start gap-2">
      <span className="w-[92px] shrink-0 pt-0.5 text-[11px] text-dim">{label}</span>
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  );
}

function TokenList({
  tokens,
  onChange,
  tone,
  mono,
}: {
  tokens: string[];
  onChange: (t: string[]) => void;
  tone?: "ok" | "bad";
  mono?: boolean;
}) {
  const [draft, setDraft] = useState("");
  return (
    <div className="flex flex-wrap items-center gap-1">
      {tokens.map((t, i) => (
        <span
          key={`${t}-${i}`}
          className={cx(
            "group inline-flex items-center gap-1 rounded-md border px-1.5 py-px text-[10.5px]",
            mono && "font-mono",
            tone === "ok" && "border-ok/30 bg-ok/8 text-ok",
            tone === "bad" && "border-bad/30 bg-bad/8 text-bad",
            !tone && "border-line2 bg-bg1 text-dim",
          )}
        >
          {t}
          <button
            type="button"
            onClick={() => onChange(tokens.filter((_, j) => j !== i))}
            className="cursor-pointer opacity-40 hover:opacity-100"
          >
            <X size={9} />
          </button>
        </span>
      ))}
      <input
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if ((e.key === "Enter" || e.key === "Return") && draft.trim()) {
            onChange([...tokens, draft.trim()]);
            setDraft("");
            e.preventDefault();
          }
        }}
        placeholder="+ add"
        className="w-16 rounded-md border border-transparent bg-transparent px-1.5 py-px text-[10.5px] text-ink outline-none placeholder:text-faint focus:border-line2 focus:bg-bg1"
      />
    </div>
  );
}
