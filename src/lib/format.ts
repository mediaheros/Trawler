export function fmtBytes(n: number): string {
  if (!n || n <= 0) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`;
}

export function fmtSpeed(n: number): string {
  if (!n || n <= 0) return "0 B/s";
  return `${fmtBytes(n)}/s`;
}

export function fmtAge(iso: string | null, ageDays: number): string {
  let days = ageDays;
  if (iso) {
    const d = (Date.now() - new Date(iso).getTime()) / 8.64e7;
    if (Number.isFinite(d)) days = d;
  }
  if (days < 0) days = 0;
  if (days < 1) {
    const h = Math.round(days * 24);
    return h <= 1 ? "1h" : `${h}h`;
  }
  if (days < 30) return `${Math.round(days)}d`;
  if (days < 365) return `${Math.round(days / 30.44)}mo`;
  return `${(days / 365.25).toFixed(days < 730 ? 1 : 0)}y`;
}

export function fmtEta(sec: number): string {
  if (sec < 0 || sec >= 8640000) return "∞";
  if (sec < 60) return `${sec}s`;
  if (sec < 3600) return `${Math.round(sec / 60)}m`;
  if (sec < 86400) return `${Math.floor(sec / 3600)}h ${Math.round((sec % 3600) / 60)}m`;
  return `${Math.floor(sec / 86400)}d ${Math.round((sec % 86400) / 3600)}h`;
}

export function seederHealth(seeders: number | null): "good" | "ok" | "low" | "dead" {
  const s = seeders ?? 0;
  if (s >= 50) return "good";
  if (s >= 10) return "ok";
  if (s >= 1) return "low";
  return "dead";
}

export const qbitStateLabel: Record<string, string> = {
  downloading: "Downloading",
  stalledDL: "Stalled",
  metaDL: "Fetching metadata",
  forcedDL: "Downloading",
  allocating: "Allocating",
  uploading: "Seeding",
  stalledUP: "Seeded",
  forcedUP: "Seeding",
  pausedDL: "Paused",
  stoppedDL: "Stopped",
  pausedUP: "Done",
  stoppedUP: "Done",
  queuedDL: "Queued",
  queuedUP: "Queued",
  checkingDL: "Checking",
  checkingUP: "Checking",
  checkingResumeData: "Checking",
  moving: "Moving",
  error: "Error",
  missingFiles: "Missing files",
  unknown: "Unknown",
};

export function stateKind(state: string): "active" | "done" | "paused" | "error" {
  if (state === "error" || state === "missingFiles") return "error";
  if (state.startsWith("paused") || state.startsWith("stopped")) {
    return state.endsWith("UP") ? "done" : "paused";
  }
  if (state === "uploading" || state === "stalledUP" || state === "forcedUP") return "done";
  return "active";
}

import type { QualityProfile } from "./api";

/** Profile equality that ignores resolution ordering. */
export function sameProfile(a: QualityProfile, b: QualityProfile): boolean {
  return (
    a.codec === b.codec &&
    a.maxSizeGb === b.maxSizeGb &&
    a.allowSeasonPacks === b.allowSeasonPacks &&
    a.resolutions.length === b.resolutions.length &&
    [...a.resolutions].sort().join(",") === [...b.resolutions].sort().join(",")
  );
}
