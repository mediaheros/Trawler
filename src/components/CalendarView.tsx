import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { CalendarArrowDown, ChevronLeft, ChevronRight } from "lucide-react";
import { api, type CalendarItem } from "../lib/api";
import { useStore } from "../store";
import { Button, cx } from "./ui";

const STATE_DOT: Record<string, string> = {
  downloaded: "bg-ok",
  grabbed: "bg-accent",
  wanted: "bg-warn",
  upcoming: "bg-bg4",
  ignored: "bg-bg4",
};

function monthLabel(d: Date): string {
  return d.toLocaleDateString(undefined, { month: "long", year: "numeric" });
}

function isoDate(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

export default function CalendarView() {
  const toast = useStore((s) => s.toast);
  const [anchor, setAnchor] = useState(() => {
    const d = new Date();
    return new Date(d.getFullYear(), d.getMonth(), 1);
  });
  const [items, setItems] = useState<CalendarItem[]>([]);
  const [exporting, setExporting] = useState(false);
  const reqSeq = useRef(0);

  // grid: start on the Monday on/before the 1st, six weeks
  const gridStart = useMemo(() => {
    const d = new Date(anchor);
    const dow = (d.getDay() + 6) % 7; // Mon=0
    d.setDate(d.getDate() - dow);
    return d;
  }, [anchor]);

  const days = useMemo(
    () =>
      Array.from({ length: 42 }, (_, i) => {
        const d = new Date(gridStart);
        d.setDate(d.getDate() + i);
        return d;
      }),
    [gridStart],
  );

  const load = useCallback(async () => {
    // pad a day each side: UTC airstamps can fall on a neighboring local day
    const start = new Date(gridStart);
    start.setDate(start.getDate() - 1);
    const end = new Date(gridStart);
    end.setDate(end.getDate() + 43);
    const seq = ++reqSeq.current;
    try {
      const data = await api.calendarRange(isoDate(start), isoDate(end));
      if (seq === reqSeq.current) setItems(data);
    } catch (e) {
      if (seq === reqSeq.current) toast(String(e), "bad");
    }
  }, [gridStart, toast]);

  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const poll = async () => {
      await load();
      if (active) timer = setTimeout(poll, 30_000);
    };
    void poll();
    return () => {
      active = false;
      ++reqSeq.current;
      if (timer !== null) clearTimeout(timer);
    };
  }, [load]);

  const byDay = useMemo(() => {
    const map = new Map<string, CalendarItem[]>();
    for (const it of items) {
      // backend computes the LOCAL day; never bucket by the UTC string
      const key = it.localDate;
      if (!map.has(key)) map.set(key, []);
      map.get(key)!.push(it);
    }
    return map;
  }, [items]);

  const exportIcs = async () => {
    setExporting(true);
    try {
      const path = await api.exportIcal();
      toast(`Saved the next 90 days of episodes to ${path}`, "ok");
    } catch (e) {
      toast(String(e), "bad");
    } finally {
      setExporting(false);
    }
  };

  const todayKey = isoDate(new Date());
  const shift = (months: number) =>
    setAnchor((a) => new Date(a.getFullYear(), a.getMonth() + months, 1));

  return (
    <div className="flex h-full flex-col px-6 pb-6">
      {/* controls */}
      <div className="mb-3 flex items-center gap-2">
        <span className="text-[14px] font-semibold tracking-tight">{monthLabel(anchor)}</span>
        <div className="ml-2 flex items-center gap-0.5">
          <button type="button" onClick={() => shift(-1)} className="cursor-pointer rounded-md p-1 text-faint hover:bg-bg3 hover:text-ink">
            <ChevronLeft size={15} />
          </button>
          <button
            type="button"
            onClick={() => setAnchor(new Date(new Date().getFullYear(), new Date().getMonth(), 1))}
            className="cursor-pointer rounded-md px-2 py-0.5 text-[11.5px] text-dim hover:bg-bg3 hover:text-ink"
          >
            Today
          </button>
          <button type="button" onClick={() => shift(1)} className="cursor-pointer rounded-md p-1 text-faint hover:bg-bg3 hover:text-ink">
            <ChevronRight size={15} />
          </button>
        </div>
        <div className="ml-auto flex items-center gap-3">
          <span className="hidden items-center gap-3 text-[10.5px] text-faint md:flex">
            <span className="flex items-center gap-1"><i className="size-[6px] rounded-full bg-ok" /> downloaded</span>
            <span className="flex items-center gap-1"><i className="size-[6px] rounded-full bg-accent" /> grabbing</span>
            <span className="flex items-center gap-1"><i className="size-[6px] rounded-full bg-warn" /> wanted</span>
            <span className="flex items-center gap-1"><i className="size-[6px] rounded-full bg-bg4" /> upcoming</span>
          </span>
          <Button onClick={exportIcs} busy={exporting} className="px-2.5 py-1 text-[11.5px]" title="Save the next 90 days as an .ics for any calendar app">
            <CalendarArrowDown size={12} /> Export next 90 days
          </Button>
        </div>
      </div>

      {/* weekday header */}
      <div className="grid grid-cols-7 gap-px pb-1">
        {["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].map((d) => (
          <div key={d} className="px-2 text-[10.5px] font-medium uppercase tracking-wide text-faint">
            {d}
          </div>
        ))}
      </div>

      {/* grid */}
      <div className="grid min-h-0 flex-1 grid-cols-7 grid-rows-6 gap-px overflow-hidden rounded-(--radius-card) border border-line bg-line/60">
        {days.map((d) => {
          const key = isoDate(d);
          const inMonth = d.getMonth() === anchor.getMonth();
          const isToday = key === todayKey;
          const dayItems = byDay.get(key) ?? [];
          const shown = dayItems.slice(0, 3);
          return (
            <div
              key={key}
              className={cx(
                "flex min-h-0 flex-col gap-0.5 overflow-hidden bg-bg1 p-1.5",
                !inMonth && "bg-bg0/80 opacity-55",
              )}
            >
              <span
                className={cx(
                  "mb-0.5 flex size-5 items-center justify-center rounded-full text-[10.5px]",
                  isToday ? "bg-accent font-bold text-bg0" : "text-faint",
                )}
              >
                {d.getDate()}
              </span>
              {shown.map((it, i) => (
                <div
                  key={`${it.showId}-${it.season}-${it.number}-${i}`}
                  title={`${it.showName} S${String(it.season).padStart(2, "0")}E${String(it.number).padStart(2, "0")}${it.title ? ` · ${it.title}` : ""} — ${it.state}`}
                  className="flex min-w-0 items-center gap-1 rounded-[5px] bg-bg2 px-1 py-px"
                >
                  <span className={cx("size-[5px] shrink-0 rounded-full", STATE_DOT[it.state] ?? "bg-bg4")} />
                  <span className="truncate font-mono text-[9.5px] text-dim">
                    {it.showName}
                  </span>
                  <span className="ml-auto shrink-0 font-mono text-[9px] text-faint">
                    {it.season}×{String(it.number).padStart(2, "0")}
                  </span>
                </div>
              ))}
              {dayItems.length > 3 && (
                <span className="px-1 text-[9px] text-faint">+{dayItems.length - 3} more</span>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
