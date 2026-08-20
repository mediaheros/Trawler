import puppeteer from "puppeteer-core";
import { mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
const OUT = fileURLToPath(new URL(".out/", import.meta.url));
mkdirSync(OUT, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const browser = await puppeteer.launch({
  executablePath: "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
  headless: "new",
});
const page = await browser.newPage();
await page.setViewport({ width: 1440, height: 950, deviceScaleFactor: 2 });
await page.goto("file:///C:/Users/Shanti/src/trawler/docs/index.html", { waitUntil: "networkidle0" });
await sleep(800);
// force reveal animations done
await page.evaluate(() => document.querySelectorAll(".reveal").forEach((el) => el.classList.add("in")));
await sleep(300);
await page.evaluate(() => document.querySelector("#download")?.scrollIntoView());
await sleep(600);
await page.screenshot({ path: `${OUT}/site-dl-desktop.png` });
await page.setViewport({ width: 390, height: 844, deviceScaleFactor: 2 });
await sleep(400);
await page.evaluate(() => document.querySelector("#download")?.scrollIntoView());
await sleep(500);
await page.screenshot({ path: `${OUT}/site-dl-mobile.png` });
await browser.close();
console.log("site shots done");
