import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';
import fs from 'node:fs';
const { chromium } = await import(pathToFileURL(process.env.RYOKAN_PLAYWRIGHT_MODULE).href);
const executablePath = process.env.RYOKAN_CHROME_BIN || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const browser = await chromium.launch({ executablePath, headless: true });
const context = await browser.newContext({ viewport: { width: 1440, height: 1000 }, timezoneId: 'Europe/Vienna', locale: 'en-GB' });
const page = await context.newPage();
const errors = [];
page.on('pageerror', error => errors.push(error.message));
await page.route('**/*', route => new URL(route.request().url()).origin === process.argv[2] ? route.continue() : route.abort());
const screenshots = process.env.RYOKAN_CALENDAR_SCREENSHOTS || '/private/tmp/ryokan-calendar-screenshots';
fs.mkdirSync(screenshots, { recursive: true });
const date = async () => page.locator('.calendar-period').getAttribute('data-date');
async function expectDate(value) { await page.waitForFunction(value => document.querySelector('.calendar-period')?.dataset.date === value, value); }
async function click(name) { await page.getByRole(['Day','Week','Month'].includes(name) ? 'tab' : 'link', { name, exact: true }).click(); }
function verifyContext() {
    const url = new URL(page.url());
    assert.equal(url.searchParams.get('monitored'), 'true');
    assert.equal(url.searchParams.get('search'), 'Calendar');
    assert.equal(url.searchParams.get('external'), 'true');
    assert.equal(url.searchParams.get('timezone'), 'Europe/Vienna');
}
try {
    await page.goto(process.argv[2] + '/calendar?range=day&date=2026-01-01&monitored=true&search=Calendar&external=true&timezone=Europe%2FVienna');
    await expectDate('2026-01-01');
    await page.waitForFunction(() => !!window.htmx);
    await page.screenshot({ path: screenshots + '/calendar-desktop.png', fullPage: true });
    assert.equal(await page.locator('.calendar-episode-time').first().innerText(), '13:00');
    await click('Previous'); await expectDate('2025-12-31'); verifyContext();
    assert.equal(await page.locator('[data-range="day"][role="tab"]').getAttribute('aria-selected'), 'true');
    await page.goBack(); await expectDate('2026-01-01'); verifyContext();
    await page.goForward(); await expectDate('2025-12-31'); verifyContext();
    await click('Week');
    await page.waitForFunction(() => document.querySelector('.calendar-period')?.dataset.range === 'this_week');
    await click('Previous'); await expectDate('2025-12-24'); verifyContext();
    assert.match(await page.locator('.calendar-empty').innerText(), /No saved episodes/);
    await click('Today');
    await page.waitForFunction(() => document.querySelector('.calendar-period')?.dataset.date !== '2025-12-24');
    assert.equal(await page.locator('.calendar-period').getAttribute('data-range'), 'this_week');
    await click('Month'); await page.locator('.calendar-month-grid').waitFor(); verifyContext();
    await page.locator('#calendar-date').fill('2024-02-29');
    await page.getByRole('button', { name: 'Go', exact: true }).click(); await expectDate('2024-02-29');
    await click('Day');
    await page.waitForFunction(() => document.querySelector('.calendar-period')?.dataset.range === 'day');
    await click('Next'); await expectDate('2024-03-01'); verifyContext();
    await page.setViewportSize({ width: 375, height: 812 });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    assert.equal(await page.getByRole('link', { name: 'Previous', exact: true }).isVisible(), true);
    await page.getByRole('link', { name: 'Previous', exact: true }).focus();
    await page.keyboard.press('Enter'); await expectDate('2024-02-29'); verifyContext();
    await page.screenshot({ path: screenshots + '/calendar-mobile.png', fullPage: true });
    assert.deepEqual(errors, []);
    console.log('PASS calendar: yesterday, back/forward, day/week/month, leap day, filters, timezone, mobile overflow and keyboard navigation');
    console.log('Screenshots: ' + screenshots);
} finally { await browser.close(); }
