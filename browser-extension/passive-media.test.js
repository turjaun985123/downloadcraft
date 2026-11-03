const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const rank = require('./rank.js');

const source = fs.readFileSync(__dirname + '/background.js', 'utf8');
const start = source.indexOf('if (msg.type === "pageMedia" && Array.isArray(msg.items))');
const end = source.indexOf('if (msg.type === "getMedia")', start);
assert.ok(start >= 0 && end > start, 'background pageMedia handler exists');
const handlerSource = source.slice(start, end);

function pageMediaHarness() {
  const noted = [];
  const hostCalls = [];
  const tabMedia = new Map();
  const handle = new Function(
    'msg', 'sender', 'settings', 'DCRank', 'privateHost', 'hostBlocked',
    'noteMedia', 'tabMedia', 'hostSend',
    `return (async () => { ${handlerSource} })();`,
  );
  return {
    noted,
    hostCalls,
    handle: (items) => handle(
      { type: 'pageMedia', items },
      { tab: { id: 17 }, url: 'https://watch.example/video' },
      async () => ({ deepDetect: true }),
      rank,
      () => false,
      () => false,
      (tabId, item) => noted.push({ tabId, item }),
      tabMedia,
      async (...args) => { hostCalls.push(args); return { type: 'ok' }; },
    ),
  };
}

test('passively reported manifests and media tracks populate candidates without native handoff', async () => {
  const h = pageMediaHarness();
  const result = await h.handle([
    { url: 'https://cdn.example/video/master.m3u8', mime: 'application/vnd.apple.mpegurl' },
    { url: 'https://cdn.example/video/track.m4s', mime: 'video/mp4', pairUrl: 'https://cdn.example/video/audio.m4s', pairMime: 'audio/mp4' },
  ]);

  assert.deepEqual(result, { ok: true });
  assert.equal(h.noted.length, 2);
  assert.equal(h.noted[0].tabId, 17);
  assert.equal(h.hostCalls.length, 0);
});
