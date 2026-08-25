// Thin wrapper over the Tauri updater plugin so components stay testable in
// the browser (where updates don't exist and every call resolves to "none").

import { inTauri } from "./api";

export interface UpdateInfo {
  version: string;
  /** downloads the update; reports progress in bytes */
  download: (onProgress: (downloaded: number, total: number | null) => void) => Promise<void>;
  /**
   * Installs the downloaded update. On Windows this call does NOT resolve —
   * the plugin hands off to the installer and exits the process (the
   * installer relaunches the app). On Linux AppImage it resolves and the
   * caller should relaunch.
   */
  install: () => Promise<void>;
  /** frees the Rust-side resource (and any downloaded bytes) */
  close: () => Promise<void>;
}

type NativeUpdaterEvent =
  | { event: "Started"; data: { contentLength?: number } }
  | { event: "Progress"; data: { chunkLength: number } }
  | { event: "Finished"; data?: Record<string, never> };

export interface NativeUpdateHandle {
  version: string;
  download: (
    onEvent: (event: NativeUpdaterEvent) => void,
    options: { timeout: number },
  ) => Promise<void>;
  install: () => Promise<void>;
  close: () => Promise<void>;
}

/**
 * Owns the plugin resource across an in-flight download. The plugin creates
 * its downloaded-bytes resource only when download resolves, so closing the
 * update before then must wait and close again after that resource exists.
 */
export function wrapUpdate(update: NativeUpdateHandle): UpdateInfo {
  let downloadInFlight: Promise<void> | null = null;
  let closeRequested = false;
  let closePromise: Promise<void> | null = null;

  const close = (): Promise<void> => {
    closeRequested = true;
    if (!closePromise) {
      closePromise = (async () => {
        if (downloadInFlight) {
          try {
            await downloadInFlight;
          } catch {
            // The native Update still needs closing after a failed download.
          }
        }
        await update.close().catch(() => undefined);
      })();
    }
    return closePromise;
  };

  return {
    version: update.version,
    download: async (onProgress) => {
      if (closeRequested) throw new Error("this update handle has been closed");
      let downloaded = 0;
      let total: number | null = null;
      const operation = update.download((event) => {
        switch (event.event) {
          case "Started":
            total = event.data.contentLength ?? null;
            break;
          case "Progress":
            downloaded += event.data.chunkLength;
            onProgress(downloaded, total);
            break;
          case "Finished":
            onProgress(total ?? downloaded, total);
            break;
        }
      }, { timeout: 30 * 60_000 });
      downloadInFlight = operation;
      try {
        await operation;
      } finally {
        if (downloadInFlight === operation) downloadInFlight = null;
      }
    },
    install: () => update.install(),
    close,
  };
}

/** Returns the available update, or null when current / not applicable. */
export async function checkForUpdate(): Promise<UpdateInfo | null> {
  if (!inTauri) return null;
  const { check } = await import("@tauri-apps/plugin-updater");
  const update = await check({ timeout: 15_000 });
  if (!update) return null;
  return wrapUpdate(update as NativeUpdateHandle);
}

/** Restart into the freshly installed version (Linux path — see install()). */
export async function relaunchApp(): Promise<void> {
  if (!inTauri) return;
  const { relaunch } = await import("@tauri-apps/plugin-process");
  await relaunch();
}

/** The running app's real version (falls back to build-time in the browser). */
export async function currentVersion(): Promise<string> {
  if (!inTauri) return __APP_VERSION__;
  try {
    const { getVersion } = await import("@tauri-apps/api/app");
    return await getVersion();
  } catch {
    return __APP_VERSION__;
  }
}
