// Synthetic rendering only: no Tauri bridge, credentials, settings, or provider traffic.
// PLAYWRIGHT_MODULE points to an installed playwright/index.mjs, as in direct-api-ui.mjs.
import assert from 'node:assert/strict';
import { createServer } from 'vite';

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE);
let browserLaunch;
let closing = false;
let browserCleanup;
const closeBrowser = () => {
  closing = true;
  return browserCleanup ??= (async () => {
    // A timeout or signal can arrive before launch resolves. Await that
    // same launch so its eventual browser cannot escape cached cleanup.
    const browser = await browserLaunch?.catch(() => undefined);
    await browser?.close();
  })();
};
const server = await createServer({
  root: new URL('../', import.meta.url).pathname,
  configFile: false,
  plugins: [{ name: 'connection-layout-fixture',
    // Vite also handles SIGTERM: do not let its shutdown exit before launch settles.
    closeBundle: closeBrowser,
    configureServer(server) {
      server.middlewares.use('/__connection-layout', (_req, res) => {
        res.setHeader('Content-Type', 'text/html; charset=utf-8');
        res.end(`<!doctype html><meta charset="utf-8"><link rel="stylesheet" href="/src/styles.css">
          <main id="fixture" style="padding:16px"></main><script type="module">
          import { state } from '/src/state.ts';
          import { setLanguage } from '/src/i18n.ts';
          import { renderLocalCliRows, renderDirectApiPanel } from '/src/views.ts';
          state.localClis = { deep: false, clients: [
            { id: 'claude', name: 'Claude Code', installed: true, probeState: 'ready',
              canRegister: true, needsCodetasRegistration: true, version: '1.2.3',
              message: 'インストール済みです。長い説明文も親要素の中で折り返します。', registrationHint: '' },
            { id: 'github-copilot', name: 'GitHub Copilot CLI', installed: true, probeState: 'ready',
              canRegister: false, needsCodetasRegistration: false, codetasProviderId: 'github',
              message: 'https://example.invalid/' + 'a'.repeat(100), registrationHint: '' },
          ] };
          state.directApis = [
            { providerId: 'anthropic-apikey', name: 'Claude API (Anthropic)', hint: 'ANTHROPIC_API_KEY' },
            { providerId: 'openai-api', name: 'OpenAI API (Codex models)', hint: 'OPENAI_API_KEY' },
          ];
          window.renderFixture = (width, language) => {
            setLanguage(language);
            document.getElementById('fixture').innerHTML =
              '<div style="width:' + width + 'px"><section class="panel local-cli-panel">' +
              '<header><h3>ローカル CLI の検出</h3></header><div class="local-cli-list">' +
              renderLocalCliRows() + '</div></section>' + renderDirectApiPanel() + '</div>';
          };
          </script>`);
      });
  } }],
  server: { host: '127.0.0.1', port: 0 },
});
let cleanup;
const close = () => {
  closing = true;
  return cleanup ??= (async () => {
    try {
      await closeBrowser();
    } finally {
      await server.close();
    }
  })();
};
const stop = () => { void close().finally(() => process.exit(130)); };
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, stop);
const deadline = setTimeout(() => {
  console.error('connection layout regression timed out');
  process.exitCode = 1;
  void close();
}, 30000);
try {
  await server.listen();
  if (closing) throw new Error('connection layout regression stopped before browser launch');
  browserLaunch = chromium.launch({ headless: true });
  const browser = await browserLaunch;
  if (closing) throw new Error('connection layout regression stopped during browser launch');
  const page = await browser.newPage({ viewport: { width: 1360, height: 1000 } });
  page.setDefaultTimeout(5000);
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/__connection-layout`);
  await page.waitForFunction(() => typeof window.renderFixture === 'function').catch(error => { console.error(errors); throw error; });
  const results = [];
  for (const viewport of [760, 1360]) {
    await page.setViewportSize({ width: viewport, height: 1000 });
    for (const language of ['ja', 'en', 'ko']) {
      for (const width of [320, 360, 620, 621, 960]) {
        if (width + 32 > viewport) continue;
        await page.evaluate(({ width, language }) => window.renderFixture(width, language), { width, language });
        const problems = await page.evaluate(() => {
          const issues = [];
          for (const panel of document.querySelectorAll('.local-cli-panel')) {
            if (panel.scrollWidth > panel.clientWidth + 1) issues.push('panel overflow');
            for (const row of panel.querySelectorAll('.local-cli-row')) {
              const bounds = row.getBoundingClientRect();
              if (row.scrollWidth > row.clientWidth + 1) issues.push('row overflow');
              for (const child of row.querySelectorAll('*')) {
                const box = child.getBoundingClientRect();
                if (box.left < bounds.left - 1 || box.right > bounds.right + 1)
                  issues.push('outside row: ' + child.tagName + ' ' + child.textContent);
              }
              const name = row.querySelector('strong').getBoundingClientRect();
              const chip = row.querySelector('.chip').getBoundingClientRect();
              if (name.right > chip.left + 1 && name.top < chip.bottom && name.bottom > chip.top)
                issues.push('name overlaps status');
            }
          }
          return issues;
        });
        assert.deepEqual(problems, [], JSON.stringify({ viewport, language, width, problems }));
        results.push({ viewport, language, width });
      }
    }
  }
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ ok: true, cases: results.length, results }));
} finally {
  clearTimeout(deadline);
  await close();
  for (const signal of ['SIGINT', 'SIGTERM']) process.removeListener(signal, stop);
}
