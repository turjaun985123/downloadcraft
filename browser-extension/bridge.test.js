// The app link: request context capture (headers, POST bodies, IP, proxy)
// and the persistent port's request/reply matching, with browser APIs stubbed.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(__dirname + '/bridge.js', 'utf8');

function load(overrides = {}) {
  const listeners = {};
  const posted = [];
  let portMsg, portGone;
  const ev = (name) => ({ addListener: (f) => { listeners[name] = f; } });
  const chrome = {
    runtime: {
      lastError: null,
      connectNative: overrides.connectNative || (() => ({
        onMessage: { addListener: (f) => { portMsg = f; } },
        onDisconnect: { addListener: (f) => { portGone = f; } },
        postMessage: (m) => posted.push(m),
      })),
      sendNativeMessage: overrides.sendNativeMessage || (async (h, m) => ({ type: 'ok', via: 'oneshot', m })),
    },
    webRequest: {
      onBeforeRequest: ev('before'), onSendHeaders: ev('headers'),
      onBeforeRedirect: ev('redirect'), onResponseStarted: ev('started'),
      OnSendHeadersOptions: { EXTRA_HEADERS: 'extraHeaders' },
    },
    proxy: { settings: { get: async () => ({ value: { mode: 'fixed_servers', rules: { singleProxy: { scheme: 'socks5', host: '10.0.0.2', port: 1080 } } } }), onChange: ev('proxy') } },
  };
  const ctx = { chrome, console, setTimeout, clearTimeout, TextEncoder, URL, btoa: (s) => Buffer.from(s, 'binary').toString('base64'), Date, Map, Set, Promise, Object, String, Array, Number, JSON, Uint8Array };
  vm.createContext(ctx);
  vm.runInContext(source + '\nthis.DCBridge = DCBridge;', ctx);
  ctx.DCBridge.start('com.download_craft');
  return { B: ctx.DCBridge, listeners, posted, port: () => portMsg, gone: () => portGone };
}

const dec = (b64) => Buffer.from(b64, 'base64').toString('utf8');

test('a form POST is rebuilt with its headers, IP and the browser proxy', async () => {
  const { B, listeners } = load();
  await new Promise((r) => setTimeout(r, 5));
  listeners.before({ requestId: '1', url: 'https://bank.example/export', method: 'POST',
    requestBody: { formData: { from: ['2026-01-01'], 'q x': ['a&b'] } } });
  listeners.headers({ requestId: '1', requestHeaders: [
    { name: 'User-Agent', value: 'UA/1' }, { name: 'Content-Type', value: 'application/x-www-form-urlencoded' },
    { name: 'Cookie', value: 'sid=1' }, { name: 'Referer', value: 'https://bank.example/' }, { name: 'Range', value: 'bytes=0-' }] });
  listeners.started({ requestId: '1', url: 'https://bank.example/export', ip: '198.51.100.7' });
  const c = B.browserContext('https://bank.example/export');
  assert.equal(c.method, 'POST');
  assert.equal(dec(c.body), 'from=2026-01-01&q+x=a%26b');
  assert.deepEqual([...Object.keys(c.headers)].sort(), ['content-type', 'user-agent']);
  assert.equal(c.serverIp, '198.51.100.7');
  assert.equal(c.proxy, 'socks5://10.0.0.2:1080');
  assert.equal(c._cookie, 'sid=1');
  assert.equal(c._referer, 'https://bank.example/');
  assert.equal(c.fromExtension, true);
});

test('multipart bodies keep their boundary; uploaded files are not repeatable', () => {
  const { B, listeners } = load();
  listeners.before({ requestId: '2', url: 'https://s.example/r', method: 'POST', requestBody: { formData: { a: ['1'] } } });
  listeners.headers({ requestId: '2', requestHeaders: [{ name: 'Content-Type', value: 'multipart/form-data; boundary=XyZ' }] });
  const c = B.browserContext('https://s.example/r');
  assert.equal(dec(c.body), '--XyZ\r\nContent-Disposition: form-data; name="a"\r\n\r\n1\r\n--XyZ--\r\n');
  listeners.before({ requestId: '3', url: 'https://s.example/up', method: 'POST', requestBody: { raw: [{ bytes: new Uint8Array([1]).buffer }, { file: 'C:/x' }] } });
  const u = B.browserContext('https://s.example/up');
  assert.equal(u.method, undefined);
  assert.equal(u._post, true);
});

test('redirect targets map back to the request; GET drops Content-Type', () => {
  const { B, listeners } = load();
  listeners.before({ requestId: '4', url: 'https://a.example/get', method: 'GET' });
  listeners.headers({ requestId: '4', requestHeaders: [{ name: 'Accept-Language', value: 'bn' }, { name: 'Content-Type', value: 'x' }] });
  listeners.redirect({ requestId: '4', redirectUrl: 'https://cdn.example/file.zip' });
  const c = B.browserContext('https://cdn.example/file.zip', { visitorData: 'Cgt4' });
  assert.deepEqual(JSON.parse(JSON.stringify(c.headers)), { 'accept-language': 'bn' });
  assert.equal(c.visitorData, 'Cgt4');
  assert.equal(B.browserContext('https://unknown.example/').headers, undefined);
});

test('replies find their request by id; pushes reach their handlers', async () => {
  const { B, posted, port } = load();
  const hello = posted.find((m) => m.type === 'hello');
  port()({ type: 'pong', version: 2, rid: hello.rid });
  await new Promise((r) => setTimeout(r, 5));
  assert.equal(B.live(), true);
  const got = [];
  B.on('event', (m) => got.push(m.event));
  const p = B.send({ type: 'list' });
  const sent = posted.find((m) => m.type === 'list');
  port()({ type: 'event', event: 'done', id: 1 });
  port()({ type: 'list', items: [], rid: sent.rid });
  assert.equal((await p).items.length, 0);
  assert.deepEqual(got, ['done']);
});

test('without the live port every request still goes through the one-shot host', async () => {
  const { B } = load();
  const r = await B.send({ type: 'ping' });
  assert.equal(r.via, 'oneshot');
});

test('a capture keeps one stable id across retries of the same request object', async () => {
  const { B } = load();
  const request = { type: 'download', url: 'https://example.test/file.zip' };
  const first = await B.send(request);
  const retry = await B.send(request);
  assert.match(first.m.captureId, /^[a-z0-9-]{8,96}$/i);
  assert.equal(retry.m.captureId, first.m.captureId);
  const nextCapture = await B.send({ ...request });
  assert.notEqual(nextCapture.m.captureId, first.m.captureId);
});

test('a live-port failure retries the same capture id through the one-shot host', async () => {
  const { B, posted, port, gone } = load();
  const hello = posted.find((m) => m.type === 'hello');
  port()({ type: 'pong', version: 2, rid: hello.rid });
  await new Promise((r) => setTimeout(r, 5));
  assert.equal(B.live(), true);

  const request = { type: 'download', url: 'https://example.test/retry.zip' };
  const sent = B.send(request);
  await new Promise((r) => setTimeout(r, 5));
  const persistent = posted.find((m) => m.type === 'download');
  assert.ok(persistent && persistent.captureId);

  gone()();
  const fallback = await sent;
  assert.equal(fallback.via, 'oneshot');
  assert.equal(fallback.m.captureId, persistent.captureId);
});

test('one-shot native messaging retries transient failures with the same request id', async () => {
  let attempts = 0;
  const { B } = load({ sendNativeMessage: async (host, message) => {
    attempts++;
    if (attempts < 3) throw new Error('host is starting');
    return { type: 'ok', attempts, message };
  } });
  const request = { type: 'download', url: 'https://example.test/retry.zip' };
  const reply = await B.send(request);
  assert.equal(attempts, 3);
  assert.equal(reply.message.captureId, (await B.send(request)).message.captureId);
});

test('native port connection retries transient startup failures', async () => {
  let attempts = 0;
  const { posted } = load({ connectNative: () => {
    attempts++;
    if (attempts < 3) throw new Error('host is starting');
    return { onMessage: { addListener() {} }, onDisconnect: { addListener() {} }, postMessage: (m) => posted.push(m) };
  } });
  await new Promise((resolve) => setTimeout(resolve, 500));
  assert.equal(attempts, 3);
  assert.ok(posted.some((message) => message.type === 'hello'));
});
