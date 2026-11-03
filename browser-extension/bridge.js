// Live link to the Download Craft app, IDM-style.
//
// 1. One persistent native port (runtime.connectNative) instead of starting
//    the host program for every message: requests carry an id ("rid"), the
//    app can push to us (capture settings, site rules, "finished"/"failed"
//    events, "fetch this inside the page" requests), and it reconnects with
//    back-off. Every request still falls back to sendNativeMessage.
// 2. The browser's side of each request (method + POST body, request
//    headers, server IP, proxy), so a download handed to the app is repeated
//    the way the browser made it.
// 3. Fetch-through-page: when a server refuses the app, the app can ask for
//    the file to be fetched inside the open page, with its own session.
//
// Nothing runs at load time: background.js calls DCBridge.start() once its
// own globals exist (Firefox loads this file first).
var DCBridge = (() => {
  "use strict";
  const api = (typeof browser !== "undefined" && browser.runtime) ? browser : chrome;
  const isFirefox = typeof browser !== "undefined" && browser.runtime && typeof browser.runtime.getBrowserInfo === "function";
  let host = "";
  let port = null;
  let live = false;          // port answered "hello" with a push-capable host
  let rid = 0;
  let backoff = 1000;
  let retryTimer = 0;
  let captureSequence = 0;
  const captureIds = new WeakMap();
  const requestIds = new WeakMap();
  const waiters = new Map(); // rid -> {resolve, reject, timer}
  const handlers = { settings: [], event: [], pageFetch: [], state: [] };

  function emit(type, msg) {
    for (const fn of handlers[type] || []) {
      try { fn(msg); } catch (e) { console.error("[download-craft] bridge handler", e); }
    }
  }

  async function connect() {
    retryTimer = 0;
    if (port || !host) return;
    const delays = [150, 300];
    for (let attempt = 0; attempt < 3 && !port; attempt++) {
      try {
        port = api.runtime.connectNative(host);
      } catch (e) {
        if (attempt < delays.length) await new Promise((resolve) => setTimeout(resolve, delays[attempt]));
      }
    }
    if (!port) return later();
    port.onMessage.addListener(onMessage);
    port.onDisconnect.addListener(() => {
      void (chrome.runtime && chrome.runtime.lastError);
      port = null;
      const was = live;
      live = false;
      for (const w of waiters.values()) { clearTimeout(w.timer); w.reject(new Error("the app's connection closed")); }
      waiters.clear();
      if (was) emit("state", { live: false });
      later();
    });
    call({ type: "hello" }, 8000).then((r) => {
      live = !!r && r.type === "pong" && (r.version || 0) >= 2;
      if (live) { backoff = 1000; emit("state", { live: true, theme: r.theme }); }
    }).catch(() => { /* old host or none: sendNativeMessage keeps working */ });
  }

  function later() {
    if (retryTimer) return;
    retryTimer = setTimeout(connect, backoff);
    backoff = Math.min(backoff * 2, 60000);
  }

  function onMessage(m) {
    if (!m || typeof m !== "object") return;
    if (Number.isInteger(m.rid) && waiters.has(m.rid)) {
      const w = waiters.get(m.rid);
      waiters.delete(m.rid);
      clearTimeout(w.timer);
      w.resolve(m);
      return;
    }
    if (m.type === "settings" || m.type === "event" || m.type === "pageFetch") emit(m.type, m);
  }

  function call(msg, timeoutMs) {
    return new Promise((resolve, reject) => {
      if (!port) return reject(new Error("not connected"));
      const id = ++rid;
      const timer = setTimeout(() => { waiters.delete(id); reject(new Error("the app did not answer")); }, timeoutMs || 20000);
      waiters.set(id, { resolve, reject, timer });
      try { port.postMessage({ ...msg, rid: id }); } catch (e) { waiters.delete(id); clearTimeout(timer); reject(e); }
    });
  }

  function outgoingMessage(msg) {
    if (!msg || typeof msg !== "object") return msg;
    const validId = (id) => typeof id === "string" && /^[A-Za-z0-9._-]{8,96}$/.test(id);
    const makeId = () => typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
      ? crypto.randomUUID()
      : `${Date.now().toString(36)}-${(++captureSequence).toString(36)}-${Math.random().toString(36).slice(2, 12)}`;
    if (msg.type === "download" && !validId(msg.captureId)) {
      let id = captureIds.get(msg);
      if (!id) {
        id = makeId();
        captureIds.set(msg, id);
      }
      return { ...msg, captureId: id };
    }
    // These messages append commands, download history, or open a batch
    // window. A timeout can trigger a one-shot retry after the first host
    // already acted, so keep a stable id across that retry as well.
    if (["links", "control", "record"].includes(msg.type) && !validId(msg.requestId)) {
      let id = requestIds.get(msg);
      if (!id) {
        id = makeId();
        requestIds.set(msg, id);
      }
      return { ...msg, requestId: id };
    }
    return msg;
  }

  async function sendOneShot(msg) {
    const delays = [150, 300];
    let lastError;
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const reply = await api.runtime.sendNativeMessage(host || "com.download_craft", msg);
        if (reply) return reply;
        lastError = new Error("the native host returned no reply");
      } catch (error) {
        lastError = error;
      }
      if (attempt < delays.length) await new Promise((resolve) => setTimeout(resolve, delays[attempt]));
    }
    throw lastError || new Error("the native host did not respond");
  }

  // Every message to the app goes through here: the live port when there is
  // one, the one-shot host otherwise (same replies either way).
  async function send(msg) {
    const outgoing = outgoingMessage(msg);
    if (live && port) {
      try { return await call(outgoing); } catch (e) { /* fall back below */ }
    }
    return sendOneShot(outgoing);
  }

  // ---- the browser's side of each request -----------------------------------
  const CTX_TTL = 3 * 60 * 1000;
  const byId = new Map();  // requestId -> ctx
  const byUrl = new Map(); // url -> ctx (original and redirect targets)
  const NAV_TYPES = ["main_frame", "sub_frame", "other"];
  let browserProxy = "";   // Chrome: the profile's fixed proxy, if any

  function ctxOf(id) {
    let c = byId.get(id);
    if (!c) {
      c = { at: Date.now() };
      byId.set(id, c);
      if (byId.size > 400) byId.delete(byId.keys().next().value);
    }
    return c;
  }
  function index(url, c) {
    if (!url) return;
    byUrl.delete(url);
    byUrl.set(url, c);
    if (byUrl.size > 400) byUrl.delete(byUrl.keys().next().value);
  }

  function proxyText(info) {
    if (!info || !info.host || !info.port) return "";
    const t = String(info.type || "").toLowerCase();
    const scheme = t === "socks" || t === "socks5" ? (info.proxyDNS ? "socks5h" : "socks5")
      : t === "socks4" ? "socks4" : t === "https" ? "https" : t === "http" ? "http" : "";
    return scheme ? `${scheme}://${info.host}:${info.port}` : "";
  }

  async function readChromeProxy() {
    try {
      if (!api.proxy || !api.proxy.settings || isFirefox) return;
      const s = await api.proxy.settings.get({});
      const v = s && s.value;
      const rules = v && v.mode === "fixed_servers" && v.rules;
      const one = rules && (rules.singleProxy || rules.proxyForHttps || rules.proxyForHttp || rules.fallbackProxy);
      browserProxy = one ? proxyText({ type: one.scheme || "http", host: one.host, port: one.port }) : "";
    } catch (e) { browserProxy = ""; }
  }

  function listen() {
    const filter = { urls: ["http://*/*", "https://*/*"], types: NAV_TYPES };
    try {
      api.webRequest.onBeforeRequest.addListener((d) => {
        const c = ctxOf(d.requestId);
        c.method = String(d.method || "GET").toUpperCase();
        c.url = d.url;
        if (c.method === "POST" && d.requestBody) c.requestBody = d.requestBody;
        if (isFirefox && d.proxyInfo) c.proxy = proxyText(d.proxyInfo);
        index(d.url, c);
      }, filter, ["requestBody"]);
    } catch (e) { /* no request bodies: POST downloads stay in the browser */ }
    try {
      const spec = ["requestHeaders"];
      if (chrome.webRequest && chrome.webRequest.OnSendHeadersOptions
          && Object.prototype.hasOwnProperty.call(chrome.webRequest.OnSendHeadersOptions, "EXTRA_HEADERS")) spec.push("extraHeaders");
      api.webRequest.onSendHeaders.addListener((d) => {
        ctxOf(d.requestId).headers = d.requestHeaders || [];
      }, filter, spec);
    } catch (e) { /* headers optional */ }
    try {
      api.webRequest.onBeforeRedirect.addListener((d) => {
        const c = byId.get(d.requestId);
        if (c && d.redirectUrl) index(d.redirectUrl, c);
      }, filter);
    } catch (e) {}
    try {
      api.webRequest.onResponseStarted.addListener((d) => {
        const c = byId.get(d.requestId);
        if (!c) return;
        if (d.ip) c.ip = d.ip;
        if (isFirefox && d.proxyInfo) c.proxy = proxyText(d.proxyInfo);
        index(d.url, c);
      }, filter);
    } catch (e) {}
    try {
      if (api.proxy && api.proxy.settings && api.proxy.settings.onChange && !isFirefox) {
        api.proxy.settings.onChange.addListener(readChromeProxy);
      }
    } catch (e) {}
    readChromeProxy();
  }

  function b64(bytes) {
    let bin = "";
    for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return btoa(bin);
  }
  function headerOf(list, name) {
    const h = (list || []).find((x) => String(x.name || "").toLowerCase() === name);
    return h ? String(h.value || "") : "";
  }

  // The POST body as the server got it (form fields are re-encoded the way
  // the browser did, raw bodies are kept byte for byte). null = unknown.
  function bodyOf(c) {
    const rb = c.requestBody;
    if (!rb || rb.error) return null;
    if (Array.isArray(rb.raw)) {
      const parts = [];
      let n = 0;
      for (const p of rb.raw) {
        if (!p || !p.bytes) return null; // an uploaded file: not repeatable
        const u = new Uint8Array(p.bytes);
        parts.push(u);
        n += u.length;
      }
      if (n > 1536 * 1024) return null;
      const all = new Uint8Array(n);
      let at = 0;
      for (const u of parts) { all.set(u, at); at += u.length; }
      return b64(all);
    }
    if (rb.formData && typeof rb.formData === "object") {
      const ctype = headerOf(c.headers, "content-type").toLowerCase();
      const enc = new TextEncoder();
      if (ctype.startsWith("multipart/form-data")) {
        const m = /boundary=("?)([^";]+)\1/i.exec(headerOf(c.headers, "content-type"));
        if (!m) return null;
        let out = "";
        for (const [k, vals] of Object.entries(rb.formData)) {
          for (const v of vals) out += `--${m[2]}\r\nContent-Disposition: form-data; name="${k}"\r\n\r\n${v}\r\n`;
        }
        out += `--${m[2]}--\r\n`;
        return b64(enc.encode(out));
      }
      const q = [];
      for (const [k, vals] of Object.entries(rb.formData)) {
        for (const v of vals) q.push(encodeURIComponent(k).replace(/%20/g, "+") + "=" + encodeURIComponent(v).replace(/%20/g, "+"));
      }
      return b64(enc.encode(q.join("&")));
    }
    return null;
  }

  const KEEP = /^(user-agent|accept|accept-language|content-type|upgrade-insecure-requests|dnt|sec-ch-ua.*|sec-fetch-.*)$/;
  // What the browser knew about `url`: for BrowserContext in the app, plus
  // the exact Cookie/Referer it sent. `extra.visitorData` rides along.
  function browserContext(url, extra) {
    const out = { fromExtension: true };
    const c = byUrl.get(url);
    if (c && Date.now() - c.at < CTX_TTL) {
      const headers = {};
      for (const h of c.headers || []) {
        const n = String(h.name || "").toLowerCase();
        if (KEEP.test(n) && h.value) headers[n] = String(h.value);
      }
      if (c.method === "POST") {
        const body = bodyOf(c);
        if (body !== null) { out.method = "POST"; out.body = body; }
      } else delete headers["content-type"];
      if (Object.keys(headers).length) out.headers = headers;
      if (c.ip) out.serverIp = c.ip;
      if (c.proxy) out.proxy = c.proxy;
      out._cookie = headerOf(c.headers, "cookie") || null;
      out._referer = headerOf(c.headers, "referer") || null;
      out._post = c.method === "POST";
    }
    if (!out.proxy && browserProxy) out.proxy = browserProxy;
    if (extra && typeof extra.visitorData === "string" && extra.visitorData) out.visitorData = extra.visitorData;
    return out;
  }

  // ---- fetch inside the page -----------------------------------------------
  const pendingFetch = new Set();

  // Runs in the page (scripting.executeScript): fetch with the page's own
  // cookies and origin, stream the body to the background in base64 pieces.
  async function fetchInPage(id, url) {
    try {
      const r = await fetch(url, { credentials: "include" });
      if (!r.ok) return { ok: false, status: r.status, error: "HTTP " + r.status };
      if ((r.headers.get("content-type") || "").toLowerCase().includes("text/html")) {
        return { ok: false, status: r.status, error: "the server answered with a web page" };
      }
      const reader = r.body.getReader();
      const CH = 384 * 1024;
      let buf = new Uint8Array(0), total = 0;
      const flush = async (last) => {
        let bin = "";
        for (let i = 0; i < buf.length; i += 0x8000) bin += String.fromCharCode.apply(null, buf.subarray(i, i + 0x8000));
        const res = await chrome.runtime.sendMessage({ type: "pageFetchChunk", id, chunk: btoa(bin), done: last });
        buf = new Uint8Array(0);
        if (!res || !res.ok) throw new Error((res && res.error) || "the app stopped listening");
      };
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        total += value.length;
        if (total > 1024 * 1024 * 1024) return { ok: false, error: "too large to hand over from the page" };
        const n = new Uint8Array(buf.length + value.length);
        n.set(buf); n.set(value, buf.length); buf = n;
        if (buf.length >= CH) await flush(false);
      }
      await flush(true);
      return { ok: true };
    } catch (e) { return { ok: false, error: String((e && e.message) || e) }; }
  }

  function samePage(a, b) {
    try {
      const x = new URL(a), y = new URL(b);
      x.hash = ""; y.hash = "";
      return x.href === y.href;
    } catch (e) { return false; }
  }

  async function pageFetch(m) {
    const id = String(m.id || "");
    const fail = (error, status) => send({ type: "pageFetchChunk", fetchId: id, error: error || "failed", status: status || null }).catch(() => {});
    if (!/^[A-Za-z0-9-]{1,64}$/.test(id) || !/^https?:\/\//i.test(m.url || "") || !/^https?:\/\//i.test(m.page || "")) return fail("bad request");
    let tab = null;
    try {
      const tabs = await api.tabs.query({});
      tab = tabs.find((t) => t.url && samePage(t.url, m.page))
        || tabs.find((t) => { try { return t.url && new URL(t.url).origin === new URL(m.page).origin; } catch (e) { return false; } });
    } catch (e) {}
    if (!tab) return fail("the page is not open in the browser any more");
    pendingFetch.add(id);
    try {
      const res = await api.scripting.executeScript({ target: { tabId: tab.id }, func: fetchInPage, args: [id, m.url] });
      const r = res && res[0] && res[0].result;
      if (!r || !r.ok) await fail((r && r.error) || "the page could not fetch it", r && r.status);
    } catch (e) {
      await fail(String((e && e.message) || e));
    } finally {
      pendingFetch.delete(id);
    }
  }

  // A piece from fetchInPage: only for a request the app made, relayed in
  // order (the page waits for each answer).
  async function relayChunk(msg) {
    const id = String(msg.id || "");
    if (!pendingFetch.has(id)) return { ok: false, error: "no such request" };
    try {
      const r = await send({ type: "pageFetchChunk", fetchId: id, chunk: String(msg.chunk || ""), done: !!msg.done });
      return { ok: !!r && r.type === "ok", error: r && r.error };
    } catch (e) { return { ok: false, error: String((e && e.message) || e) }; }
  }

  return {
    start(hostName) {
      host = hostName;
      listen();
      handlers.pageFetch.push(pageFetch);
      connect();
    },
    on(type, fn) { (handlers[type] || (handlers[type] = [])).push(fn); },
    send,
    live: () => live,
    browserContext,
    relayChunk,
  };
})();
