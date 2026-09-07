// Optional WebTransport transport for the reading room. When a signed-in page brings up a
// QUIC/WebTransport connection to cms-server, htmx's `/r/` requests ride the wire instead of
// HTTP — same htmx, same URLs, transport swapped underneath. If the wire isn't up (or drops
// mid-request), htmx falls back to its normal HTTP request. Nothing about the room changes.
//
// The page owns the connection (WebTransport is solid on the main thread; a service worker
// can't read the HttpOnly session cookie and is a poor host for a live QUIC session), and it
// authorizes the connection with the short "wire token" from /auth/status — the one bearer
// value the page is allowed to read — via `urn:auth:resume`. So one HTTP login covers both
// transports. Reuses the existing wasm wire codec; the kernel never runs in the page.
import init, { encodeIssue, decodeReply } from "./ikigai_cms_web.js";

let wt = null;
let ready = false;
let connecting = false;

const hexToBytes = (hex) => {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.substr(i * 2, 2), 16);
  return out;
};

async function readAll(readable) {
  const reader = readable.getReader();
  const chunks = [];
  let n = 0;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    chunks.push(value);
    n += value.length;
  }
  const out = new Uint8Array(n);
  let o = 0;
  for (const c of chunks) {
    out.set(c, o);
    o += c.length;
  }
  return out;
}

// One ikigai-wire Call on its own bidi stream → the decoded reply (a ready-to-swap HTML string).
async function wireCall(bytes) {
  const stream = await wt.createBidirectionalStream();
  const writer = stream.writable.getWriter();
  await writer.write(bytes);
  await writer.close();
  return decodeReply(await readAll(stream.readable));
}

function setIndicator(on) {
  const el = document.getElementById("wire-status");
  if (!el) return;
  el.textContent = on ? "● wire" : "";
  el.title = on ? "resolving over WebTransport" : "";
}

function drop() {
  ready = false;
  wt = null;
  setIndicator(false);
}

// Bring up the wire for the current session (no-op if not signed in or unsupported).
async function connect() {
  if (ready || connecting) return;
  connecting = true;
  try {
    if (typeof WebTransport === "undefined") return; // older browser → stay on HTTP
    const st = await (await fetch("/auth/status")).json();
    if (!st.authenticated || !st.wire_token) return; // only an authenticated page bridges
    const cert = await (await fetch("cert.json", { cache: "no-store" })).json();
    // Where to dial the wire follows the server's bind, and the server's cert SANs follow it
    // too — a dial host the certificate does not name is rejected by the browser. `cert.host`
    // is "127.0.0.1" for the loopback default (what this line used to hard-code); it is absent
    // when the page is reached at some other name (a TLS proxy, or a bind naming no single
    // address), and then the page's own hostname is the only host that can be right.
    const wireHost = cert.host || location.hostname;
    wt = new WebTransport(`https://${wireHost}:${cert.port}`, {
      serverCertificateHashes: [{ algorithm: "sha-256", value: hexToBytes(cert.cert) }],
    });
    await wt.ready;
    // Bridge the HTTP login onto this connection (raises its ceiling to the session's cap).
    // The reply is `{"ok":true}` on success, or `{"error":...}` / an error fragment on failure.
    const resume = await wireCall(
      encodeIssue("urn:auth:resume", JSON.stringify({ token: st.wire_token })),
    );
    let bridged = false;
    try {
      bridged = JSON.parse(resume).ok === true;
    } catch (_) {}
    if (!bridged) {
      try {
        wt.close();
      } catch (_) {}
      drop();
      return;
    }
    ready = true;
    setIndicator(true);
    // If the connection closes, fall back to HTTP for subsequent requests.
    wt.closed.then(drop, drop);
  } catch (_) {
    drop();
  } finally {
    connecting = false;
  }
}

async function disconnect() {
  const c = wt;
  drop();
  try {
    c && c.close();
  } catch (_) {}
}

// Intercept htmx's `/r/` GETs when the wire is up; otherwise let htmx do its normal HTTP request.
document.body.addEventListener("htmx:beforeRequest", (evt) => {
  if (!ready) return;
  const cfg = evt.detail.requestConfig;
  if (!cfg || cfg.verb !== "get" || !String(cfg.path).startsWith("/r/")) return;
  evt.preventDefault(); // cancel htmx's XHR; we answer over the wire
  const { target, elt } = evt.detail;
  const [iriEnc, qs] = cfg.path.replace(/^\/r\//, "").split("?");
  const args = {};
  if (qs) for (const [k, v] of new URLSearchParams(qs)) args[k] = v;
  const p = cfg.parameters;
  if (p) {
    if (typeof p.forEach === "function") p.forEach((v, k) => (args[k] = v));
    else for (const k of Object.keys(p)) args[k] = p[k];
  }
  const swapStyle = (elt.getAttribute("hx-swap") || "innerHTML").split(" ")[0];
  wireCall(encodeIssue(decodeURIComponent(iriEnc), JSON.stringify(args)))
    .then((html) => htmx.swap(target, html, { swapStyle }))
    .catch(() => {
      // Wire failed mid-request → drop to HTTP for this request and everything after it.
      drop();
      const q = new URLSearchParams(args).toString();
      htmx.ajax("GET", "/r/" + iriEnc + (q ? "?" + q : ""), {
        source: elt,
        target,
        swap: swapStyle,
      });
    });
});

// Login/logout transitions from auth.js; on load, try to bring the wire up for an existing session.
document.addEventListener("ikigai:authed", connect);
document.addEventListener("ikigai:deauthed", disconnect);
init().then(connect).catch(() => {});

window.ikigaiWire = { connected: () => ready };
