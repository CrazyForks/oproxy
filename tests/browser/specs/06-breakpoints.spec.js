// @ts-check
const { test, expect } = require('@playwright/test');
const http = require('http');
const { gotoRail } = require('./helpers');

const location = (overrides = {}) => ({
  host: null,
  path: null,
  port: null,
  protocol: null,
  query: null,
  methods: [],
  mode: 'glob',
  ...overrides,
});

function proxyGet(baseURL, targetUrl) {
  const proxy = new URL(baseURL);
  const target = new URL(targetUrl);
  return new Promise((resolve, reject) => {
    const req = http.request({
      host: proxy.hostname,
      port: proxy.port || 80,
      method: 'GET',
      path: targetUrl,
      headers: { Host: target.host },
    }, res => {
      res.resume();
      res.on('end', () => resolve(res.statusCode));
    });
    req.on('error', reject);
    req.end();
  });
}

async function withServer(run) {
  const server = http.createServer((_req, res) => res.end('ok'));
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    await run(server.address().port);
  } finally {
    server.close();
  }
}

test.describe('Breakpoints', () => {
  test.beforeEach(async ({ request }) => {
    const rules = await (await request.get('/admin/breakpoints')).json();
    for (const rule of rules) await request.delete(`/admin/breakpoints/${rule.id}`);
  });

  test.afterEach(async ({ request }) => {
    const rules = await (await request.get('/admin/breakpoints')).json();
    for (const rule of rules) await request.delete(`/admin/breakpoints/${rule.id}`);
  });

  test('breakpoints view loads and opens add dialog', async ({ page }) => {
    await gotoRail(page, 'Breakpoints');
    await expect(page.getByText('No requests are paused.')).toBeVisible();
    await page.getByRole('button', { name: /Add breakpoint/ }).click();
    const dialog = page.locator('.ui-dialog');
    await expect(page.getByRole('heading', { name: 'Add breakpoint' })).toBeVisible();
    await expect(dialog.getByText('Pause', { exact: true })).toBeVisible();
    await expect(dialog.getByText('matching', { exact: true })).toBeVisible();
    await expect(dialog.getByText('Methods', { exact: true })).toBeVisible();
  });

  test('adds breakpoint rule through UI', async ({ page, request }) => {
    await gotoRail(page, 'Breakpoints');
    await page.getByRole('button', { name: /Add breakpoint/ }).click();
    await page.getByPlaceholder('example.com').fill('ui-break.example.com');
    await page.getByRole('button', { name: 'Save' }).click();
    await expect(page.getByText('ui-break.example.com')).toBeVisible();

    const rules = await (await request.get('/admin/breakpoints')).json();
    expect(rules.some(r => r.location?.host === 'ui-break.example.com')).toBeTruthy();
  });

  test('delete breakpoint rule removes it', async ({ page, request }) => {
    await request.post('/admin/breakpoints', {
      data: { id: '', location: location({ host: 'del-break.example.com' }), bp_type: 'Request', enabled: true },
    });
    await gotoRail(page, 'Breakpoints');
    await expect(page.getByText('del-break.example.com')).toBeVisible();

    await page.locator('.rule-row', { hasText: 'del-break.example.com' }).getByText('×').click();
    await page.getByRole('button', { name: 'Delete', exact: true }).click();
    await expect(page.getByText('del-break.example.com')).toHaveCount(0);
  });

  test('breakpoint row toggle persists enabled state', async ({ page, request }) => {
    await request.post('/admin/breakpoints', {
      data: { id: '', location: location({ host: 'toggle-break.example.com' }), bp_type: 'Request', enabled: true },
    });
    await gotoRail(page, 'Breakpoints');
    const toggle = page.getByLabel('Toggle rule toggle-break.example.com');
    await expect(toggle).toHaveAttribute('aria-pressed', 'true');

    await toggle.click();
    await expect(toggle).toHaveAttribute('aria-pressed', 'false');
    const rules = await (await request.get('/admin/breakpoints')).json();
    const rule = rules.find(r => r.location?.host === 'toggle-break.example.com');
    expect(rule).toBeTruthy();
    expect(rule.enabled).toBe(false);
  });

  test('protocol breakpoint tiers persist and render in the UI', async ({ page, request }) => {
    await request.post('/admin/breakpoints', {
      data: {
        id: '',
        location: location({
          host: 'socket-break.example.com',
          path: '/ws',
          wire_protocol: 'websocket',
          body_mode: 'frames',
        }),
        bp_type: 'Request',
        tier: 'frame',
        enabled: true,
      },
    });
    await request.post('/admin/breakpoints', {
      data: {
        id: '',
        location: location({
          host: 'tunnel-break.example.com',
          wire_protocol: 'socks5',
          body_mode: 'tunnel',
        }),
        bp_type: 'Request',
        tier: 'tunnel',
        enabled: true,
      },
    });

    await gotoRail(page, 'Breakpoints');
    const frameRow = page.locator('.rule-row', { hasText: 'socket-break.example.com' });
    await expect(frameRow).toContainText('/ws');
    await expect(frameRow).toContainText('frame · glob');

    const tunnelRow = page.locator('.rule-row', { hasText: 'tunnel-break.example.com' });
    await expect(tunnelRow).toContainText('tunnel · glob');

    const rules = await (await request.get('/admin/breakpoints')).json();
    expect(rules.find(r => r.location?.host === 'socket-break.example.com')?.tier).toBe('frame');
    expect(rules.find(r => r.location?.host === 'tunnel-break.example.com')?.tier).toBe('tunnel');
  });

  test('frame breakpoint tier applies WebSocket matcher fields without a WS method', async ({ page, request }) => {
    await gotoRail(page, 'Breakpoints');
    await page.getByRole('button', { name: /Add breakpoint/ }).click();
    const dialog = page.locator('.ui-dialog');
    await dialog.locator('select').nth(1).selectOption('frame');
    await dialog.getByPlaceholder('api.example.com').fill('ui-frame-break.example.com');
    await page.getByRole('button', { name: 'Save', exact: true }).click();

    const rules = await (await request.get('/admin/breakpoints')).json();
    const saved = rules.find(r => r.location?.host === 'ui-frame-break.example.com');
    expect(saved).toBeTruthy();
    expect(saved.tier).toBe('frame');
    expect(saved.location).toMatchObject({
      wire_protocol: 'websocket',
      body_mode: 'frames',
    });
    expect(saved.location.methods || []).not.toContain('WS');
  });

  test('Disable all turns off rules and releases held requests', async ({ page, request, baseURL }) => {
    await withServer(async port => {
      await request.post('/admin/breakpoints', {
        data: { id: '', location: location({ path: '/held-bp' }), bp_type: 'Request', enabled: true },
      });
      await gotoRail(page, 'Breakpoints');

      // Ensure no stale pending entries interfere with this case.
      const pendingBefore = await (await request.get('/admin/breakpoints/pending')).json();
      for (const p of pendingBefore) {
        await request.post(`/admin/breakpoints/pending/${encodeURIComponent(p.id)}/resolve`, {
          data: { action: 'continue' },
        });
      }

      // Trigger one proxied request that matches the rule so it enters the held queue.
      const proxied = proxyGet(baseURL, `http://127.0.0.1:${port}/held-bp`);
      await expect.poll(async () => {
        const pending = await (await request.get('/admin/breakpoints/pending')).json();
        return pending.length;
      }).toBeGreaterThan(0);

      await page.getByRole('button', { name: 'Disable all' }).click();
      await expect.poll(async () => {
        const pending = await (await request.get('/admin/breakpoints/pending')).json();
        return pending.length;
      }).toBe(0);

      const rules = await (await request.get('/admin/breakpoints')).json();
      expect(rules.find(r => r.location?.path === '/held-bp')?.enabled).toBeFalsy();
      await expect(proxied).resolves.toBe(200);
    });
  });
});
