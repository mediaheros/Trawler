// Pre-deploy guard: refuse to deploy a docs/ tree whose updater manifest is
// OLDER than what mediahero.org already serves. docs/dl/ is gitignored, so a
// second machine's checkout silently carries a stale copy — deploying it
// would wipe the newest release from the site and roll every install's
// updater back. Run before `wrangler pages deploy`; exits non-zero on danger.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const localManifest = join(root, "docs", "dl", "latest.json");

const parseVer = (v) => String(v).trim().split(".").map((n) => parseInt(n, 10) || 0);
const cmp = (a, b) => {
  const [x, y] = [parseVer(a), parseVer(b)];
  for (let i = 0; i < 3; i++) if ((x[i] || 0) !== (y[i] || 0)) return (x[i] || 0) - (y[i] || 0);
  return 0;
};

let local;
try {
  local = JSON.parse(readFileSync(localManifest, "utf8")).version;
} catch (e) {
  console.error(`BLOCKED: cannot read ${localManifest} (${e.message})`);
  console.error("A docs/ tree without dl/latest.json must never be deployed.");
  process.exit(1);
}

let live;
try {
  const res = await fetch("https://mediahero.org/dl/latest.json", { signal: AbortSignal.timeout(15000) });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  live = (await res.json()).version;
} catch (e) {
  console.error(`WARNING: could not read the live manifest (${e.message}).`);
  console.error(`Local manifest is ${local}. If the site is reachable in a browser, investigate before deploying.`);
  process.exit(1);
}

if (cmp(local, live) < 0) {
  console.error(`BLOCKED: local manifest is ${local} but mediahero.org already serves ${live}.`);
  console.error("Deploying would roll the updater back for every install.");
  console.error("Sync docs/dl/ from the machine that built the newer release (or from the live site) first.");
  process.exit(1);
}
console.log(`ok: local ${local} vs live ${live} — safe to deploy`);
