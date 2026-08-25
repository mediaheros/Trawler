/** Platform detection for UI affordances (shortcut glyphs, paths, copy). */
export type DesktopPlatform = "macos" | "windows" | "linux";

export function detectDesktopPlatform(
  platform = typeof navigator === "undefined" ? "" : navigator.platform,
  userAgent = typeof navigator === "undefined" ? "" : navigator.userAgent,
): DesktopPlatform {
  if (platform.startsWith("Mac") || userAgent.includes("Macintosh")) return "macos";
  if (platform.startsWith("Win") || userAgent.includes("Windows")) return "windows";
  return "linux";
}

export const desktopPlatform = detectDesktopPlatform();
export const isMac = desktopPlatform === "macos";
export const isWindows = desktopPlatform === "windows";
export const isLinux = desktopPlatform === "linux";

/** The primary modifier key's display glyph. */
export const modKey = isMac ? "\u2318" : "Ctrl";

/** Is the primary modifier held? (Cmd on mac, Ctrl elsewhere) */
export function modHeld(e: { ctrlKey: boolean; metaKey: boolean }): boolean {
  return isMac ? e.metaKey : e.ctrlKey;
}
