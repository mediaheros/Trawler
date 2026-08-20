// Agent-view states for the chat polish pass.
import puppeteer from "puppeteer-core";
import { mkdirSync } from "node:fs";
import { join } from "node:path";

const OUT = process.argv[2] ?? "shots";
mkdirSync(OUT, { recursive: true });
const browser = await puppeteer.launch({
  executablePath: "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
  headless: "new",
  args: ["--no-first-run", "--hide-scrollbars"],
});
const page = await browser.newPage();
await page.setViewport({ width: 1440, height: 900, deviceScaleFactor: 2 });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const shot = async (name) => {
  await page.screenshot({ path: join(OUT, `${name}.png`) });
  console.log("shot:", name);
};

await page.goto("http://localhost:1420/?shot=1", { waitUntil: "networkidle2" });
await sleep(1100);
await page.evaluate(() => {
  [...document.querySelectorAll("nav button")]
    .find((b) => (b.getAttribute("title") || "").startsWith("Agent"))
    ?.click();
});
await sleep(600);
await shot("agent-1-empty");

// focus the composer to show the focus treatment
await page.evaluate(() => document.querySelector("textarea")?.focus());
await sleep(300);
await shot("agent-2-focused");

// type a multi-line message to see autosize
await page.type("textarea", "Find the F1 Hungarian GP race weekend in 1080p,\nunder 12 GB, race only");
await sleep(300);
await shot("agent-3-typed");

// send → mock run streams steps then replies
await page.evaluate(() => {
  const btns = [...document.querySelectorAll("button")];
  btns.find((b) => b.querySelector("svg.lucide-arrow-up"))?.click();
});
await sleep(2600);
await shot("agent-4-running");
await sleep(3500);
await shot("agent-5-conversation");

await browser.close();
console.log("done");
