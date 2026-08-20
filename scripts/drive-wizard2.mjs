// Drive the real wizard: starter indexers + qBittorrent install, then poll.
import puppeteer from "puppeteer-core";
const OUT = "C:/Users/Shanti/AppData/Local/Temp/claude/C--Users-Shanti/9f1d4765-0506-4f4e-aa6c-1e89728c9913/scratchpad";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const browser = await puppeteer.connect({ browserURL: "http://127.0.0.1:9223", defaultViewport: null });
const page = (await browser.pages())[0];

const clickText = (t) =>
  page.evaluate((txt) => {
    const b = [...document.querySelectorAll("button")].find((x) => x.textContent.trim().includes(txt));
    if (b) { b.click(); return true; }
    return false;
  }, t);

// 1) starter indexers
console.log("click starters:", await clickText("Add starter indexers"));

// 2) ensure the qBt install is going (click if the button is idle)
await sleep(1500);
const qbtClicked = await clickText("Install automatically");
console.log("clicked qbt install:", qbtClicked);

// 3) poll wizard state up to 8 minutes
for (let i = 0; i < 96; i++) {
  await sleep(5000);
  const t = await page.evaluate(() => document.body.innerText);
  const line = t.split("\n").filter((l) =>
    /qBittorrent|Prowlarr|indexer|crew|Web UI|winget|Enable/i.test(l)).slice(0, 10).join(" | ");
  console.log(`[${(i + 1) * 5}s]`, line.slice(0, 300));
  if (!/Welcome to Trawler/.test(t)) { console.log("WIZARD DISMISSED"); break; }
  if (/one more click to enable/i.test(t)) {
    console.log("clicking enable Web UI:", await clickText("Enable"));
  }
  if (/re-check now/.test(t) && i % 6 === 5) {
    await page.evaluate(() => {
      [...document.querySelectorAll("button")].find((x) => x.textContent.includes("re-check"))?.click();
    });
  }
}
await page.screenshot({ path: `${OUT}/wizard-02-progress.png` });
browser.disconnect();
