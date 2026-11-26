import assert from 'node:assert/strict';
import test from 'node:test';
import http from 'node:http';
import { registerHooks } from 'node:module';

// Same HTTP transport selection as the hosted build. The signing boundary is
// substituted; batching, URL construction, fetch and body cancellation are real.
registerHooks({ load(url, context, nextLoad) {
  const loaded = nextLoad(url, context);
  if (url.endsWith('/shared/lib/relay-url.ts') || url.endsWith('/shared/api/relayClientSession.ts'))
    return { ...loaded, source: String(loaded.source).replaceAll('import.meta.env', '({VITE_RELAY_TRANSPORT:"http"})') };
  return loaded;
} });
const page = new EventTarget();
page.setTimeout = setTimeout; page.clearTimeout = clearTimeout;
page.__TAURI_INTERNALS__ = { invoke: async (command, args) => {
  assert.equal(command, 'sign_event');
  return JSON.stringify({ id: 'auth', ...args });
} };
globalThis.window = page;
const { RelayClient } = await import('./relayClientSession.ts');
const { documentQuerySignal, isDocumentQueryDeparture } = await import('./documentQueryLifetime.ts');

test('shared HTTP readers cancel on departure, reject partial bodies, and resume with a fresh lifetime', { timeout: 10000 }, async () => {
  let mode = 'partial', arrived;
  const requests = [];
  const event = { id: '1'.repeat(64), kind: 7, pubkey: '2'.repeat(64), created_at: 1, content: '', tags: [], sig: '' };
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', chunk => { body += chunk; });
    req.on('end', () => {
      requests.push({ path: req.url, method: req.method, filters: JSON.parse(body) });
      if (mode === 'partial') { res.writeHead(200, { 'content-type': 'application/json' }); res.write('['); }
      else if (mode === 'failure') { res.writeHead(503); res.end('{}'); }
      else { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify([event])); }
      arrived?.();
    });
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  page.location = { origin: `http://127.0.0.1:${server.address().port}` };
  try {
    const client = new RelayClient();
    const started = new Promise(resolve => { arrived = resolve; });
    const a = client.fetchEvents({ kinds: [7] }), b = client.fetchEvents({ kinds: [9] });
    const settled = Promise.allSettled([a, b]);
    await started;
    assert.equal(requests.length, 1, 'sibling readers must still share one HTTP request');
    assert.deepEqual(requests[0], { path: '/buzz/api/filter-query', method: 'POST', filters: [{ kinds: [7] }, { kinds: [9] }] });
    const oldSignal = documentQuerySignal();
    page.dispatchEvent(new Event('pagehide'));
    const results = await settled;
    for (const result of results) {
      assert.equal(result.status, 'rejected');
      assert.equal(result.reason, oldSignal.reason);
      assert.equal(isDocumentQueryDeparture(result.reason), true);
    }
    await assert.rejects(client.fetchEvents({ kinds: [7] }), e => e === oldSignal.reason);
    assert.equal(requests.length, 1, 'a departed document cannot start another query');

    mode = 'success';
    const restored = new Event('pageshow');
    Object.defineProperty(restored, 'persisted', { value: true });
    page.dispatchEvent(restored);
    assert.equal(oldSignal.aborted, true, 'restoring must not revive the old request');
    assert.notEqual(documentQuerySignal(), oldSignal);
    assert.equal(documentQuerySignal().aborted, false);
    assert.deepEqual(await client.fetchEvents({ kinds: [7] }), [event]);
    mode = 'failure';
    await assert.rejects(client.fetchEvents({ kinds: [7] }), e => e.message === 'HTTP 503' && !isDocumentQueryDeparture(e));
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  }
});
