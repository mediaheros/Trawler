// Attach to the REAL installed Trawler over CDP and screenshot current state.
import puppeteer from "puppeteer-core";
const OUT = "C:/Users/Shanti/AppData/Local/Temp/claude/C--Users-Shanti/9f1d4765-0506-4f4e-aa6c-1e89728c9913/scratchpad";

const browser = await puppeteer.connect({
  browserURL: "http://127.0.0.1:9223",
  defaultViewport: null,
});
const pages = await browser.pages();
const page = pages.find((p) => p.url().includes("index.html") || p.url().includes("tauri")) ?? pages[0];
console.log("attached to:", page.url());
await page.screenshot({ path: `${OUT}/wizard-01-initial.png` });
const text = await page.evaluate(() => document.body.innerText.slice(0, 1200));
console.log("---PAGE TEXT---");
console.log(text);
browser.disconnect();
