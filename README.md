# ikigai-cms-web

The **semantic-CMS reading room**: your personal content — bookmarks, notes,
library metadata — as one RDF graph where everything is a tagged, linkable,
queryable resource, served by a kernel and browsed as a query-driven reading
room (a *view is a query*).

Targeted, like `ikigai-web-demo`, at the needs of the CMS: **QUIC/WebTransport-bound,
passkey-authenticated, cap-scoped**. It reuses web-demo's proven plumbing (the
WebTransport kernel server, the WASM wire client, the passkey ceremony) and adds a
CMS server, the reading-room UI, and a server-verified relying party.

## Rungs

1. **The kernel spine** (`build_cms_kernel`) — *this crate, today.* Composes the CMS
   source files (jailed, read through the kernel), the assembled bookmark graph
   (`urn:cms:graph`, org → Turtle via the `ikigai-cms` transreptor), and SPARQL over
   it (`urn:sparql:*`). A `SELECT` over `graph=urn:cms:graph` returns the tagged
   bookmarks — proven by the integration tests.
2. **WebTransport server** (`cms-server`) — *this crate, now.* `cargo run --features
   server --bin cms-server -- [port] [src_dir]` serves `build_cms_kernel` over
   WebTransport (HTTP/3 over QUIC), speaking the `ikigai-wire` `Call`/`Reply` protocol
   — the same bytes `ikigai-ipc`/`ikigai-quic` speak. SPARQL over the graph runs
   server-side; the browser renders the result. Plus **`urn:cms:view:{tag}`** — a view
   renders to an htmx HTML fragment of cards (a view is a query). Plus the **browser
   shell** (`dist/index.html`): a WASM wire codec (`encodeIssue`/`decodeReply`) + the
   WebTransport transport + a minimal htmx, restylable with three stylesheets. *Then:*
   swap the Rust render template for XSLT stylesheet resources (`urn:cms:style:*`).
3. **Passkey gate** (`webauthn-rs` relying party) — *this crate, now.* The room is gated
   by a verified passkey that raises the connection's capability ceiling.
4. WebGPU view (a `<cms-graph>` web component; view = query, SHACL-shape renderers).

## Run the reading room

```sh
# 1. build the browser wire codec (once, or after changing src/wire_client.rs)
./build-wasm.sh

# 2. start the server — it serves the page (dist/) AND the WebTransport wire, one process
cargo run --features server --bin cms-server -- 4433 ~/Dropbox/org-mode-files

# 3. open the reading room (the server prints the URL):
#    http://localhost:8080          (set CMS_PORT to change the page port)
```

The `4433` positional arg is the internal WebTransport port (the page reads it from
`cert.json`); `CMS_PORT` (default 8080) is the page URL you open. The RP origin defaults
to that page origin, so the passkey can't drift from the URL you open.

The page opens a WebTransport connection to `cms-server`. **The room is gated by a
passkey** (rung 3): the server resolves under a public ceiling until a verified passkey
raises it, so on first run click **register passkey** (Touch ID) to enrol, then **sign
in** — the WebAuthn ceremony rides over the wire as `urn:auth:*`, the server (a
`webauthn-rs` relying party) verifies it and mints the connection's capability. After
sign-in the reading room loads; clicking a tag chip re-queries the graph. The passkey
store persists through the OS keystore — the **macOS Keychain** (via `ikigai-secret`), a
dev file store elsewhere — not a plaintext file. The RP origin defaults to
`http://localhost:8080` (override with `CMS_RP_ORIGIN`/`CMS_RP_ID`).

Needs a WebTransport browser: Chrome/Edge or Safari 26.4+ (any browser once WebTransport
went Baseline in March 2026 — but the local page uses `serverCertificateHashes` to trust
the self-signed cert, and Firefox's support for that self-signed path lags, so it may not
connect locally; with a real CA cert in production, drop `serverCertificateHashes` and all
of them work).

## The render pipeline (target)

```
view (SPARQL) → CONSTRUCT (align/shape meaning) → RDF/XML → XSLT (type→card) → htmx
```

CONSTRUCT shapes meaning; XSLT shapes pixels; htmx delivers hypermedia; web
components are the interactive islands (the WebGPU graph).

## Status

Rung 1 only. Native kernel library; the WebTransport server and browser front-end
land in the next rungs.
