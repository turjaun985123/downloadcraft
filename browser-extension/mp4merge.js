// DCMp4: join a video-only MP4 and an audio-only MP4 into one file, in the
// browser, without re-encoding - what "Download in browser" needs for the
// qualities sites only serve as separate video and audio (Facebook 720p+,
// YouTube HD). Both inputs are DASH representations: either fragmented
// (moov + mvex, then moof/mdat pairs - YouTube, Facebook) or plain (one
// mdat with stco/co64 chunk offsets). Output: one ftyp, one moov with both
// tracks (ids 1 and 2), then the media - fragments interleaved by time, or
// the two mdats back to back with chunk offsets moved.
// Pure: no DOM or chrome.* APIs, so it runs under `node --test` too.
(function (g) {
  "use strict";

  const CONTAINERS = new Set(["moov", "trak", "mdia", "minf", "stbl", "mvex", "moof", "traf", "edts", "dinf"]);

  const need = (b, i, n) => {
    if (!Number.isSafeInteger(i) || !Number.isSafeInteger(n) || i < 0 || n < 0 || i > b.length - n) {
      throw new Error("mp4: truncated box field");
    }
  };
  const byteAt = (b, i) => { need(b, i, 1); return b[i]; };
  const u32 = (b, i) => {
    need(b, i, 4);
    return ((b[i] << 24) >>> 0) + (b[i + 1] << 16) + (b[i + 2] << 8) + b[i + 3];
  };
  const put32 = (b, i, v) => { need(b, i, 4); b[i] = (v >>> 24) & 255; b[i + 1] = (v >>> 16) & 255; b[i + 2] = (v >>> 8) & 255; b[i + 3] = v & 255; };
  const u64 = (b, i) => {
    need(b, i, 8);
    const value = u32(b, i) * 4294967296 + u32(b, i + 4);
    if (!Number.isSafeInteger(value)) throw new Error("mp4: box size is not safely representable");
    return value;
  };
  const put64 = (b, i, v) => { put32(b, i, Math.floor(v / 4294967296)); put32(b, i + 4, v % 4294967296); };
  const typeAt = (b, i) => { need(b, i, 4); return String.fromCharCode(b[i], b[i + 1], b[i + 2], b[i + 3]); };

  // Child boxes of [start, end): {type, start, end, body} (body = payload start).
  function boxes(b, start, end) {
    const out = [];
    let i = start;
    if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 0 || end < start || end > b.length) {
      throw new Error("mp4: invalid box bounds");
    }
    while (i < end) {
      if (end - i < 8) throw new Error(`mp4: truncated box header at ${i}`);
      let size = u32(b, i);
      const type = typeAt(b, i + 4);
      let body = i + 8;
      if (size === 1) {
        if (end - i < 16) throw new Error(`mp4: truncated extended ${type} box at ${i}`);
        size = u64(b, i + 8);
        body = i + 16;
      } else if (size === 0) {
        size = end - i;
      }
      if (!Number.isSafeInteger(size) || size < body - i || size > end - i) throw new Error(`mp4: bad ${type} box at ${i}`);
      out.push({ type, start: i, end: i + size, body });
      i += size;
    }
    return out;
  }
  const child = (b, box, type) => boxes(b, box.body, box.end).find((x) => x.type === type) || null;
  const path = (b, box, types) => types.reduce((cur, t) => (cur ? child(b, cur, t) : null), box);

  function parse(buf) {
    const b = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
    const top = boxes(b, 0, b.length);
    const ftyp = top.find((x) => x.type === "ftyp");
    const moov = top.find((x) => x.type === "moov");
    if (!moov) throw new Error("mp4: no moov box");
    const traks = boxes(b, moov.body, moov.end).filter((x) => x.type === "trak");
    if (!traks.length) throw new Error("mp4: no track");
    const trak = traks[0];
    const mdhd = path(b, trak, ["mdia", "mdhd"]);
    const timescale = mdhd ? (byteAt(b, mdhd.body) === 1 ? u32(b, mdhd.body + 20) : u32(b, mdhd.body + 12)) : 0;
    const hdlr = path(b, trak, ["mdia", "hdlr"]);
    const handler = hdlr ? typeAt(b, hdlr.body + 8) : "";
    const mvex = child(b, moov, "mvex");
    // Fragments: each moof plus the mdat(s) after it, up to the next moof.
    const frags = [];
    for (let k = 0; k < top.length; k++) {
      if (top[k].type !== "moof") continue;
      let end = top[k].end;
      for (let j = k + 1; j < top.length && top[j].type !== "moof"; j++) {
        if (top[j].type === "mdat") end = top[j].end;
      }
      const tfdt = path(b, top[k], ["traf", "tfdt"]);
      const time = tfdt ? (byteAt(b, tfdt.body) === 1 ? u64(b, tfdt.body + 4) : u32(b, tfdt.body + 4)) : 0;
      frags.push({ start: top[k].start, end, moof: top[k], time });
    }
    return { b, ftyp, moov, trak, timescale, handler, mvex, frags, mdats: top.filter((x) => x.type === "mdat"),
      mvhd: child(b, moov, "mvhd") };
  }

  const copy = (b, box) => b.slice(box.start, box.end);

  function setTrackId(box, b, id) {
    // tkhd / trex / tfhd carry the id right after their version/flags (tkhd
    // after its times).
    if (box.type === "tkhd") put32(b, box.body + (byteAt(b, box.body) === 1 ? 20 : 12), id);
    else put32(b, box.body + 4, id);
  }

  // A copy of `src` (a box) with every tkhd/trex/tfhd inside set to `id`.
  function withTrackId(src, id) {
    const b = src.slice();
    const walk = (s, e) => {
      for (const x of boxes(b, s, e)) {
        if (x.type === "tkhd" || x.type === "trex" || x.type === "tfhd") setTrackId(x, b, id);
        if (CONTAINERS.has(x.type)) walk(x.body, x.end);
      }
    };
    walk(0, b.length);
    return b;
  }

  function box(type, ...parts) {
    const size = 8 + parts.reduce((n, p) => n + p.length, 0);
    const out = new Uint8Array(size);
    put32(out, 0, size);
    for (let i = 0; i < 4; i++) out[4 + i] = type.charCodeAt(i);
    let at = 8;
    for (const p of parts) { out.set(p, at); at += p.length; }
    return out;
  }

  function concat(parts) {
    const n = parts.reduce((s, p) => s + p.length, 0);
    const out = new Uint8Array(n);
    let at = 0;
    for (const p of parts) { out.set(p, at); at += p.length; }
    return out;
  }

  // mvhd of the video file, with the longer duration and next track id 3.
  function mergedMvhd(v, a) {
    const b = copy(v.b, v.mvhd);
    const off = 8;
    const v1 = byteAt(b, off) === 1;
    const tsAt = off + (v1 ? 20 : 12);
    const ts = u32(b, tsAt);
    const durOf = (p) => {
      const m = p.mvhd;
      if (!m) return 0;
      const one = byteAt(p.b, m.body) === 1;
      const pts = u32(p.b, m.body + (one ? 20 : 12));
      const d = one ? u64(p.b, m.body + 24) : u32(p.b, m.body + 16);
      return pts ? d / pts : 0;
    };
    const dur = Math.round(Math.max(durOf(v), durOf(a)) * ts);
    if (v1) put64(b, tsAt + 4, dur); else put32(b, tsAt + 4, Math.min(dur, 0xffffffff));
    put32(b, b.length - 4, 3);
    return b;
  }

  const s32 = (b, i) => { const x = u32(b, i); return x > 0x7fffffff ? x - 4294967296 : x; };

  // Every sample of a fragmented track, in order: where its bytes are in the
  // source, its size, duration, composition offset and whether it is a sync
  // sample. A gap between fragments (tfdt ahead of the running time) is
  // added to the sample before it, so the timeline stays where it was.
  function samplesOf(p) {
    const b = p.b;
    const trex = p.mvex && boxes(b, p.mvex.body, p.mvex.end).find((x) => x.type === "trex");
    const dflt = trex
      ? { dur: u32(b, trex.body + 12), size: u32(b, trex.body + 16), flags: u32(b, trex.body + 20) }
      : { dur: 0, size: 0, flags: 0 };
    const samples = [];
    const chunks = [];
    let time = 0;
    for (const f of p.frags) {
      for (const traf of boxes(b, f.moof.body, f.moof.end).filter((x) => x.type === "traf")) {
        const tfhd = child(b, traf, "tfhd");
        if (!tfhd) continue;
        const tf = u32(b, tfhd.body) & 0xffffff;
        let at = tfhd.body + 8;
        let base = f.moof.start;
        if (tf & 0x1) { base = u64(b, at); at += 8; }
        if (tf & 0x2) at += 4;
        let tDur = dflt.dur, tSize = dflt.size, tFlags = dflt.flags;
        if (tf & 0x8) { tDur = u32(b, at); at += 4; }
        if (tf & 0x10) { tSize = u32(b, at); at += 4; }
        if (tf & 0x20) { tFlags = u32(b, at); at += 4; }
        const tfdt = child(b, traf, "tfdt");
        if (tfdt) {
          const start = byteAt(b, tfdt.body) === 1 ? u64(b, tfdt.body + 4) : u32(b, tfdt.body + 4);
          if (start > time && samples.length) samples[samples.length - 1].dur += start - time;
          time = Math.max(time, start);
        }
        let ptr = base;
        for (const trun of boxes(b, traf.body, traf.end).filter((x) => x.type === "trun")) {
          const version = byteAt(b, trun.body);
          const rf = u32(b, trun.body) & 0xffffff;
          const n = u32(b, trun.body + 4);
          let q = trun.body + 8;
          if (rf & 0x1) { ptr = base + s32(b, q); q += 4; }
          let first = null;
          if (rf & 0x4) { first = u32(b, q); q += 4; }
          const chunk = { count: 0, bytes: 0, t: p.timescale ? time / p.timescale : 0, from: samples.length };
          for (let k = 0; k < n; k++) {
            let dur = tDur, size = tSize, flags = tFlags, cts = 0;
            if (rf & 0x100) { dur = u32(b, q); q += 4; }
            if (rf & 0x200) { size = u32(b, q); q += 4; }
            if (rf & 0x400) { flags = u32(b, q); q += 4; }
            if (rf & 0x800) { cts = version ? s32(b, q) : u32(b, q); q += 4; }
            if (k === 0 && first !== null) flags = first;
            if (ptr + size > b.length) throw new Error("mp4: sample outside the file");
            samples.push({ off: ptr, size, dur, cts, sync: !(flags & 0x10000) });
            ptr += size;
            time += dur;
            chunk.count++;
            chunk.bytes += size;
          }
          if (chunk.count) chunks.push(chunk);
        }
      }
    }
    return { samples, chunks, duration: time };
  }

  const full = (type, version, ...parts) => box(type, new Uint8Array([version, 0, 0, 0]), ...parts);
  const be32 = (v) => { const o = new Uint8Array(4); put32(o, 0, v >>> 0); return o; };
  const be64 = (v) => { const o = new Uint8Array(8); put64(o, 0, v); return o; };
  // Run-length entries: [[count, value], ...].
  function runs(values) {
    const out = [];
    for (const v of values) {
      const last = out[out.length - 1];
      if (last && last[1] === v) last[0]++; else out.push([1, v]);
    }
    return out;
  }

  // A sample table (stbl) for the track, with the given chunk offsets.
  function sampleTable(p, t, offsets, wide) {
    const { samples, chunks } = t;
    const stsd = child(p.b, path(p.b, p.trak, ["mdia", "minf", "stbl"]), "stsd");
    const parts = [copy(p.b, stsd)];
    const stts = runs(samples.map((x) => x.dur));
    parts.push(full("stts", 0, be32(stts.length), ...stts.flatMap(([n, d]) => [be32(n), be32(d)])));
    if (samples.some((x) => x.cts)) {
      const ctts = runs(samples.map((x) => x.cts));
      parts.push(full("ctts", samples.some((x) => x.cts < 0) ? 1 : 0, be32(ctts.length), ...ctts.flatMap(([n, c]) => [be32(n), be32(c)])));
    }
    if (samples.some((x) => !x.sync)) {
      const sync = [];
      samples.forEach((x, k) => { if (x.sync) sync.push(k + 1); });
      parts.push(full("stss", 0, be32(sync.length), ...sync.map(be32)));
    }
    const stsc = [];
    chunks.forEach((c, k) => {
      if (!stsc.length || stsc[stsc.length - 1][1] !== c.count) stsc.push([k + 1, c.count]);
    });
    parts.push(full("stsc", 0, be32(stsc.length), ...stsc.flatMap(([f, n]) => [be32(f), be32(n), be32(1)])));
    parts.push(full("stsz", 0, be32(0), be32(samples.length), ...samples.map((x) => be32(x.size))));
    parts.push(wide
      ? full("co64", 0, be32(offsets.length), ...offsets.map(be64))
      : full("stco", 0, be32(offsets.length), ...offsets.map(be32)));
    return box("stbl", ...parts);
  }

  // tkhd/mdhd/mvhd durations: 32-bit in version 0, 64-bit in version 1.
  function setDuration(b, at32, at64, value) {
    if (byteAt(b, 8) === 1) put64(b, at64, value); else put32(b, at32, Math.min(value, 0xffffffff));
  }

  // The track's trak with its new id, durations and sample table. An edit
  // list in a fragmented file may give a duration of 0 ("to the end"),
  // which a plain file reads as an empty edit: it gets the real length.
  function plainTrak(p, t, id, movieScale, offsets, wide) {
    const toMovie = (d) => (p.timescale ? Math.round((d * movieScale) / p.timescale) : 0);
    const movieDur = toMovie(t.duration);
    const rebuild = (b, x) => {
      if (x.type === "stbl") return sampleTable(p, t, offsets, wide);
      const c = copy(b, x);
      if (x.type === "tkhd") {
        put32(c, 8 + (c[8] === 1 ? 20 : 12), id);
        setDuration(c, 8 + 20, 8 + 28, movieDur);
        return c;
      }
      if (x.type === "mdhd") {
        setDuration(c, 8 + 16, 8 + 24, t.duration);
        return c;
      }
      if (x.type === "elst") {
        const v1 = c[8] === 1;
        const step = v1 ? 20 : 12;
        for (let k = 0, n = u32(c, 12); k < n; k++) {
          const at = 16 + k * step;
          const d = v1 ? u64(c, at) : u32(c, at);
          const mt = v1 ? s32(c, at + 12) : s32(c, at + 4);
          if (d === 0 && mt >= 0) {
            const rest = Math.max(0, movieDur - toMovie(mt));
            if (v1) put64(c, at, rest); else put32(c, at, Math.min(rest, 0xffffffff));
          }
        }
        return c;
      }
      if (CONTAINERS.has(x.type)) return box(x.type, ...boxes(b, x.body, x.end).map((y) => rebuild(b, y)));
      return c;
    };
    return rebuild(p.b, p.trak);
  }

  // Fragmented inputs (DASH) become one plain MP4: ftyp, moov with a full
  // sample table per track, one mdat. Players seek in it reliably; the
  // fragmented join (no index) made some of them lose the sound partway
  // through or after a seek.
  function mergeFragmented(v, a) {
    const tracks = [[v, 1], [a, 2]].map(([p, id]) => ({ p, id, t: samplesOf(p) }));
    for (const { t } of tracks) if (!t.samples.length) throw new Error("mp4: a track has no samples");
    // mdat layout: each fragment's run of samples is one chunk, chunks of
    // both tracks interleaved by time, so playback reads forward.
    const order = [];
    for (const tr of tracks) tr.t.chunks.forEach((c, k) => order.push({ tr, k, t: c.t }));
    order.sort((x, y) => x.t - y.t || x.tr.id - y.tr.id);
    const mediaSize = order.reduce((n, o) => n + o.tr.t.chunks[o.k].bytes, 0);
    const movieScale = (v.mvhd && u32(v.b, v.mvhd.body + (byteAt(v.b, v.mvhd.body) === 1 ? 20 : 12))) || 1000;
    const ftyp = box("ftyp", new Uint8Array([105, 115, 111, 109]), be32(512),
      new Uint8Array([105, 115, 111, 109, 105, 115, 111, 50, 97, 118, 99, 49, 109, 112, 52, 49]));
    const mdatHead = mediaSize + 8 > 0xffffffff ? 16 : 8;
    const build = (wide, base) => {
      const offs = new Map(tracks.map((tr) => [tr.id, []]));
      let pos = base;
      for (const o of order) { offs.get(o.tr.id)[o.k] = pos; pos += o.tr.t.chunks[o.k].bytes; }
      const mvhd = v.mvhd ? copy(v.b, v.mvhd) : null;
      if (!mvhd) throw new Error("mp4: no mvhd");
      const longest = Math.max(...tracks.map((tr) => (tr.p.timescale ? Math.round((tr.t.duration * movieScale) / tr.p.timescale) : 0)));
      setDuration(mvhd, 8 + 16, 8 + 24, longest);
      put32(mvhd, mvhd.length - 4, 3);
      return box("moov", mvhd, ...tracks.map((tr) => plainTrak(tr.p, tr.t, tr.id, movieScale, offs.get(tr.id), wide)));
    };
    // Offsets depend on the moov's size, which only depends on the offset
    // width: size it once, then build with the real offsets.
    let wide = false;
    let size = build(false, 0).length;
    if (ftyp.length + size + mdatHead + mediaSize > 0xffffffff) {
      wide = true;
      size = build(true, 0).length;
    }
    const moov = build(wide, ftyp.length + size + mdatHead);
    const out = new Uint8Array(ftyp.length + moov.length + mdatHead + mediaSize);
    out.set(ftyp, 0);
    out.set(moov, ftyp.length);
    let at = ftyp.length + moov.length;
    if (mdatHead === 16) {
      put32(out, at, 1); out.set([109, 100, 97, 116], at + 4); put64(out, at + 8, mediaSize + 16);
    } else {
      put32(out, at, mediaSize + 8); out.set([109, 100, 97, 116], at + 4);
    }
    at += mdatHead;
    for (const o of order) {
      const c = o.tr.t.chunks[o.k];
      for (let j = c.from; j < c.from + c.count; j++) {
        const x = o.tr.t.samples[j];
        out.set(o.tr.p.b.subarray(x.off, x.off + x.size), at);
        at += x.size;
      }
    }
    return out;
  }

  function mergePlain(v, a) {
    // Layout: ftyp, moov, then each file's mdats in order. Chunk offsets
    // (stco/co64) are absolute, so each is moved by where its mdat lands.
    const traks = [[v, withTrackId(copy(v.b, v.trak), 1)], [a, withTrackId(copy(a.b, a.trak), 2)]];
    const mvhd = mergedMvhd(v, a);
    const ftyp = v.ftyp ? copy(v.b, v.ftyp) : box("ftyp");
    const moovSize = 8 + mvhd.length + traks.reduce((n, [, t]) => n + t.length, 0);
    let pos = ftyp.length + moovSize;
    const media = [];
    for (const [p, t] of traks) {
      const moves = p.mdats.map((m) => {
        const r = { from: m.start, to: m.end, delta: pos - m.start };
        pos += m.end - m.start;
        media.push(p.b.slice(m.start, m.end));
        return r;
      });
      const shift = (off) => {
        const m = moves.find((x) => off >= x.from && off < x.to);
        return m ? off + m.delta : off;
      };
      const walk = (s, e) => {
        for (const x of boxes(t, s, e)) {
          if (x.type === "stco" || x.type === "co64") {
            const n = u32(t, x.body + 4);
            for (let k = 0; k < n; k++) {
              if (x.type === "stco") {
                const at = x.body + 8 + k * 4;
                const nv = shift(u32(t, at));
                if (nv > 0xffffffff) throw new Error("mp4: file too large to merge here");
                put32(t, at, nv);
              } else {
                const at = x.body + 8 + k * 8;
                put64(t, at, shift(u64(t, at)));
              }
            }
          }
          if (CONTAINERS.has(x.type)) walk(x.body, x.end);
        }
      };
      walk(0, t.length);
    }
    return concat([ftyp, box("moov", mvhd, ...traks.map(([, t]) => t)), ...media]);
  }

  // Join `video` and `audio` (ArrayBuffer/Uint8Array) into one MP4.
  function merge(video, audio) {
    let v = parse(video);
    let a = parse(audio);
    // Accept the two in either order.
    if (v.handler === "soun" && a.handler === "vide") [v, a] = [a, v];
    const fragmented = v.frags.length > 0 && a.frags.length > 0;
    if (fragmented) return mergeFragmented(v, a);
    if (!v.frags.length && !a.frags.length) return mergePlain(v, a);
    throw new Error("mp4: one input is fragmented and the other is not");
  }

  // Whether bytes look like an MP4 (ftyp/styp/moov/moof first box).
  function isMp4(buf) {
    const b = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
    return b.length >= 8 && ["ftyp", "styp", "moov", "moof", "sidx"].includes(typeAt(b, 4));
  }

  const DCMp4 = { merge, parse, isMp4 };
  g.DCMp4 = DCMp4;
  if (typeof module !== "undefined" && module.exports) module.exports = DCMp4;
})(typeof globalThis !== "undefined" ? globalThis : this);
