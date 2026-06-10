// @ts-check
const { test, expect } = require('@playwright/test');

const mockRule = () => ({
  id: '',
  name: 'test-mock',
  enabled: true,
  location: {
    host: null,
    path: '.*',
    port: null,
    protocol: null,
    query: null,
    methods: [],
    mode: 'regex',
  },
  responses: [{ status: 200, headers: {}, body: '{"ok":true}', delay_ms: 0 }],
});

async function clearMockRules(request) {
  const rules = await (await request.get('/admin/mock/rules')).json();
  for (const rule of rules) {
    await request.delete(`/admin/mock/rules/${encodeURIComponent(rule.id)}`);
  }
}

test.describe('Mock rules', () => {
  test.beforeEach(async ({ page, request }) => {
    await clearMockRules(request);
    await page.goto('/');
    await page.getByRole('button', { name: 'Mock Server', exact: true }).click();
    await expect(page.getByRole('heading', { name: 'Mock Server', exact: true })).toBeVisible();
  });

  test.afterEach(async ({ request }) => {
    await clearMockRules(request);
  });

  test('mock view renders', async ({ page }) => {
    await expect(page.getByRole('button', { name: /Add mock/ })).toBeVisible();
  });

  test('GET /admin/mock/rules returns array', async ({ request }) => {
    const res = await request.get('/admin/mock/rules');
    expect(res.ok()).toBeTruthy();
    const body = await res.json();
    expect(Array.isArray(body)).toBeTruthy();
  });

  test('create mock rule via API, GET returns it', async ({ request }) => {
    const res = await request.post('/admin/mock/rules', { data: mockRule() });
    expect(res.ok()).toBeTruthy();
    // List and find the created rule
    const list = await (await request.get('/admin/mock/rules')).json();
    const created = list.find(r => r.name === 'test-mock');
    expect(created).toBeTruthy();
    // Cleanup
    await request.delete(`/admin/mock/rules/${created.id}`);
  });

  test('delete mock rule removes it', async ({ request }) => {
    await request.post('/admin/mock/rules', { data: { ...mockRule(), name: 'del-mock' } });
    const before = await (await request.get('/admin/mock/rules')).json();
    const created = before.find(r => r.name === 'del-mock');
    expect(created).toBeTruthy();
    await request.delete(`/admin/mock/rules/${created.id}`);
    const after = await (await request.get('/admin/mock/rules')).json();
    expect(after.some(r => r.id === created.id)).toBeFalsy();
  });

  test('typed protocol mock behaviors persist and render in the UI', async ({ page, request }) => {
    const typedRules = [
      {
        id: '',
        name: 'ws-script-mock',
        enabled: true,
        location: {
          host: 'ws.mock.example',
          path: '/socket',
          port: null,
          protocol: null,
          query: null,
          methods: [],
          mode: 'glob',
          wire_protocol: 'websocket',
          application_protocol: null,
          body_mode: 'frames',
        },
        behavior: {
          type: 'web_socket_script',
          frames: [{ opcode: 1, payload: 'mock-ws', delay_ms: 0 }],
        },
        responses: [],
        call_count: 0,
      },
      {
        id: '',
        name: 'grpc-script-mock',
        enabled: true,
        location: {
          host: 'grpc.mock.example',
          path: '/pkg.Service/Unary',
          port: null,
          protocol: null,
          query: null,
          methods: [],
          mode: 'glob',
          wire_protocol: null,
          application_protocol: 'grpc',
          body_mode: 'stream_messages',
        },
        behavior: {
          type: 'grpc_script',
          messages: [{ compressed: false, payload_base64: 'Z3JwYy1tb2Nr', delay_ms: 0 }],
          trailers: {},
        },
        responses: [],
        call_count: 0,
      },
      {
        id: '',
        name: 'tunnel-deny-mock',
        enabled: true,
        location: {
          host: 'tunnel.mock.example',
          path: null,
          port: null,
          protocol: null,
          query: null,
          methods: [],
          mode: 'glob',
          wire_protocol: 'socks5',
          application_protocol: null,
          body_mode: 'tunnel',
        },
        behavior: {
          type: 'tunnel_decision',
          decision: { allow: false, delay_ms: 25 },
        },
        responses: [],
        call_count: 0,
      },
    ];

    for (const rule of typedRules) {
      const res = await request.post('/admin/mock/rules', { data: rule });
      expect(res.ok()).toBeTruthy();
    }

    await page.reload();
    await page.getByRole('button', { name: 'Mock Server', exact: true }).click();
    await expect(page.getByRole('heading', { name: 'Mock Server', exact: true })).toBeVisible();
    for (const name of typedRules.map(r => r.name)) {
      await expect(page.locator('.rule-row', { hasText: name })).toBeVisible();
    }

    await expect(page.locator('.rule-row', { hasText: 'ws-script-mock' })).toContainText('1 frame');
    await expect(page.locator('.rule-row', { hasText: 'ws-script-mock' })).toContainText('web socket script');
    await expect(page.locator('.rule-row', { hasText: 'grpc-script-mock' })).toContainText('1 message');
    await expect(page.locator('.rule-row', { hasText: 'grpc-script-mock' })).toContainText('grpc script');
    await expect(page.locator('.rule-row', { hasText: 'tunnel-deny-mock' })).toContainText('deny tunnel');

    await page.locator('.rule-row', { hasText: 'ws-script-mock' }).getByRole('button', { name: /Show mock responses/ }).click();
    await expect(page.getByText('mock-ws')).toBeVisible();
    await page.locator('.rule-row', { hasText: 'grpc-script-mock' }).getByRole('button', { name: /Show mock responses/ }).click();
    await expect(page.getByText('grpc-mock')).toBeVisible();
    await page.locator('.rule-row', { hasText: 'tunnel-deny-mock' }).getByRole('button', { name: /Show mock responses/ }).click();
    await expect(page.getByText('deny · +25 ms')).toBeVisible();

    const list = await (await request.get('/admin/mock/rules')).json();
    expect(list.map(r => r.behavior?.type).sort()).toEqual(['grpc_script', 'tunnel_decision', 'web_socket_script']);
  });
});
