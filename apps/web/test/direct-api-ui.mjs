// Run with a local Vite server and PLAYWRIGHT_MODULE pointing to playwright/index.mjs.
// Synthetic Tauri responses only: no credentials, real settings, or provider traffic.
import assert from 'node:assert/strict';
import fs from 'node:fs';

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE);
const fixture = JSON.parse(fs.readFileSync(new URL('./direct-api-fixture.json', import.meta.url), 'utf8'));
const template = fixture.configuration.providers[0];
fixture.configuration.providers.push(
  { ...structuredClone(template), id: 'anthropic', name: 'Claude subscription', credential: { source: 'oAuth', reference: 'anthropic', transport: 'bearer', command: null } },
  { ...structuredClone(template), id: 'openai', name: 'Codex subscription', credential: { source: 'forward', reference: null, transport: 'bearer', command: null } },
);
const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROMIUM_EXECUTABLE });
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.once(signal, async () => { await browser.close(); process.exit(130); });
}
try {
  const page = await browser.newPage({ viewport: { width: 1360, height: 920 } });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('dialog', (dialog) => dialog.accept());
  await page.addInitScript(({ configuration, status }) => {
    localStorage.setItem('codetas.language', 'ja');
    window.__config = configuration;
    window.__calls = [];
    const targets = [
      { providerId: 'anthropic-apikey', name: 'Claude API (Anthropic)', hint: 'ANTHROPIC_API_KEY' },
      { providerId: 'openai-api', name: 'OpenAI API (Codex models)', hint: 'OPENAI_API_KEY' },
    ];
    const makeProvider = (id) => ({
      ...structuredClone(configuration.providers[0]), id,
      name: id === 'anthropic-apikey' ? 'Claude API (Anthropic)' : 'OpenAI API',
      protocol: id === 'anthropic-apikey' ? 'anthropicMessages' : 'responses',
      baseUrl: id === 'anthropic-apikey' ? 'https://api.anthropic.com/v1' : 'https://api.openai.com/v1',
      apiKeyEnv: id === 'anthropic-apikey' ? 'ANTHROPIC_API_KEY' : 'OPENAI_API_KEY',
      credential: {
        source: 'environment', command: null, headerName: null,
        reference: id === 'anthropic-apikey' ? 'ANTHROPIC_API_KEY' : 'OPENAI_API_KEY',
        transport: id === 'anthropic-apikey' ? 'xApiKey' : 'bearer',
      },
    });
    window.__TAURI_INTERNALS__ = { invoke: async (command, args = {}) => {
      window.__calls.push({ command, args });
      switch (command) {
        case 'gateway_configuration': return structuredClone(window.__config);
        case 'provider_gateway_status': return status;
        case 'list_direct_api_targets': return targets;
        case 'list_provider_presets': return [{ id: 'openai-apikey', name: 'OpenAI API (legacy ID)' }];
        case 'list_gateway_observability_trash': case 'oauth_provider_registry':
        case 'list_hermes_profiles': case 'list_hermes_editable_files':
        case 'gateway_route_dry_runs': case 'list_codex_maintenance_jobs': return [];
        case 'scan_hermes_sync': return null;
        case 'scan_local_cli_clients': return { deep: false, clients: [] };
        case 'gateway_observability_summary': return {};
        case 'gateway_observability_breakdown': return { providers: [], models: [], surfaces: [], daily: [] };
        case 'gateway_service_status': return { supported: true, installed: false, running: false };
        case 'gateway_diagnostics': return { checks: [], passed: 0, warnings: 0, errors: 0 };
        case 'gateway_cache_health': return { sessions: [], episodes: [], policy: { enabled: false } };
        case 'codex_plugin_status': return { installed: true, enabled: true, mcpHealthy: true };
        case 'register_codetas_provider': {
          const existing = window.__config.providers.find((p) => p.id === args.providerId);
          if (existing) existing.enabled = true;
          else window.__config.providers.push(makeProvider(args.providerId));
          return { providerId: args.providerId, credentialNeeded: true, message: 'API参照を設定してください' };
        }
        case 'install_provider_preset': window.__config.providers.push(makeProvider(args.input.presetId)); return status;
        case 'upsert_gateway_provider': {
          if (window.__providerSaveDelay) {
            await new Promise((resolve) => setTimeout(resolve, window.__providerSaveDelay));
          }
          const index = window.__config.providers.findIndex((p) => p.id === args.input.provider.id);
          assertNeverMissing(index);
          window.__config.providers[index] = structuredClone(args.input.provider);
          return status;
        }
        case 'sync_codex_model_catalog': return '/mock/catalog.json';
        default: throw new Error('Unexpected command: ' + command);
      }
    } };
    function assertNeverMissing(index) { if (index < 0) throw new Error('Unknown provider'); }
  }, fixture);
  await page.goto(process.env.UI_URL ?? 'http://127.0.0.1:4193/');
  await page.locator('[data-view="settings"]').click();
  await page.getByRole('heading', { name: '直接 API 接続', exact: true }).waitFor();

  const originalLogins = await page.evaluate(() => window.__config.providers
    .filter((p) => ['anthropic', 'openai'].includes(p.id)));
  for (const [id, env, transport] of [
    ['anthropic-apikey', 'ANTHROPIC_API_KEY', 'xApiKey'],
    ['openai-api', 'OPENAI_API_KEY', 'bearer'],
  ]) {
    await page.locator(`[data-action="register-direct-api"][data-provider-id="${id}"]`).click();
    const form = page.locator('#provider-editor-form');
    await form.waitFor();
    assert.equal(await form.locator('[name="id"]').inputValue(), id);
    assert.equal(await form.locator('[name="credentialSource"]').inputValue(), 'environment');
    assert.equal(await form.locator('[name="credentialReference"]').inputValue(), env);
    assert.equal(await form.locator('[name="credentialTransport"]').inputValue(), transport);
    assert.equal(await form.locator('[name="credentialReference"]').isVisible(), true);
    assert.equal(await form.locator('.drawer-advanced').getAttribute('open'), null);
    await form.locator('[name="credentialSource"]').selectOption('keychain');
    await form.locator('[name="credentialReference"]').fill(`codetas-test/${id}`);
    await form.locator('[name="credentialReference"]').press('Tab');
    try {
      await page.waitForFunction(async (providerId) => {
        const config = (await import('/src/state.ts')).state.configuration;
        return config.providers.some((p) => p.id === providerId && p.credential.source === 'keychain'
          && p.credential.reference === `codetas-test/${providerId}`);
      }, id, { timeout: 5000 });
    } catch (error) {
      console.error(await page.evaluate(async () => ({ calls: window.__calls,
        form: Object.fromEntries(new FormData(document.querySelector('#provider-editor-form'))),
        appState: (await import('/src/state.ts')).state.editingProviderId,
        notices: [...document.querySelectorAll('.notice')].map((n) => n.textContent),
        invalid: [...document.querySelectorAll('#provider-editor-form :invalid')].map((n) => ({ name: n.name, value: n.value, error: n.validationMessage })) })));
      throw error;
    }
    await page.locator('.provider-drawer > header [data-action="close-provider-editor"]').click();
    await page.locator(`.direct-api-panel [data-action="edit-provider"][data-provider-id="${id}"]`).click();
    assert.equal(await page.locator('[name="credentialReference"]').inputValue(), `codetas-test/${id}`);
    await page.locator('.provider-drawer > header [data-action="close-provider-editor"]').click();
  }
  assert.deepEqual(await page.evaluate(() => window.__config.providers
    .filter((p) => ['anthropic', 'openai'].includes(p.id))), originalLogins);

  // Queue a second edit while the first save is in flight, then switch drawers.
  // The delayed save must retain the original provider's form snapshot.
  await page.evaluate(() => { window.__providerSaveDelay = 800; });
  await page.locator('.direct-api-panel [data-action="edit-provider"][data-provider-id="anthropic-apikey"]').click();
  await page.evaluate(async () => {
    const form = document.querySelector('#provider-editor-form');
    form.querySelector('[name="credentialReference"]').value = 'codetas-test/claude-first';
    const { saveProviderForm } = await import('/src/actions.ts');
    window.__firstSave = saveProviderForm(new FormData(form));
  });
  await page.waitForFunction(() => window.__calls.some((call) =>
    call.command === 'upsert_gateway_provider'
    && call.args.input.provider.credential.reference === 'codetas-test/claude-first'), undefined, { timeout: 5000 });
  await page.evaluate(async () => {
    const form = document.querySelector('#provider-editor-form');
    form.querySelector('[name="credentialReference"]').value = 'codetas-test/claude-last';
    const { saveProviderForm } = await import('/src/actions.ts');
    window.__lastSave = saveProviderForm(new FormData(form));
  });
  await page.locator('.provider-drawer > header [data-action="close-provider-editor"]').click();
  await page.locator('.direct-api-panel [data-action="edit-provider"][data-provider-id="openai-api"]').click();
  await page.evaluate(() => Promise.all([window.__firstSave, window.__lastSave]));
  await page.waitForFunction(() => window.__config.providers.some((provider) =>
    provider.id === 'anthropic-apikey'
    && provider.credential.reference === 'codetas-test/claude-last'), undefined, { timeout: 5000 });
  assert.equal(await page.locator('#provider-editor-form [name="id"]').inputValue(), 'openai-api');
  assert.equal(await page.locator('#provider-editor-form [name="credentialReference"]').inputValue(), 'codetas-test/openai-api');
  await page.locator('.provider-drawer > header [data-action="close-provider-editor"]').click();
  await page.evaluate(() => { window.__providerSaveDelay = 0; });

  await page.locator('[data-view="connections"], [data-view="providers"]').first().click();
  await page.locator('[name="presetId"]').selectOption('openai-apikey');
  await page.locator('#preset-form button[type="submit"]').click();
  await page.locator('#provider-editor-form').waitFor();
  assert.equal(await page.locator('#provider-editor-form [name="id"]').inputValue(), 'openai-apikey');
  await page.locator('.provider-drawer > header [data-action="close-provider-editor"]').click();

  await page.locator('[data-view="settings"]').click();
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(await page.locator('.direct-api-panel').evaluate((panel) => panel.scrollWidth <= panel.clientWidth + 2));
  const calls = await page.evaluate(() => window.__calls.map((c) => c.command));
  assert.ok(!calls.some((command) => /login|launch_provider_oauth|test_gateway_provider/.test(command)));
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ checks: ['settings-api-targets', 'registration', 'visible-auth', 'correct-env-and-transport', 'keychain-save-and-edit', 'subscription-preserved', 'queued-save-provider-isolation', 'preset-opens-editor', 'mobile-layout', 'no-provider-traffic'], errors }));
} finally {
  await browser.close();
}
