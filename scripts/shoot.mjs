// Screenshot rig: drives the vite dev server (mock mode) through every view
// and captures 2x-retina PNGs for the visual review.
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
const shot = async (name) => {
  await sleep(120);
  await page.screenshot({ path: join(OUT, `${name}.png`) });
  console.log("shot:", name);
};
const nav = async (label) => {
  await page.evaluate((l) => {
    [...document.querySelectorAll("nav button")]
      .find((b) => (b.getAttribute("title") || "").startsWith(l))
      ?.click();
  }, label);
  await sleep(350);
};
const clickText = async (text, selector = "button") => {
  await page.evaluate(
    (t, sel) => {
      [...document.querySelectorAll(sel)].find((b) => b.textContent.trim() === t)?.click();
    },
    text,
    selector,
  );
  await sleep(350);
};

await page.goto("http://localhost:1420/?shot=1", { waitUntil: "networkidle2" });
await sleep(1200);

// 1. home hero
await shot("01-home");

// 2. search results
await page.evaluate(() => {
  const input = document.querySelector("input[placeholder*='Search movies']");
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value").set;
  setter.call(input, "severance");
  input.dispatchEvent(new Event("input", { bubbles: true }));
});
await clickText("Search");
await sleep(1600);
await shot("02-search-results");

// 3. agent view
await nav("Agent");
await sleep(600);
await shot("03-agent");

// 4. shows library
await nav("Shows");
await sleep(700);
await shot("04-shows-library");

// 5. show detail drawer
await page.evaluate(() => {
  [...document.querySelectorAll("div")]
    .filter((d) => d.className.includes && /cursor-pointer/.test(d.className))
    .find((d) => /Severance/.test(d.textContent))
    ?.click();
});
await sleep(700);
await shot("05-show-detail");
await page.keyboard.press("Escape");
await sleep(300);

// 6. calendar
await clickText("Calendar");
await sleep(900);
await shot("06-calendar");

// 7. discover
await clickText("Discover");
await sleep(900);
await shot("07-discover");

// 8. show preview modal
await page.evaluate(() => {
  document.querySelector('[role="button"][tabindex="0"]')?.click();
});
await sleep(800);
await shot("08-show-preview");
await page.keyboard.press("Escape");
await sleep(300);

// 9. import modal (input phase)
await clickText("Library");
await sleep(400);
await clickText("Import");
await sleep(400);
await shot("09-import-input");

// 10. import review phase
await page.evaluate(() => {
  const ta = document.querySelector('[role="dialog"][aria-label="Import watchlist"] textarea');
  const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, "value").set;
  setter.call(ta, "Severance\nBattlestar Galactica\nDark\nSome Unknown Show\nThe Expanse\nAndor");
  ta.dispatchEvent(new Event("input", { bubbles: true }));
});
await clickText("Find matches");
await sleep(900);
await shot("10-import-review");
await page.keyboard.press("Escape");
await sleep(300);

// 11. downloads
await nav("Downloads");
await sleep(800);
await shot("11-downloads");

// 12-18. settings tabs
await nav("Settings");
await sleep(500);
await shot("12-settings-connections");
for (const [tab, name] of [
  ["Indexers", "13-settings-indexers"],
  ["Grabbing", "14-settings-grabbing"],
  ["Following", "15-settings-following"],
  ["Notifications", "16-settings-notifications"],
  ["Agent", "17-settings-agent"],
  ["App", "18-settings-app"],
]) {
  await clickText(tab);
  await sleep(400);
  await shot(name);
}

await browser.close();
console.log("done");
