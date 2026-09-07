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
cargo run --features server --bin cms-server

# 3. open the reading room (the server prints the URL):
#    http://localhost:8080          (set page_port to change the page port)
```

Configuration is the config home plus CLI flags — **never environment variables**:
`~/.config/ikigai/cms.toml` states the durable posture, a flag (`--help` lists them)
overrides it for one run, and a config file that doesn't parse fails loud. Example:

```toml
# ~/.config/ikigai/cms.toml
page_port = 8090                  # the URL you open (default 8080)
bind      = "127.0.0.1"           # where the page listens (default; see below)
wire_port = 4434                  # internal WebTransport port (default 4433)
src_dir   = "~/Dropbox/org-mode-files"
bookmarks = "bookmarks-src.org"   # sub-path under src_dir
dist      = "~/git-personal/ikigai-cms-web/dist"
llm_provider = "mlx"              # which ~/.config/ikigai/llm.json provider the
                                  # maintenance passes use (default: its default)
```

`wire_port` is internal (the page reads it from `cert.json`); `page_port` is the page
URL you open. The RP origin defaults to that page origin, so the passkey can't drift
from the URL you open.

### Reaching the room from another machine

`bind` is where the page listens; it defaults to `127.0.0.1`, and **moving it off
loopback is refused unless a TLS terminator fronts the room.** That is not caution, it
is arithmetic: `http://localhost` is a **secure context** and `http://192.168.1.20` is
not, WebAuthn runs *only* in a secure context, so a LAN-bound room over plain HTTP has a
passkey gate that cannot function at all — nobody can register, nobody can sign in. The
server refuses to start rather than look healthy until the first sign-in attempt.

| `bind` | `rp_origin` | |
|---|---|---|
| loopback | `http://localhost:{page_port}` | legal — the default, and the ssh-tunnel deployment |
| loopback | `https://…` | legal — a TLS reverse proxy on this host |
| non-loopback | `https://…` | legal, with a startup warning: only the proxy's origin works |
| non-loopback | `http://…` | **refused** |
| any | `http://<non-loopback-host>` | **refused** — that origin is not a secure context |

Plus `dev_open` (which ungates the HTTP face entirely — no passkey) requires a loopback
bind; off loopback it would serve the whole room to the network.

So two arrangements actually reach the room from elsewhere, and they are not equivalent:

1. **SSH tunnel — keeps your passkeys.** Leave `bind = "127.0.0.1"` and forward the page
   port: `ssh -N -L 8080:127.0.0.1:8080 <host>`. The browser still sees
   `http://localhost:8080`, so `rp_id`/`rp_origin` don't change and no credential is
   disturbed. (The WebTransport wire is QUIC/UDP and does *not* ride an `ssh -L` tunnel;
   the HTTP face serves the whole reading room on its own, just without the wire.)
2. **Reverse proxy terminating TLS — a one-way door.** Set `rp_id` and `rp_origin` to the
   proxy's host. ⚠ A passkey is bound to its `rp_id`: changing it **invalidates every
   enrolled passkey**, and each must be re-enrolled against the new origin. There is no
   migration. Choose the hostname once. (This server never terminates TLS itself.)

The WebTransport certificate's SANs are **derived** from `bind` and `rp_origin` (plus
anything `cert_sans` adds), so they cannot drift from the address the browser dials — a
cert that doesn't name the dialed host is rejected by the browser, and that failure shows
up nowhere near the config that caused it. The default derives exactly the
`["localhost", "127.0.0.1", "::1"]` that used to be compiled in.

The page opens a WebTransport connection to `cms-server`. **The room is gated by a
passkey** (rung 3): the server resolves under a public ceiling until a verified passkey
raises it, so on first run click **register passkey** (Touch ID) to enrol, then **sign
in** — the WebAuthn ceremony rides over the wire as `urn:auth:*`, the server (a
`webauthn-rs` relying party) verifies it and mints the connection's capability. After
sign-in the reading room loads; clicking a tag chip re-queries the graph. The passkey
store persists through the OS keystore — the **macOS Keychain** (via `ikigai-secret`), a
dev file store elsewhere — not a plaintext file. The RP origin defaults to
`http://localhost:{page_port}` (override with `rp_origin`/`rp_id`).

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
