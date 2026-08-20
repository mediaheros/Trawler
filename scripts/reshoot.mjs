// Focused recapture: show-detail drawer (with QualitySelect) + settings after
// the alignment fix.
import puppeteer from "puppeteer-core";
import { mkdirSync } from "node:fs";
import { join } from "node:path";

const OUT = process.argv[2] ?? "shots";
mkdirSync(OUT, { recursive: true });

const browser = await puppeteer.launch({
  executablePath: "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
  headless: "new",
  args: ["--no-first-run", "--disable-extensions", "--hide-scrollbars"],
});
const page = await browser.newPage();
await page.setViewport({ width: 1440, height: 900, deviceScaleFactor: 2 });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

await page.goto("http://localhost:1420/?shot=1", { waitUntil: "networkidle2" });
await sleep(1200);

// shows → open the first show card via its poster image
await page.evaluate(() => {
  [...document.querySelectorAll("nav button")]
    .find((b) => (b.getAttribute("title") || "").startsWith("Shows"))
    ?.click();
});
await sleep(600);
await page.evaluate(() => {
  // the grid card is the clickable wrapper around the poster
  const grid = [...document.querySelectorAll("div")].find((d) =>
    /grid-cols-\[repeat/.test(d.className || ""),
  );
  grid?.firstElementChild?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
});
await sleep(900);
await page.screenshot({ path: join(OUT, "05-show-detail.png") });
console.log("shot: 05-show-detail");
await page.keyboard.press("Escape");
await sleep(300);

// settings after alignment fix
await page.evaluate(() => {
  [...document.querySelectorAll("nav button")]
    .find((b) => (b.getAttribute("title") || "").startsWith("Settings"))
    ?.click();
});
await sleep(500);
await page.evaluate(() => {
  [...document.querySelectorAll("button")].find((b) => b.textContent.trim() === "Following")?.click();
});
await sleep(400);
await page.screenshot({ path: join(OUT, "15b-settings-following.png") });
console.log("shot: 15b-settings-following");
await page.evaluate(() => {
  [...document.querySelectorAll("button")].find((b) => b.textContent.trim() === "App")?.click();
});
await sleep(400);
await page.screenshot({ path: join(OUT, "18b-settings-app.png") });
console.log("shot: 18b-settings-app");

await browser.close();
console.log("done");
