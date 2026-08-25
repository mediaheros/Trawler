import type { ReactNode } from "react";
import { useEffect, useRef, useState } from "react";
import { Loader2, X } from "lucide-react";
import { seederHealth } from "../lib/format";
import { useStore } from "../store";

export function cx(...parts: Array<string | false | null | undefined>) {
  return parts.filter(Boolean).join(" ");
}

// ---------- status pill (one look for TVmaze status, everywhere) ----------

const STATUS_STYLE: Record<string, string> = {
  Running: "bg-accent/15 text-accent",
  Ended: "bg-bg3 text-faint",
  "To Be Determined": "bg-warn/12 text-warn",
  "In Development": "bg-accent2/12 text-accent2",
};

export function statusLabel(s: string): string {
  if (s === "Running") return "Airing";
  if (s === "To Be Determined") return "TBD";
  if (s === "In Development") return "Upcoming";
  return s;
}

export function StatusPill({ status, className }: { status: string; className?: string }) {
  return (
    <span
      className={cx(
        "rounded-md px-1.5 py-px text-[10.5px] font-medium",
        STATUS_STYLE[status] ?? "bg-bg3 text-dim",
        className,
      )}
    >
      {statusLabel(status)}
    </span>
  );
}

// ---------- toggleable chip (quality filters, plan resolutions) ----------

export function Chip({
  active,
  onClick,
  children,
  className,
}: {
  active: boolean;
  onClick: () => void;
  children: ReactNode;
  className?: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cx(
        "cursor-pointer rounded-md border px-2 py-1 font-mono text-[10.5px] leading-none transition-colors active:scale-[0.97]",
        active
          ? "border-accent/50 bg-accent-soft text-accent"
          : "border-line bg-bg1 text-faint hover:text-dim",
        className,
      )}
    >
      {children}
    </button>
  );
}

// ---------- canonical icon action button ----------

export function IconBtn({
  title,
  onClick,
  danger,
  reveal,
  children,
  className,
  disabled = false,
}: {
  title: string;
  onClick: () => void;
  danger?: boolean;
  /** hover-revealed inside a `group` parent; stays reachable by keyboard */
  reveal?: boolean;
  children: ReactNode;
  className?: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      title={title}
      onClick={onClick}
      disabled={disabled}
      className={cx(
        "cursor-pointer rounded-md p-1.5 text-faint transition-colors hover:bg-bg3 disabled:pointer-events-none disabled:opacity-40",
        danger ? "hover:text-bad" : "hover:text-ink",
        reveal && "opacity-0 focus-visible:opacity-100 group-hover:opacity-100 group-focus-within:opacity-100",
        className,
      )}
    >
      {children}
    </button>
  );
}

export function CloseBtn({
  onClick,
  className,
  disabled = false,
}: {
  onClick: () => void;
  className?: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-label="Close"
      className={cx(
        "cursor-pointer rounded-lg p-1.5 text-faint transition-colors hover:bg-bg3 hover:text-dim disabled:pointer-events-none disabled:opacity-40",
        className,
      )}
    >
      <X size={15} />
    </button>
  );
}

// ---------- badges ----------

const badgeTones: Record<string, string> = {
  resolution: "text-accent bg-accent-soft",
  hdr: "text-warn bg-warn/10",
  kind: "text-accent2 bg-accent2/10",
  proper: "text-ok bg-ok/10",
  plain: "text-dim bg-bg3",
};

export function Badge({
  children,
  tone = "plain",
  mono = true,
}: {
  children: ReactNode;
  tone?: keyof typeof badgeTones;
  mono?: boolean;
}) {
  return (
    <span
      className={cx(
        "inline-flex items-center rounded-[5px] px-1.5 py-[1px] text-[10.5px] font-medium leading-[1.5] whitespace-nowrap",
        mono && "font-mono",
        badgeTones[tone],
      )}
    >
      {children}
    </span>
  );
}

// ---------- seeder health ----------

const healthColor: Record<string, string> = {
  good: "bg-ok",
  ok: "bg-accent",
  low: "bg-warn",
  dead: "bg-bad",
};

export function SeederDot({ seeders }: { seeders: number | null }) {
  const h = seederHealth(seeders);
  return (
    <span className="relative inline-flex size-[7px]">
      <span className={cx("absolute inline-flex size-full rounded-full", healthColor[h])} />
      {h === "good" && (
        <span className={cx("absolute inline-flex size-full rounded-full opacity-40 blur-[3px]", healthColor[h])} />
      )}
    </span>
  );
}

// ---------- buttons ----------

export function Button({
  children,
  onClick,
  variant = "default",
  disabled,
  busy,
  className,
  title,
}: {
  children: ReactNode;
  onClick?: () => void;
  variant?: "default" | "primary" | "ghost" | "danger";
  disabled?: boolean;
  busy?: boolean;
  className?: string;
  title?: string;
}) {
  const styles: Record<string, string> = {
    default:
      "bg-bg3 hover:bg-bg4 text-ink border border-line2",
    primary:
      "bg-accent text-bg0 hover:brightness-110 font-semibold shadow-[0_0_16px_-4px_var(--color-accent)]",
    ghost: "text-dim hover:text-ink hover:bg-bg3",
    danger: "bg-bad/10 text-bad hover:bg-bad/20 border border-bad/20",
  };
  return (
    <button
      type="button"
      title={title}
      onClick={onClick}
      disabled={disabled || busy}
      className={cx(
        "inline-flex items-center justify-center gap-1.5 whitespace-nowrap rounded-(--radius-btn) px-3 py-1.5 text-[12.5px] transition-all duration-150",
        "disabled:opacity-45 disabled:pointer-events-none cursor-pointer select-none active:scale-[0.97]",
        styles[variant],
        className,
      )}
    >
      {busy && <Loader2 size={13} className="spin" />}
      {children}
    </button>
  );
}

// ---------- segmented control ----------

export function Segmented<T extends string>({
  value,
  onChange,
  options,
}: {
  value: T;
  onChange: (v: T) => void;
  options: Array<{ value: T; label: string }>;
}) {
  return (
    <div className="inline-flex items-center gap-0.5 rounded-[9px] bg-bg2 border border-line p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          onClick={() => onChange(o.value)}
          className={cx(
            "rounded-[7px] px-2.5 py-1 text-[12px] font-medium transition-colors duration-150 cursor-pointer",
            value === o.value
              ? "bg-bg4 text-ink shadow-[inset_0_0_0_1px_var(--color-line2)]"
              : "text-faint hover:text-dim",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

// ---------- form field ----------

export function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <label className="block">
      <div className="mb-1 text-[11.5px] font-medium text-dim">{label}</div>
      {children}
      {hint && <div className="mt-1 text-[11px] text-faint">{hint}</div>}
    </label>
  );
}

export function NumInput({
  value,
  onCommit,
  min,
  max,
  integer,
}: {
  value: number;
  onCommit: (n: number) => void;
  min?: number;
  max?: number;
  integer?: boolean;
}) {
  const [text, setText] = useState(String(value));
  const focused = useRef(false);
  useEffect(() => {
    if (!focused.current) setText(String(value));
  }, [value]);
  const commit = () => {
    let n = Number(text);
    if (!Number.isFinite(n)) n = value;
    if (integer) n = Math.trunc(n);
    if (min !== undefined) n = Math.max(min, n);
    if (max !== undefined) n = Math.min(max, n);
    onCommit(n);
    setText(String(n));
  };
  return (
    <input
      value={text}
      onChange={(e) => setText(e.target.value)}
      onFocus={() => {
        focused.current = true;
      }}
      onBlur={() => {
        focused.current = false;
        commit();
      }}
      onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
      inputMode="decimal"
      className={cx(
        "w-full rounded-(--radius-btn) bg-bg1 border border-line2 px-2.5 py-1.5 text-[12px] text-ink font-mono",
        "placeholder:text-faint focus:border-accent/50 transition-colors",
      )}
      spellCheck={false}
      autoComplete="off"
    />
  );
}

export function TextInput({
  value,
  onChange,
  placeholder,
  type = "text",
  mono,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  type?: string;
  mono?: boolean;
}) {
  return (
    <input
      type={type}
      value={value}
      placeholder={placeholder}
      onChange={(e) => onChange(e.target.value)}
      className={cx(
        "w-full rounded-(--radius-btn) bg-bg1 border border-line2 px-2.5 py-1.5 text-[12.5px] text-ink",
        "placeholder:text-faint focus:border-accent/50 transition-colors",
        mono && "font-mono text-[12px]",
      )}
      spellCheck={false}
      autoComplete="off"
    />
  );
}

// ---------- toasts ----------

export function ToastStack() {
  const toasts = useStore((s) => s.toasts);
  const dismiss = useStore((s) => s.dismissToast);
  // the update card sits at the same corner — stack toasts above it instead
  // of covering its buttons. Mirror the banner's own visibility rule so a
  // dismissed card doesn't leave toasts floating over empty space.
  const updatePending = useStore(
    (s) => s.pendingUpdate !== null && s.updateDismissedVersion !== s.pendingUpdate.version,
  );
  return (
    <div
      className={cx(
        "pointer-events-none fixed right-4 z-50 flex w-[340px] flex-col gap-2 transition-[bottom] duration-300",
        updatePending ? "bottom-[10rem]" : "bottom-4",
      )}
    >
      {toasts.map((t) => (
        <button
          type="button"
          key={t.id}
          onClick={() => dismiss(t.id)}
          className={cx(
            "toast-in pointer-events-auto cursor-pointer rounded-(--radius-card) border px-3.5 py-2.5 text-left text-[12.5px] leading-snug backdrop-blur-md transition-all hover:border-line2 hover:brightness-110",
            t.tone === "ok" && "border-ok/25 bg-bg2/95 text-ink",
            t.tone === "bad" && "border-bad/30 bg-bg2/95 text-ink",
            t.tone === "info" && "border-line2 bg-bg2/95 text-ink",
          )}
        >
          <span
            className={cx(
              "mr-2 inline-block size-[7px] rounded-full align-middle",
              t.tone === "ok" && "bg-ok",
              t.tone === "bad" && "bg-bad",
              t.tone === "info" && "bg-accent",
            )}
          />
          {t.text}
        </button>
      ))}
    </div>
  );
}

// ---------- empty / message states ----------

export function CenterMessage({
  icon,
  title,
  body,
}: {
  icon?: ReactNode;
  title: string;
  body?: ReactNode;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 py-16 text-center">
      {icon && <div className="mb-1 text-faint">{icon}</div>}
      <div className="text-[14px] font-semibold text-dim">{title}</div>
      {body && <div className="max-w-[420px] text-[12.5px] leading-relaxed text-faint">{body}</div>}
    </div>
  );
}
