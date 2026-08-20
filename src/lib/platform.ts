/** Platform detection for UI affordances (shortcut glyphs, install copy). */
export const isMac =
  typeof navigator !== "undefined" &&
  (navigator.platform.startsWith("Mac") || navigator.userAgent.includes("Macintosh"));

/** The primary modifier key's display glyph. */
export const modKey = isMac ? "\u2318" : "Ctrl";

/** Is the primary modifier held? (Cmd on mac, Ctrl elsewhere) */
export function modHeld(e: { ctrlKey: boolean; metaKey: boolean }): boolean {
  return isMac ? e.metaKey : e.ctrlKey;
}
