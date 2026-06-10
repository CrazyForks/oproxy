// @ts-check
const { test, expect } = require('@playwright/test');
const { gotoRail } = require('./helpers');

const fixtureE2E = process.env.OPROXY_FIXTURE_E2E === '1';
const fixtureWsUrl = process.env.OPROXY_FIXTURE_WS_URL || 'ws://127.0.0.1:18081/socket';
const fixtureGrpcUrl = process.env.OPROXY_FIXTURE_GRPC_URL || 'https://127.0.0.1:19090/echo.EchoService/UnaryEcho';

async function waitForSession(request, predicate) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    const list = await (await request.get('/api/sessions?limit=100&include_bodies=true')).json();
    for (const session of list.sessions || []) {
      const detail = await (await request.get(`/api/sessions/${session.id}`)).json();
      const exchange = detail.exchange || session;
      if (predicate(exchange)) return exchange;
    }
    await new Promise(resolve => setTimeout(resolve, 250));
  }
  throw new Error('matching session was not recorded');
}

test.describe('Compose', () => {
  test.beforeEach(async ({ page }) => {
    await gotoRail(page, 'Compose');
  });

  test('compose view renders with empty state and new tab button', async ({ page }) => {
    await expect(page.locator('.cmp-tab-new')).toBeVisible();
    await expect(page.getByText('No request open.')).toBeVisible();
  });

  test('clicking + creates new request tab and shows editor', async ({ page }) => {
    await page.locator('.cmp-tab-new').click();
    await expect(page.locator('.cmp-editor')).toBeVisible();
    await expect(page.locator('.cmp-method')).toBeVisible();
    await expect(page.locator('.cmp-url')).toBeVisible();
    await expect(page.getByRole('button', { name: /Send/ })).toBeVisible();
  });

  test('can type URL in compose editor', async ({ page }) => {
    await page.locator('.cmp-tab-new').click();
    await page.locator('.cmp-url').fill('https://httpbin.org/get');
    await expect(page.locator('.cmp-url')).toHaveValue('https://httpbin.org/get');
  });

  test('pasting cURL into the URL field imports request fields', async ({ page }) => {
    await page.locator('.cmp-tab-new').click();

    const curl = `curl -X POST https://api.example.com/users -H 'content-type: application/json' -H 'x-token: abc' --data '{"name":"Ada"}'`;
    await page.locator('.cmp-url').evaluate((input, text) => {
      const event = new Event('paste', { bubbles: true, cancelable: true });
      Object.defineProperty(event, 'clipboardData', {
        value: { getData: type => type === 'text/plain' ? text : '' },
      });
      input.dispatchEvent(event);
    }, curl);

    await expect(page.locator('.cmp-method')).toHaveValue('POST');
    await expect(page.locator('.cmp-url')).toHaveValue('https://api.example.com/users');
    await expect(page.locator('.cmp-body-tabs .tab', { hasText: 'Body' })).toHaveClass(/on/);
    await expect(page.locator('.cmp-body-ta')).toHaveValue('{"name":"Ada"}');

    await page.locator('.cmp-body-tabs .tab', { hasText: 'Headers' }).click();
    await expect.poll(async () => {
      return page.locator('.kvedit-row').evaluateAll(rows => rows.map(row => {
        const [key, value] = Array.from(row.querySelectorAll('input'));
        return [key?.value, value?.value];
      }));
    }).toEqual(expect.arrayContaining([
      ['content-type', 'application/json'],
      ['x-token', 'abc'],
    ]));
  });

  test('New Request button in empty state creates tab', async ({ page }) => {
    await page.getByRole('button', { name: '+ New request' }).click();
    await expect(page.locator('.cmp-method')).toBeVisible();
  });

  test('collections sidebar can create a collection', async ({ page }) => {
    await page.getByRole('button', { name: /Collection/ }).click();
    await expect(page.getByText('Collection 1')).toBeVisible();
  });

  test('vars panel can add a variable row', async ({ page }) => {
    await page.getByTitle('New variable').click();
    await expect(page.locator('.cmp-var')).toHaveCount(1);
    await expect(page.getByText('var_1')).toBeVisible();
  });

  test('collections and variables persist across reloads', async ({ page }) => {
    await page.getByRole('button', { name: /Collection/ }).click();
    await page.getByTitle('New variable').click();

    await page.locator('.cmp-tab-new').click();
    await page.locator('.cmp-url').fill('https://persist.example.com/api');
    // Save opens the collection picker bar for unsaved requests
    await page.getByRole('button', { name: 'Save' }).click();
    await expect(page.locator('.cmp-save-bar')).toBeVisible();
    await page.locator('.cmp-save-bar').getByRole('button', { name: 'Save' }).click();
    await expect(page.locator('.cmp-req-name', { hasText: 'Untitled' })).toBeVisible();

    await page.reload();
    await page.getByRole('button', { name: 'Compose', exact: true }).click();
    await expect(page.getByRole('heading', { name: 'Compose' })).toBeVisible();
    await expect(page.getByText('Collection 1')).toBeVisible();
    await expect(page.locator('.cmp-req-name', { hasText: 'Untitled' })).toBeVisible();
    await expect(page.getByText('var_1')).toBeVisible();
  });

  test('response headers and timing tabs render without crashing', async ({ page }) => {
    const errors = [];
    page.on('pageerror', err => errors.push(String(err)));

    await page.route('/admin/forward', async route => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          status: 200,
          statusText: 'OK',
          body: '{"ok":true}',
          headers: {
            'content-type': 'application/json',
            'x-test': 'yes',
          },
        }),
      });
    });

    await page.locator('.cmp-tab-new').click();
    await page.locator('.cmp-url').fill('https://example.com/api');
    await page.getByRole('button', { name: /Send/ }).click();

    await page.getByRole('button', { name: 'headers', exact: true }).click();
    await expect(page.getByText('content-type')).toBeVisible();
    await expect(page.getByText('application/json')).toBeVisible();

    await page.getByRole('button', { name: 'timing', exact: true }).click();
    await expect(page.locator('.cmp-response .kv .k', { hasText: 'Request' })).toBeVisible();
    await expect(page.locator('.cmp-response .kv .k', { hasText: 'Total' })).toBeVisible();

    expect(errors).toEqual([]);
  });

  test('websocket mode sends scripted frames and renders frame results', async ({ page }) => {
    let payload;
    await page.route('/admin/forward/websocket', async route => {
      payload = route.request().postDataJSON();
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          status: 101,
          status_text: 'Switching Protocols',
          session_id: 'ws-compose-session',
          frames: [
            { direction: 'client', opcode: 'text', payload: payload.frames[0].payload, payload_len: payload.frames[0].payload.length },
            { direction: 'server', opcode: 'text', payload: payload.frames[0].payload, payload_len: payload.frames[0].payload.length },
          ],
          protocol: {
            downstream: 'WebSocket',
            upstream: 'WebSocket',
            application: 'http',
            body_mode: 'frames',
          },
        }),
      });
    });

    await page.locator('.cmp-tab-new').click();
    await page.getByRole('button', { name: 'WS' }).click();
    await page.locator('.cmp-url').fill('ws://127.0.0.1:18081/socket');
    await page.getByLabel('WebSocket frame payload').fill('compose-ws');
    await page.getByRole('button', { name: /Send/ }).click();

    await expect(page.locator('.cmp-response')).toContainText('Switching Protocols');
    await expect(page.locator('.cmp-response')).toContainText('server · text');
    await expect(page.locator('.cmp-response')).toContainText('compose-ws');
    expect(payload).toMatchObject({
      url: 'ws://127.0.0.1:18081/socket',
      frames: [{ opcode: 'text', payload: 'compose-ws' }],
    });
  });

  test('grpc mode sends unary payload through protocol-aware forward', async ({ page }) => {
    let payload;
    await page.route('/admin/forward', async route => {
      payload = route.request().postDataJSON();
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          status: 200,
          statusText: 'OK',
          body: 'grpc-ok',
          headers: { 'content-type': 'application/grpc+proto' },
          protocol: {
            downstream: 'HTTP/2',
            upstream: 'HTTP/2',
            application: 'grpc',
            body_mode: 'stream_messages',
          },
        }),
      });
    });

    await page.locator('.cmp-tab-new').click();
    await page.getByRole('button', { name: 'gRPC' }).click();
    await page.locator('.cmp-url').fill('https://127.0.0.1:19090/echo.EchoService/UnaryEcho');
    await page.locator('.cmp-body-ta').fill('hello-grpc');
    await page.getByRole('button', { name: /Send/ }).click();

    await expect(page.locator('.cmp-response')).toContainText('200 OK');
    await page.getByRole('button', { name: 'timing', exact: true }).click();
    await expect(page.locator('.cmp-response')).toContainText('HTTP/2');
    await expect(page.locator('.cmp-response')).toContainText('grpc');
    expect(payload).toMatchObject({
      kind: 'grpc',
      method: 'POST',
      url: 'https://127.0.0.1:19090/echo.EchoService/UnaryEcho',
      body: 'hello-grpc',
    });
    expect(payload.headers['content-type']).toBe('application/grpc+proto');
  });

  test('fixture websocket mode records real echoed frames', async ({ page, request }) => {
    test.skip(!fixtureE2E, 'Set OPROXY_FIXTURE_E2E=1 and start docker compose --profile fixtures.');
    await request.delete('/admin/sessions');

    await page.locator('.cmp-tab-new').click();
    await page.getByRole('button', { name: 'WS' }).click();
    await page.locator('.cmp-url').fill(fixtureWsUrl);
    await page.getByLabel('WebSocket frame payload').fill('fixture-compose-ws');
    await page.getByRole('button', { name: /Send/ }).click();

    await expect(page.locator('.cmp-response')).toContainText('Switching Protocols');
    await expect(page.locator('.cmp-response')).toContainText('server · text');
    await expect(page.locator('.cmp-response')).toContainText('fixture-compose-ws');

    const session = await waitForSession(request, exchange => {
      const events = exchange.events || [];
      return exchange.request?.uri === fixtureWsUrl
        && exchange.protocol_context?.body_mode === 'frames'
        && events.filter(event => event.type === 'ws_frame').length >= 2;
    });
    expect(session.protocol_context?.downstream).toBe('web_socket');
  });

  test('fixture grpc mode records real unary message events', async ({ page, request }) => {
    test.skip(!fixtureE2E, 'Set OPROXY_FIXTURE_E2E=1 and start docker compose --profile fixtures.');
    await request.delete('/admin/sessions');

    const protobufEchoRequest = '\x0a\x0cfixture-grpc';
    await page.locator('.cmp-tab-new').click();
    await page.getByRole('button', { name: 'gRPC' }).click();
    await page.locator('.cmp-url').fill(fixtureGrpcUrl);
    await page.locator('.cmp-body-ta').evaluate((textarea, value) => {
      textarea.value = value;
      textarea.dispatchEvent(new Event('input', { bubbles: true }));
    }, protobufEchoRequest);
    await page.getByRole('button', { name: /Send/ }).click();

    await expect(page.locator('.cmp-response')).toContainText('200 OK');
    await page.getByRole('button', { name: 'timing', exact: true }).click();
    await expect(page.locator('.cmp-response')).toContainText('grpc');
    await expect(page.locator('.cmp-response')).toContainText('stream_messages');

    const session = await waitForSession(request, exchange => {
      const events = exchange.events || [];
      return exchange.request?.uri === fixtureGrpcUrl
        && exchange.protocol_context?.application === 'grpc'
        && events.filter(event => event.type === 'grpc_message').length >= 2;
    });
    expect(session.protocol_context?.body_mode).toBe('stream_messages');
  });
});
