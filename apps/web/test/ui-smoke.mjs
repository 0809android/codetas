// Run against the local Vite server with PLAYWRIGHT_MODULE pointing to an installed
// playwright/index.mjs. Only synthetic Tauri responses are used; no provider calls.
import fs from 'node:fs';
import assert from 'node:assert/strict';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE);
const fixture = JSON.parse(fs.readFileSync(new URL('./fixtures/gateway.json', import.meta.url), 'utf8'));
const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROMIUM_EXECUTABLE });
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, async () => { await browser.close(); process.exit(130); });
try {
 const page = await browser.newPage({ viewport: { width: 1360, height: 920 } });
 const errors = []; page.on('pageerror', e => { errors.push(e.message); console.error('PAGE ERROR',e.message); });
 await page.addInitScript(({configuration,status}) => {
  localStorage.setItem('codetas.language','ja'); window.__calls = []; window.__config = configuration;
  window.__TAURI_INTERNALS__ = { invoke: async (command,args={}) => {
   window.__calls.push({command,args});
   switch(command) {
    case 'gateway_configuration': return structuredClone(window.__config);
    case 'provider_gateway_status': return status;
    case 'list_provider_presets': case 'list_gateway_observability_trash': case 'list_direct_api_targets': case 'oauth_provider_registry': case 'gateway_route_dry_runs': case 'list_codex_maintenance_jobs': return [];
    case 'scan_local_cli_clients': return {deep:false,clients:[]};
    case 'gateway_observability_summary': return {totalRequests:10,successfulRequests:9,failedRequests:1,inputTokens:100,outputTokens:40,totalTokens:140,storageBytes:0};
    case 'gateway_observability_breakdown': return {scannedEvents:10,truncated:false,providers:[],models:[],surfaces:[],daily:[]};
    case 'gateway_service_status': return {supported:true,installed:false,running:false};
    case 'codex_plugin_status': return {installed:true,enabled:true,mcpHealthy:true,gatewayConnected:true,gatewayReachable:true};
    case 'gateway_diagnostics': return {checks:[{id:'gateway-running',level:'pass',summary:'Gateway起動中',remediation:null},{id:'provider-env-test',level:'error',summary:'テスト用の認証設定がありません',remediation:'接続画面で認証を設定してください'}],passed:1,warnings:0,errors:1};
    case 'gateway_recent_events': if(window.__failRecent) throw new Error('synthetic log failure'); return [{requestId:'test-event',ledgerSequence:1,timestampMs:Date.now(),providerId:'test',exposedModel:'test/test-model',upstreamModel:'test-model',statusCode:200,sendCount:1,totalTokens:0,recoveryKinds:['local-compaction-offline'],attemptOnly:false,shadow:false,upstreamError:{statusCode:429,errorMessage:'Individual quota reached. Resets in 2h0m0s.'}}];
    case 'test_gateway_provider': return {providerId:'test',reachable:true,authenticated:false,status:401,message:'認証が必要です',latencyMs:5,modelCount:0};
    case 'save_gateway_configuration': window.__config=args.configuration; return structuredClone(window.__config);
    case 'upsert_gateway_provider': window.__config.providers[0]=args.input.provider; return status;
    case 'sync_codex_model_catalog': return '/mock/catalog.json';
    default: throw new Error('Unmocked command: '+command);
   }
  }};
 },fixture);
 await page.goto(process.env.UI_URL ?? 'http://127.0.0.1:4187/');
 await page.getByRole('button',{name:'接続',exact:true}).waitFor();
 await page.locator('.provider-card, .provider-list').first().waitFor();
 assert.deepEqual(await page.locator('.nav-label').allTextContents(),['接続','モデル','状態・問題','設定']);
 await page.locator('[data-view="models"]').click();
 await page.locator('#model-list').waitFor();
 await page.locator('#model-search').fill('not-a-model');
 assert.equal(await page.locator('#model-list [data-action="toggle-codex-model"]').count(),0);
 await page.locator('#model-search').fill('test');
 assert.ok(await page.locator('#model-list [data-action="toggle-codex-model"]').count()>0);
 await page.locator('[data-action="sync-catalog"]').first().click();
 await page.locator('[data-view="status"]').click();
 await page.getByRole('button',{name:'状態を確認（推論なし）',exact:true}).click();
 await page.getByText('テスト用の認証設定がありません',{exact:true}).waitFor();
 await page.getByText('test/test-model：利用枠または送信頻度の制限',{exact:true}).waitFor();
 await page.evaluate(()=>{window.__failRecent=true});
 await page.getByRole('button',{name:'状態を確認（推論なし）',exact:true}).click();
 await page.getByText(/ログを取得できませんでした/).waitFor();
 assert.equal(await page.getByText('test/test-model：利用枠または送信頻度の制限',{exact:true}).count(),0);
 await page.evaluate(()=>{window.__failRecent=false});
 await page.getByRole('button',{name:'状態を確認（推論なし）',exact:true}).click();
 await page.getByRole('button',{name:'接続を確認（推論なし）',exact:true}).click();
 await page.getByRole('heading',{name:'テスト接続 — 要確認'}).waitFor();
 assert.ok(await page.locator('.notice.error').count()>0,'401 must not show success');
 await page.screenshot({path:'/tmp/codetas-status-check.png',fullPage:true});
 await page.locator('[data-view="settings"]').click();
 const auto = page.locator('[name="autoConnect"]');await auto.uncheck();
 await page.locator('#settings-form button[type="submit"]').click();
 await page.waitForFunction(()=>window.__calls.some(c=>c.command==='save_gateway_configuration'));
 assert.equal(await page.evaluate(()=>window.__config.codex.autoConnect),false);
 assert.equal(await page.locator('textarea[data-context-file], [data-action="toggle-skill-enabled"]').count(),0);
 await page.setViewportSize({width:390,height:844});
 await page.locator('[data-view="status"]').click();
 assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+2),'no horizontal overflow');
 await page.screenshot({path:'/tmp/codetas-status-mobile.png',fullPage:true});
 const calls=await page.evaluate(()=>window.__calls.map(c=>c.command));
 assert.ok(!calls.some(c=>/skill|context_document|hermes_profile/.test(c)));
 assert.deepEqual(errors,[]);
 console.log(JSON.stringify({pages:4,checks:['navigation','model-search','catalog-sync','diagnostics','quota-fallback-visible','401-not-success','settings-save','no-skill-management','mobile-layout'],commands:[...new Set(calls)],errors},null,2));
} finally { await browser.close(); }
