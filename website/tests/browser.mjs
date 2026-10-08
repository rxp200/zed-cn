import { chromium } from 'playwright';
import assert from 'node:assert/strict';
const browser=process.env.CDP_URL?await chromium.connectOverCDP(process.env.CDP_URL):await chromium.launch({executablePath:process.env.CHROMIUM_PATH,headless:true,args:['--no-sandbox']});
const url=process.env.SITE_URL||'http://127.0.0.1:4173/zed-cn/';let checks=0;
function check(value,message){assert.ok(value,message);checks++}
const context=await browser.newContext({viewport:{width:1440,height:1000}}),page=await context.newPage(),errors=[];page.on('pageerror',e=>errors.push(e.message));
await page.goto(url,{waitUntil:'networkidle'});await page.waitForSelector('.asset-link');check(await page.locator('.asset-link').count()===5,'real stable five desktop assets');check(await page.locator('.menu-toggle').isHidden(),'desktop menu toggle hidden');
await page.locator('#tab-remote').click();check(await page.locator('#panel-remote').isVisible(),'remote tab visible');check(await page.locator('#panel-ai').isHidden(),'AI tab hidden');await page.locator('#tab-remote').press('ArrowRight');check(await page.locator('#panel-git').isVisible(),'keyboard tab switch');
await page.locator('[data-channel=dev]').click();await page.waitForFunction(()=>document.querySelector('#feed-state').textContent.startsWith('DEV /'));check((await page.locator('.asset-link').first().getAttribute('href')).includes('zed-cn-dev-'),'real dev channel URLs');await page.locator('[data-channel=stable]').click();await page.waitForFunction(()=>document.querySelector('#feed-state').textContent.startsWith('STABLE /'));
for(const width of [320,390,540,768,1024,1440]){await page.setViewportSize({width,height:900});check(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),`no overflow ${width}`)}
await page.setViewportSize({width:390,height:844});await page.locator('.menu-toggle').click();check(await page.locator('#navigation').isVisible(),'mobile menu opens');await page.keyboard.press('Escape');check(await page.locator('#navigation').isHidden(),'Escape closes');check(await page.locator('.menu-toggle').evaluate(e=>document.activeElement===e),'focus restored');
await page.locator('.menu-toggle').click();await page.locator('#navigation a[href="#questions"]').click();check(await page.locator('#navigation').isHidden(),'navigation closes menu');
check(errors.length===0,`no runtime errors: ${errors}`);
await context.close();
const reduced=await browser.newContext({reducedMotion:'reduce'}),r=await reduced.newPage();await r.goto(url,{waitUntil:'networkidle'});check(await r.locator('#sculpture canvas').count()===0,'reduced motion skips WebGL');await reduced.close();
const nojs=await browser.newContext({javaScriptEnabled:false,viewport:{width:390,height:844}}),n=await nojs.newPage();await n.goto(url);check(await n.locator('h1').isVisible(),'no-JS content');check(await n.locator('#navigation').isVisible(),'no-JS navigation');check(await n.locator('noscript').isVisible(),'no-JS download fallback');await nojs.close();
const fallback=await browser.newContext(),f=await fallback.newPage();await f.route('**/updates*.json',route=>route.fulfill({status:503,body:'unavailable'}));await f.goto(url,{waitUntil:'networkidle'});check(await f.locator('#download-list a').getAttribute('href')==='https://github.com/rxp200/zed-cn/releases','network failure fallback');await fallback.close();
await browser.close();console.log(`Browser regression: ${checks} checks passed`);
