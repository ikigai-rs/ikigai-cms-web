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

## Run the reading room

```sh
# 1. build the browser wire codec (once, or after changing src/wire_client.rs)
./build-wasm.sh

# 2. start the kernel server over WebTransport (prints a cert sha-256)
cargo run --features server --bin cms-server -- 4433 ~/Dropbox/org-mode-files

# 3. serve dist/ (any static server) and open index.html with the printed hash:
#    file: python3 -m http.server --directory dist 8080
#    then: http://localhost:8080/#cert=<the printed sha-256>
```

The page opens a WebTransport connection to `cms-server`, resolves
`urn:cms:view:quic` over the wire, and swaps the returned HTML in; clicking a tag chip
re-queries the graph. Needs a WebTransport browser: Chrome/Edge or Safari 26.4+ (any
browser once WebTransport went Baseline in March 2026 — but the local page uses
`serverCertificateHashes` to trust the self-signed cert, and Firefox's support for
that self-signed path lags, so it may not connect locally; with a real CA cert in
production, drop `serverCertificateHashes` and all of them work). Auth is deferred to
rung 3 — the server resolves under root, so run it on a trusted host.
3. Server-verified passkey (relying party) → cap-scoped views; cap-on-entry.
4. WebGPU view (a `<cms-graph>` web component; view = query, SHACL-shape renderers).

## The render pipeline (target)

```
view (SPARQL) → CONSTRUCT (align/shape meaning) → RDF/XML → XSLT (type→card) → htmx
```

CONSTRUCT shapes meaning; XSLT shapes pixels; htmx delivers hypermedia; web
components are the interactive islands (the WebGPU graph).

## Status

Rung 1 only. Native kernel library; the WebTransport server and browser front-end
land in the next rungs.
