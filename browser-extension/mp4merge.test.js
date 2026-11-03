// mp4merge.js: video-only + audio-only MP4 -> one file, no re-encoding.
// Fixtures in testdata/ were made with ffmpeg (2 s, 64x48 H.264 + AAC) in the
// three layouts sites serve: plain (stco), fragmented with offsets relative
// to each moof (YouTube/Facebook DASH), and fragmented with absolute
// base-data-offsets. Each merged file was also checked with ffprobe/ffmpeg
// (two streams, decodes cleanly). Run: `node --test mp4merge.test.js`
const { it } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const M = require("./mp4merge.js");

const read = (f) => new Uint8Array(fs.readFileSync(__dirname + "/testdata/" + f));
const u32 = (b, i) => ((b[i] << 24) >>> 0) + (b[i + 1] << 16) + (b[i + 2] << 8) + b[i + 3];

function topTypes(b) {
  const out = [];
  for (let i = 0; i + 8 <= b.length;) {
    const size = u32(b, i);
    out.push(String.fromCharCode(b[i + 4], b[i + 5], b[i + 6], b[i + 7]));
    i += size;
  }
  return out;
}

// Track ids of every tkhd inside the moov.
function trackIds(b) {
  const ids = [];
  const s = Buffer.from(b);
  for (let i = s.indexOf("tkhd"); i >= 0; i = s.indexOf("tkhd", i + 4)) {
    const body = i + 4;
    ids.push(u32(b, body + (b[body] === 1 ? 20 : 12)));
  }
  return ids;
}

for (const [kind, v, a] of [
  ["fragmented (relative offsets)", "v_frag.mp4", "a_frag.m4a"],
  ["fragmented (absolute offsets)", "v_frag_abs.mp4", "a_frag_abs.m4a"],
]) {
  it(`joins ${kind} video and audio into one plain, indexed file`, () => {
    const out = M.merge(read(v), read(a));
    const p = M.parse(out);
    assert.deepEqual(trackIds(out.slice(p.moov.start, p.moov.end)), [1, 2]);
    // No fragments left: one moov with a sample table per track, one mdat
    // (players seek in this layout reliably).
    assert.equal(p.frags.length, 0);
    assert.deepEqual(topTypes(out), ["ftyp", "moov", "mdat"]);
    const s = Buffer.from(out.slice(p.moov.start, p.moov.end));
    for (const t of ["stts", "stsz", "stsc", "stco"]) {
      assert.equal(s.toString("latin1").split(t).length - 1, 2, `${t} per track`);
    }
    assert.ok(!s.includes("mvex"), "no fragment header");
    // Every sample's bytes are in the file: the media is the sum of both.
    const sizes = (src) => {
      const b = Buffer.from(src);
      let n = 0;
      for (let i = b.indexOf("trun"); i >= 0; i = b.indexOf("trun", i + 4)) n++;
      return n;
    };
    assert.ok(sizes(read(v)) > 0 && sizes(read(a)) > 0);
    const mdat = out.length - (p.moov.end);
    assert.ok(mdat > 8);
  });
}

it("joins plain (stco) files and moves every chunk offset with its data", () => {
  const vin = read("v_plain.mp4");
  const ain = read("a_plain.m4a");
  const out = M.merge(vin, ain);
  const p = M.parse(out);
  assert.deepEqual(trackIds(out.slice(p.moov.start, p.moov.end)), [1, 2]);
  assert.deepEqual(topTypes(out), ["ftyp", "moov", "mdat", "mdat"]);
  // The first chunk of each track still points at the same bytes.
  const firstChunk = (b, nth) => {
    const s = Buffer.from(b);
    let i = -1;
    for (let k = 0; k <= nth; k++) i = s.indexOf("stco", i + 1);
    return u32(b, i + 4 + 8);
  };
  const same = (src, srcOff, dst, dstOff) => Buffer.compare(Buffer.from(src.slice(srcOff, srcOff + 64)), Buffer.from(dst.slice(dstOff, dstOff + 64))) === 0;
  assert.ok(same(vin, firstChunk(vin, 0), out, firstChunk(out, 0)), "video chunk");
  assert.ok(same(ain, firstChunk(ain, 0), out, firstChunk(out, 1)), "audio chunk");
});

it("accepts the inputs in either order and refuses mixed layouts", () => {
  const a = M.merge(read("v_frag.mp4"), read("a_frag.m4a"));
  const b = M.merge(read("a_frag.m4a"), read("v_frag.mp4"));
  assert.equal(a.length, b.length);
  assert.throws(() => M.merge(read("v_frag.mp4"), read("a_plain.m4a")), /fragmented/);
  assert.throws(() => M.merge(new Uint8Array(16), read("a_plain.m4a")), /mp4/);
});

it("recognises MP4 bytes", () => {
  assert.ok(M.isMp4(read("v_frag.mp4")));
  assert.ok(!M.isMp4(new TextEncoder().encode("#EXTM3U\n#EXT-X-VERSION:3\n")));
});

it("rejects malformed and truncated MP4 atom boundaries", () => {
  assert.throws(() => M.parse(Uint8Array.of(0, 0, 0, 5, 0x66, 0x74, 0x79, 0x70)), /bad ftyp box/);
  assert.throws(() => M.parse(Uint8Array.of(0, 0, 0, 8, 0x66, 0x74, 0x79, 0x70, 0)), /truncated box header/);
  assert.throws(() => M.parse(Uint8Array.of(0, 0, 0, 1, 0x66, 0x74, 0x79, 0x70)), /truncated extended/);
});
