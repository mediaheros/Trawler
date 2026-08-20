import { useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowUp,
  Check,
  ChevronDown,
  ClipboardList,
  Eraser,
  Sparkles,
  Wrench,
  X,
} from "lucide-react";
import { type ProposalRow } from "../lib/api";
import { fmtBytes } from "../lib/format";
import { useStore, type LiveStep } from "../store";
import { Badge, Button, cx } from "../components/ui";
import { Markdown } from "../components/Markdown";
import BriefsDrawer from "../components/BriefsDrawer";

const SUGGESTIONS = [
  "Find Dune Part Two in the best quality under 10 GB",
  "Set up a brief: UFC numbered events, main card, 1080p, under 6 GB",
  "Why are my downloads stuck?",
  "What did you grab this week?",
];

export default function AgentView() {
  const {
    chat, chatLoaded, loadChat, agentBusy, liveSteps, agentThinking,
    sendToAgent, clearChat, initAgentEvents,
    proposals, loadProposals, resolveProposal, loadBriefs,
    config,
  } = useStore();
  const [input, setInput] = useState("");
  const [briefsOpen, setBriefsOpen] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    void loadChat();
    void loadProposals();
    void loadBriefs();
    initAgentEvents();
    inputRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // pin to bottom as content streams in
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [chat, liveSteps, agentThinking]);

  const send = () => {
    const text = input.trim();
    if (!text || agentBusy) return;
    setInput("");
    void sendToAgent(text);
  };

  const empty = chatLoaded && chat.length === 0;

  return (
    <div className="flex h-full">
      <div className="flex min-w-0 flex-1 flex-col">
        {/* header */}
        <div className="flex items-center justify-between px-6 pt-5 pb-4">
          <div className="flex items-baseline gap-2.5">
            <h1 className="text-[16px] font-semibold tracking-tight">Agent</h1>
            <span className="font-mono text-[10.5px] text-faint">{config?.agentModel ?? ""}</span>
          </div>
          <div className="flex items-center gap-1.5">
            {chat.length > 0 && (
              <Button variant="ghost" onClick={() => void clearChat()} className="px-2 py-1.5" title="Clear conversation">
                <Eraser size={14} />
              </Button>
            )}
            <Button
              variant={briefsOpen ? "default" : "ghost"}
              onClick={() => setBriefsOpen(!briefsOpen)}
              className="relative px-2.5 py-1.5"
              title="Standing briefs"
            >
              <ClipboardList size={14} />
              <span className="text-[12px]">Briefs</span>
              {proposals.length > 0 && (
                <span className="absolute -right-1 -top-1 flex size-4 items-center justify-center rounded-full bg-warn text-[9.5px] font-bold text-bg0">
                  {proposals.length}
                </span>
              )}
            </Button>
          </div>
        </div>

        {/* transcript */}
        <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto px-6">
          <div className={cx("mx-auto flex max-w-[720px] flex-col pb-6", empty && !agentBusy && "min-h-full justify-center")}>
            {empty && !agentBusy ? (
              <EmptyState onPick={(s) => { setInput(s); inputRef.current?.focus(); }} />
            ) : (
              <>
                {proposals.length > 0 && (
                  <div className="msg-in mb-4 mt-2 space-y-2">
                    {proposals.map((p) => (
                      <ProposalCard key={p.id} p={p} onResolve={resolveProposal} />
                    ))}
                  </div>
                )}
                {chat.map((m) =>
                  m.role === "user" ? (
                    <div key={m.id} className="msg-in mb-4 mt-2 flex justify-end">
                      <div className="max-w-[85%] select-text rounded-2xl rounded-br-md border border-accent/20 bg-accent-soft px-4 py-2.5 text-[13px] leading-relaxed">
                        {m.content}
                      </div>
                    </div>
                  ) : m.role === "assistant" ? (
                    <div key={m.id} className="msg-in mb-5 flex gap-3">
                      <AgentAvatar />
                      <div className="min-w-0 flex-1 select-text pt-1 text-[13px] text-ink/95">
                        <Markdown text={m.content ?? ""} />
                      </div>
                    </div>
                  ) : (
                    <StoredStep key={m.id} summary={m.content ?? ""} payload={m.toolPayload} />
                  ),
                )}
                {/* live run */}
                {(liveSteps.length > 0 || agentThinking) && agentBusy && (
                  <div className="mb-5 flex gap-3">
                    <AgentAvatar pulse />
                    <div className="min-w-0 flex-1 space-y-1.5 pt-1">
                      {liveSteps.map((s) => (
                        <LiveStepCard key={s.key} s={s} />
                      ))}
                      {agentThinking && <TypingDots />}
                    </div>
                  </div>
                )}
              </>
            )}
          </div>
        </div>

        {/* composer */}
        <div className="px-6 pb-5">
          <div className="mx-auto max-w-[720px]">
            <div
              className={cx(
                "flex items-end gap-2 rounded-2xl border bg-bg2 p-2 pl-4 transition-all duration-200",
                agentBusy
                  ? "border-line"
                  : "border-line2 focus-within:border-accent/45 focus-within:shadow-[0_0_28px_-10px_var(--color-accent)]",
              )}
            >
              <textarea
                ref={inputRef}
                value={input}
                onChange={(e) => {
                  setInput(e.target.value);
                  e.target.style.height = "auto";
                  e.target.style.height = `${Math.min(140, e.target.scrollHeight)}px`;
                }}
                onKeyDown={(e) => {
                  if ((e.key === "Enter" || e.key === "Return") && !e.shiftKey) {
                    e.preventDefault();
                    send();
                  }
                }}
                rows={1}
                placeholder={agentBusy ? "The agent is working…" : "Ask for anything — or describe a standing brief…"}
                spellCheck={false}
                className="max-h-[140px] min-h-[24px] flex-1 resize-none bg-transparent py-1.5 text-[13.5px] leading-relaxed text-ink outline-none placeholder:text-faint"
              />
              <button
                type="button"
                onClick={send}
                disabled={!input.trim() || agentBusy}
                className={cx(
                  "flex size-9 shrink-0 cursor-pointer items-center justify-center rounded-xl transition-all duration-150",
                  input.trim() && !agentBusy
                    ? "bg-accent text-bg0 shadow-[0_0_18px_-6px_var(--color-accent)] hover:brightness-110"
                    : "bg-bg3 text-faint",
                )}
              >
                <ArrowUp size={16} strokeWidth={2.4} />
              </button>
            </div>
            <div className="mt-1.5 text-center text-[10.5px] text-faint">
              Grabs stay inside your size caps · new briefs ask before grabbing anything
            </div>
          </div>
        </div>
      </div>

      {briefsOpen && <BriefsDrawer onClose={() => setBriefsOpen(false)} />}
    </div>
  );
}

function AgentAvatar({ pulse }: { pulse?: boolean }) {
  return (
    <div
      className={cx(
        "mt-0.5 flex size-7 shrink-0 items-center justify-center rounded-lg bg-gradient-to-br from-accent/25 to-accent2/15",
        pulse && "glow-pulse",
      )}
    >
      <Sparkles size={13} className="text-accent" />
    </div>
  );
}

function TypingDots() {
  return (
    <div className="flex items-center gap-1 py-2">
      {[0, 1, 2].map((i) => (
        <span
          key={i}
          className="typing-dot size-[6px] rounded-full bg-accent/70"
          style={{ animationDelay: `${i * 0.15}s` }}
        />
      ))}
    </div>
  );
}

function LiveStepCard({ s }: { s: LiveStep }) {
  const [open, setOpen] = useState(false);
  const canExpand = s.detail !== undefined;
  return (
    <div className="step-in">
      <button
        type="button"
        onClick={() => canExpand && setOpen(!open)}
        className={cx(
          "flex w-full items-center gap-2 rounded-lg border border-line/70 bg-bg1/80 px-2.5 py-1.5 text-left",
          canExpand && "cursor-pointer hover:border-line2",
        )}
      >
        {s.running ? (
          <span className="glow-pulse text-accent"><Wrench size={12} /></span>
        ) : s.summary.startsWith("⚠") ? (
          <X size={12} className="shrink-0 text-warn" />
        ) : (
          <Check size={12} className="shrink-0 text-ok" />
        )}
        <span className={cx("min-w-0 flex-1 truncate text-[11.5px]", s.running ? "text-dim" : "text-faint")}>
          {s.summary}
        </span>
        {canExpand && (
          <ChevronDown size={11} className={cx("shrink-0 text-faint transition-transform", open && "rotate-180")} />
        )}
      </button>
      {open && (
        <pre className="mt-1 max-h-[180px] overflow-auto rounded-lg border border-line/60 bg-bg0/80 p-2.5 font-mono text-[10.5px] leading-relaxed text-faint">
          {JSON.stringify(s.detail, null, 1)}
        </pre>
      )}
    </div>
  );
}

/** Persisted tool steps from history reload — rendered compact. */
function StoredStep({ summary, payload }: { summary: string; payload: string | null }) {
  const [open, setOpen] = useState(false);
  const detail = useMemo(() => {
    if (!payload) return undefined;
    try {
      return JSON.parse(payload);
    } catch {
      return undefined;
    }
  }, [payload]);
  return (
    <div className="mb-1.5 ml-10">
      <button
        type="button"
        onClick={() => detail !== undefined && setOpen(!open)}
        className={cx(
          "flex items-center gap-1.5 rounded-md px-1.5 py-0.5 text-[11px] text-faint",
          detail !== undefined && "cursor-pointer hover:bg-bg2 hover:text-dim",
        )}
      >
        <Wrench size={10} />
        <span className="truncate">{summary}</span>
      </button>
      {open && (
        <pre className="mt-1 max-h-[160px] overflow-auto rounded-lg border border-line/60 bg-bg0/80 p-2 font-mono text-[10.5px] text-faint">
          {JSON.stringify(detail, null, 1)}
        </pre>
      )}
    </div>
  );
}

export function ProposalCard({
  p,
  onResolve,
}: {
  p: ProposalRow;
  onResolve: (id: number, approve: boolean) => void;
}) {
  const r = p.result;
  const [resolving, setResolving] = useState<"grab" | "skip" | null>(null);
  const resolve = (approve: boolean) => {
    setResolving(approve ? "grab" : "skip");
    onResolve(p.id, approve);
  };
  return (
    <div className="overflow-hidden rounded-(--radius-card) border border-warn/25 bg-bg1">
      <div className="flex items-center gap-2 border-b border-line/60 bg-warn/[0.06] px-3 py-1.5">
        <span className="size-[6px] rounded-full bg-warn" />
        <span className="text-[11px] font-semibold text-warn/90">
          Proposal{p.briefName ? ` · ${p.briefName}` : ""}
        </span>
        <span className="ml-auto text-[10.5px] text-faint">{timeAgo(p.lastSeen)}</span>
      </div>
      <div className="px-3 py-2.5">
        <div className="truncate font-mono text-[12px] text-ink" title={r.title}>{r.title}</div>
        <div className="mt-1.5 flex flex-wrap items-center gap-1">
          {r.resolution && <Badge tone="resolution">{r.resolution}</Badge>}
          {r.source && <Badge>{r.source}</Badge>}
          {r.codec && <Badge>{r.codec}</Badge>}
          <span className="ml-1 font-mono text-[11px] text-dim">{fmtBytes(r.size)}</span>
          <span className="font-mono text-[11px] text-dim">· {r.seeders ?? "?"} seeders</span>
          {r.indexer && <span className="text-[11px] text-faint">· {r.indexer}</span>}
        </div>
        {p.reason && (
          <div className="mt-1.5 text-[11.5px] italic text-faint">"{p.reason}"</div>
        )}
        <div className="mt-2.5 flex gap-1.5">
          <Button
            variant="primary"
            onClick={() => resolve(true)}
            busy={resolving === "grab"}
            disabled={resolving !== null}
            className="px-3 py-1 text-[11.5px]"
          >
            <Check size={12} /> Grab it
          </Button>
          <Button
            variant="ghost"
            onClick={() => resolve(false)}
            busy={resolving === "skip"}
            disabled={resolving !== null}
            className="px-2.5 py-1 text-[11.5px]"
          >
            Skip
          </Button>
        </div>
      </div>
    </div>
  );
}

function EmptyState({ onPick }: { onPick: (s: string) => void }) {
  return (
    <div className="flex flex-col items-center pb-16 text-center">
      <div className="flex size-14 items-center justify-center rounded-2xl bg-gradient-to-br from-accent/25 to-accent2/15 shadow-[0_0_40px_-8px_var(--color-accent)]">
        <Sparkles size={24} className="text-accent" />
      </div>
      <div className="sheen mt-5 bg-gradient-to-r from-ink via-accent to-ink bg-clip-text text-[18px] font-semibold tracking-tight text-transparent">
        What are we hunting?
      </div>
      <p className="mt-2 max-w-[400px] text-[12.5px] leading-relaxed text-faint">
        Ask for a one-off find, or describe something to watch for on a schedule —
        the things episode tracking can't express.
      </p>
      <div className="mt-6 flex max-w-[520px] flex-wrap justify-center gap-2">
        {SUGGESTIONS.map((s) => (
          <button
            key={s}
            type="button"
            onClick={() => onPick(s)}
            className="cursor-pointer rounded-xl border border-line2 bg-bg2 px-3 py-1.5 text-[12px] text-dim transition-all hover:border-accent/40 hover:text-ink"
          >
            {s}
          </button>
        ))}
      </div>
    </div>
  );
}

function timeAgo(ts: number): string {
  const s = Date.now() / 1000 - ts;
  if (s < 90) return "just now";
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}
