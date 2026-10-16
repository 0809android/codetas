// Real layout regression without launching the user's Google Chrome.app.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { promises as fs, constants } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const require = createRequire(new URL('../package.json', import.meta.url));
const ts = require('typescript');
const project = fileURLToPath(new URL('../../../', import.meta.url));
const work = process.env.CODETAS_TEST_WORK_DIR ?? path.join(project, '.ai/work/cache-health-ui');
let browser, socket, server, profile;
let cleanupPromise;
const pending = new Map();
let nextId = 1;
const close = () => cleanupPromise ??= (async () => {
  if (socket) { socket.close(); socket = undefined; }
  for (const { reject, timeout } of pending.values()) { clearTimeout(timeout); reject(new Error('browser closed')); }
  pending.clear();
  if (browser && browser.exitCode == null) {
    const process = browser;
    await new Promise(resolve => {
      const timer = setTimeout(() => { process.kill('SIGKILL'); resolve(); }, 1500);
      process.once('exit', () => { clearTimeout(timer); resolve(); });
      process.kill('SIGTERM');
    });
  }
  if (server) { server.closeAllConnections?.(); await new Promise(resolve => server.close(resolve)); }
  if (profile) await fs.rm(profile, { recursive: true, force: true });
})();
const onSignal = signal => { void close().finally(() => { process.exitCode = signal === 'SIGINT' ? 130 : 143; }); };
const onSigint=()=>onSignal('SIGINT'); const onSigterm=()=>onSignal('SIGTERM');
process.once('SIGINT', onSigint); process.once('SIGTERM', onSigterm);
const overallDeadline = setTimeout(() => {
  process.stderr.write('cache health UI regression timed out\n');
  process.exitCode = 1; void close();
}, 30000);
async function executable() {
  if (process.env.CODETAS_CHROMIUM_BIN) {
    assert(!process.env.CODETAS_CHROMIUM_BIN.includes('/Applications/Google Chrome.app/'), 'do not headlessly launch the user Chrome.app');
    const actual = await fs.realpath(process.env.CODETAS_CHROMIUM_BIN);
    assert(!actual.includes('/Applications/Google Chrome.app/'), 'do not headlessly launch the user Chrome.app through a symlink');
    return actual;
  }
  const root = process.env.PLAYWRIGHT_BROWSERS_PATH ?? (process.platform === 'darwin'
    ? path.join(os.homedir(), 'Library/Caches/ms-playwright')
    : process.platform === 'win32' ? path.join(process.env.LOCALAPPDATA, 'ms-playwright')
    : path.join(os.homedir(), '.cache/ms-playwright'));
  const names = (await fs.readdir(root)).filter(s => s.startsWith('chromium_headless_shell-'))
    .sort((a,b) => b.localeCompare(a, undefined, { numeric: true }));
  for (const name of names) for (const relative of [
    'chrome-headless-shell-mac-arm64/chrome-headless-shell', 'chrome-headless-shell-mac-x64/chrome-headless-shell',
    'chrome-headless-shell-linux64/chrome-headless-shell', 'chrome-headless-shell-win64/chrome-headless-shell.exe',
  ]) {
    const candidate = path.join(root, name, relative);
    if (await fs.access(candidate, constants.X_OK).then(() => true, () => false)) return candidate;
  }
  throw new Error('Playwright Chromium headless shell not found; set CODETAS_CHROMIUM_BIN to its executable');
}
async function command(method, params = {}, sessionId) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 10000);
    pending.set(id, { resolve, reject, timeout });
    socket.send(JSON.stringify({ id, method, params, ...(sessionId ? {sessionId} : {}) }));
  });
}
try {
  assert.equal(typeof WebSocket, 'function', 'Node 22+ WebSocket is required');
  const binary = await executable();
  await fs.mkdir(work, { recursive: true });
  profile = await fs.mkdtemp(path.join(work, 'browser-profile-'));
  const source = await fs.readFile(new URL('../src/cache-health.ts', import.meta.url), 'utf8');
  const js = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ES2022, target: ts.ScriptTarget.ES2022 } }).outputText;
  let css = await fs.readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  if (process.env.CODETAS_TEST_REPRO_OLD_BANNER === '1') {
    css = css.replace(/#cache-health-banner \.notice,\s*\.cache-health-panel \.notice\s*\{[^}]+\}/, '');
  }
  const html = `<!doctype html><html><head><meta charset="utf-8"><link rel="stylesheet" href="/styles.css">
    <style>.cache-health-table { min-width: 1100px; }</style></head><body>
    <div class="app-shell"><aside class="side-rail">CODETAS</aside><main class="workspace">
    <header class="topbar"><h1>状態・問題</h1></header>
    <div class="notice success" id="test-toast" role="status">設定を保存しました。<button>×</button></div>
    <div id="cache-health-banner"></div><section class="view"><div id="panel-host"></div></section></main></div>
    <script type="module">import * as api from '/cache-health.mjs';
      const now=Date.now(); const sessions=Array.from({length:128},(_,i)=>({
        sessionId:'01a13ddb-8d21-7cb2-8f00-'+i.toString(16).padStart(12,'0'),providerId:'test',model:'model-'+i,
        successfulRequests:20,measuredRequests:20,unknownCacheRequests:0,inputTokens:2000000,measuredInputTokens:2000000,
        cachedInputTokens:1600000,lastInputTokens:100000,firstEventAtMs:now-60000,lastEventAtMs:now,
        cacheHitPercent:80,rollingHitPercent:80,rollingRequests:20,activeAlert:true,alertSequence:i+1,lastAlertAtMs:now}));
      const snapshot={monitorId:'test',monitoring:true,policy:{minInputTokens:100000,windowRequests:20,thresholdPercent:90,
        recoveryPercent:95,cooldownMs:1800000},sessions,persistenceError:null,retentionDays:7,maxSessions:128};
      document.getElementById('cache-health-banner').innerHTML=api.renderCacheHealthBanner(snapshot);
      document.getElementById('panel-host').innerHTML=api.renderCacheHealthPanel(snapshot);
      window.cacheTest={api,snapshot};
    </script></body></html>`;
  server = createServer((req,res) => {
    res.setHeader('Cache-Control','no-store');
    res.setHeader('Content-Security-Policy', "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; font-src 'self' data:");
    if (req.url === '/styles.css') { res.setHeader('Content-Type','text/css'); res.end(css); }
    else if (req.url === '/cache-health.mjs') { res.setHeader('Content-Type','text/javascript'); res.end(js); }
    else if (req.url === '/') { res.setHeader('Content-Type','text/html; charset=utf-8'); res.end(html); }
    else { res.statusCode=404;res.end(); }
  });
  await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
  browser = spawn(binary, ['--headless','--disable-gpu','--no-sandbox','--disable-background-networking',
    '--remote-debugging-address=127.0.0.1','--remote-debugging-port=0',`--user-data-dir=${profile}`,'about:blank'],
    { stdio: ['ignore','ignore','pipe'] });
  const endpoint = await new Promise((resolve,reject) => {
    const timeout=setTimeout(()=>reject(new Error('headless shell did not expose CDP')),10000);
    let output='';
    browser.on('error',error=>{clearTimeout(timeout);reject(error);});
    browser.stderr.on('data',chunk=>{
      output=(output+chunk.toString()).slice(-65536);
      const found=/DevTools listening on (ws:\/\/[^\s]+)/.exec(output);
      if(found){clearTimeout(timeout);resolve(found[1]);}
    });
  });
  socket=new WebSocket(endpoint);
  socket.addEventListener('message',event=>{
    const message=JSON.parse(String(event.data)); const item=pending.get(message.id);
    if(!item)return;pending.delete(message.id);clearTimeout(item.timeout);
    if(message.error)item.reject(new Error(JSON.stringify(message.error)));else item.resolve(message.result);
  });
  await new Promise((resolve,reject)=>{socket.addEventListener('open',resolve,{once:true});socket.addEventListener('error',reject,{once:true});});
  const { targetId }=await command('Target.createTarget',{url:'about:blank'});
  const { sessionId }=await command('Target.attachToTarget',{targetId,flatten:true});
  await command('Emulation.setDeviceMetricsOverride',{width:1200,height:900,deviceScaleFactor:1,mobile:false},sessionId);
  await command('Page.navigate',{url:`http://127.0.0.1:${server.address().port}/`},sessionId);
  async function evaluate(expression){
    const result=await command('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true},sessionId);
    if(result.exceptionDetails)throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  }
  for(let n=0;n<100;n++){
    if(await evaluate('Boolean(window.cacheTest)'))break;
    if(n===99)throw new Error('test UI did not initialize');
    await new Promise(resolve=>setTimeout(resolve,50));
  }
  const layout=await evaluate(`(()=>{const banner=document.querySelector('#cache-health-banner .notice');
    const toast=document.getElementById('test-toast');const rect=toast.getBoundingClientRect();
    const top=document.elementFromPoint(rect.left+rect.width/2,rect.top+rect.height/2);
    return{bannerPosition:getComputedStyle(banner).position,toastPosition:getComputedStyle(toast).position,
      toastTopmost:top===toast||toast.contains(top),bannerHeight:banner.getBoundingClientRect().height,
      toastWidth:rect.width,toastHeight:rect.height};})()`);
  assert.equal(layout.bannerPosition,'static','persistent banner must not use the fixed toast position');
  assert.equal(layout.toastPosition,'fixed','ordinary notifications retain their toast position');
  assert(layout.toastTopmost,'persistent cache warning must not obscure ordinary notifications');
  assert(layout.bannerHeight>0&&layout.toastWidth>0&&layout.toastHeight>0,'both notifications must be visible');
  if(process.env.CODETAS_TEST_REPRO_OLD_SCROLL==='1')await evaluate('window.reproduceOldScroll=true');
  const scrolling=await evaluate(`(()=>{let panel=document.getElementById('cache-health-panel');
    const scroll=panel.querySelector('.cache-health-scroll');scroll.focus();scroll.scrollTop=310;scroll.scrollLeft=45;
    const before={top:scroll.scrollTop,left:scroll.scrollLeft};
    const next={...cacheTest.snapshot,sessions:cacheTest.snapshot.sessions.map(s=>({...s,successfulRequests:s.successfulRequests+1}))};
    if(window.reproduceOldScroll){panel.outerHTML=cacheTest.api.renderCacheHealthPanel(next);panel=document.getElementById('cache-health-panel');}
    else panel=cacheTest.api.updateCacheHealthPanel(panel,next);const after=panel.querySelector('.cache-health-scroll');
    const position={top:after.scrollTop,left:after.scrollLeft};
    const focused=document.activeElement===after;const overflow=after.scrollHeight>after.clientHeight;
    const unchanged=cacheTest.api.updateCacheHealthPanel(panel,next);
    return{before,after:position,sameNode:unchanged===panel,focused,overflow};})()`);
  assert(scrolling.overflow,'fixture must actually scroll');assert(scrolling.before.top>0);
  assert.deepEqual(scrolling.after,scrolling.before,'live update must preserve both scroll offsets');
  assert(scrolling.sameNode,'unchanged data must retain the existing DOM');
  assert(scrolling.focused,'live update must preserve keyboard focus');
  console.log(JSON.stringify({ok:true,layout,scrolling}));
} finally {
  clearTimeout(overallDeadline);
  await close();
  process.removeListener('SIGINT',onSigint);process.removeListener('SIGTERM',onSigterm);
}
