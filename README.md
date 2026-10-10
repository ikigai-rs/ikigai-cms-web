# ikigai-cms-web

The **semantic-CMS reading room**: your personal content — bookmarks, notes,
library metadata — as one RDF graph where everything is a tagged, linkable,
queryable resource, served by a kernel and browsed as a query-driven reading
room (a *view is a query*).

Targeted, like `ikigai-web-demo`, at the needs of the CMS: **QUIC/WebTransport-bound,
passkey-authenticated, cap-scoped**. It reuses web-demo's proven plumbing (the
WebTransport kernel server, the WASM wire client, the passkey ceremony) and adds a
CMS server, the reading-room UI, and a server-verified relying party.

## What it composes, in three layers

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

## Install as a service

On a Mac the room runs under launchd as `dev.ikigai-rs.cms`, from the installed binary in
`~/.cargo/bin`, never a `target/` build. Install (or reinstall) it from this checkout:

```sh
cargo install --path . --features server --locked --force
```

Every binary in this crate sits behind a required feature, so an install without
`--features server` compiles the whole graph, warns that no binaries are available, and
exits 0 having installed nothing. `--locked` builds the graph the committed `Cargo.lock`
froze instead of re-resolving it against today's registry.

The plist is versioned here, at
[`deploy/macos/dev.ikigai-rs.cms.plist`](deploy/macos/dev.ikigai-rs.cms.plist); launchd
loads its copy from `~/Library/LaunchAgents/`. After an install, re-register the agent
(on Brian's machines: `just -f ~/git-personal/ikigai-devtools/justfile reregister --only
dev.ikigai-rs.cms`), which waits between the bootout and the bootstrap. Do it with someone
at the screen: a replaced binary can re-prompt for the Keychain (the passkey store) and the
source jail under `~/Dropbox`, and a denial is silent until `/tmp/ikigai-cms.log` says so.

The browser codec in `dist/` does not need rebuilding for a dependency bump unless
`ikigai-wire` moved; `./build-wasm.sh` rebuilds it when it did.

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
bind AND a plain-http loopback `rp_origin`; off loopback, or behind a proxy origin, it would
serve the whole room to the network.

Whatever the arrangement, the page face answers only for its own origin: a request whose
`Host` names another host (DNS rebinding), or whose `Origin` is not the origin its `Host`
names (a cross-site form, another localhost port), is refused with 403 before it is routed.
A loopback `rp_origin` admits any loopback name on any port, so a tunnel that remaps the
port still works. Every response carries `X-Content-Type-Options: nosniff`, and `/r/` serves
each body as its own media type (a graph is `text/turtle`, never HTML).

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
passkey** (layer 3 above): the server resolves under a public ceiling until a verified passkey
raises it, so on first run click **register passkey** (Touch ID) to enroll, then **sign
in** — the WebAuthn ceremony rides over the wire as `urn:auth:*`, the server (a
`webauthn-rs` relying party) verifies it and mints the connection's capability. After
sign-in the reading room loads; clicking a tag chip re-queries the graph. The passkey
store persists through the OS keystore — the **macOS Keychain** (via `ikigai-secret`), a
dev file store elsewhere — not a plaintext file. The RP origin defaults to
`http://localhost:{page_port}` (override with `rp_origin`/`rp_id`).

What the public ceiling reaches, over either face, is a closed list: the embedded
stylesheets, the pure compute endpoints (SPARQL, XSLT, the org transreptor) and the kernel's
own manifold and validator, all of which read only what the caller hands them or could read
directly. Every graph, every tag and link overlay, the link-check status, the review and the
purge prompts refuse it, and every endpoint refuses a verb it does not declare. The
`server_tests/privacy.rs` walk holds this: it lists the public set by name and fails on any
other binding a signed-out visitor is not refused on.

### The anonymous SPARQL budget

Because SPARQL is on that public list, one signed-out request could otherwise pin a core for
minutes: a three-way cross product of 100-row `VALUES` tables (no graph needed, about 1.3 KB of
query) answered 272 MB after 11 s, and a long property path ran over 10 minutes.
So every `urn:sparql:*` request from a caller **with no signed-in session** — over `/r/` and the
wire alike, `dev_open`'s unauthenticated localhost included — leaves the door carrying
`budget=` **1000 ms**, and a query that runs past it is refused with ikigai-sparql's typed
timeout, never a partial answer. On the HTTP face that refusal renders like every other
resolution error: `200 OK` with a `cms-error` fragment naming the budget (the wire returns it as
the reply's error). A caller's own `budget=` is kept only when it is a smaller whole number of
milliseconds; a larger one, `budget=600000` say, still stops at the door's.

Change it in `cms.toml` (or for one run with `--anonymous-sparql-budget-ms <ms>`):

```toml
anonymous_sparql_budget_ms = 1000   # 1 to 5000; the server prints the value it applies
```

The budget is stamped at the door rather than set as the SPARQL space's ceiling because the
room's own views, tag overlays and maintenance passes query through the same kernel. Those, and
signed-in callers, run under the space's own 5 s ceiling (`ikigai_sparql::budget::DEFAULT_BUDGET`),
which is also why a value above 5000 is refused: it would never apply. Measured on a copy of the
live library (2.1 MB of bookmarks, the 4.4 MB Zotero export, release build), the room's heaviest
read is a cold tag view at ~170 ms, the book CONSTRUCT ~100 ms.

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

## Conformance

The kernel passes [`ikigai-conformance`](https://crates.io/crates/ikigai-conformance)
(`tests/conformance.rs`): every input typed, every face declared, every Sink taking
`content` and declaring `urn:cap:fs:write:*`, every jail-derived Source declaring
`urn:cap:fs:read:*` — so a caller holding no fs grant is refused with a typed `Denied`
at the view, by the kernel, not three sub-resolutions down. The only findings left are
NAMES (the ids are full IRIs, held for the ecosystem-wide rename), and the test holds the
composed endpoints (`ikigai-fs`, `ikigai-sparql`, `ikigai-xslt`, `ikigai-cms`, and in the
maintenance kernel `ikigai-http`, `ikigai-llm`, `ikigai-secret`) to the same: each one the
suite cannot call unaided carries a fixture, so the walk probes it rather than recording a
failed call.

The room coins one vocabulary, **`cms:`** = `https://ikigai-rs.dev/ns/cms#`, beside
Dublin Core Elements 1.1: the kinds `cms:Book`, `cms:Bookmark`, `cms:Presentation`;
`cms:isbn` and `cms:kind` on a card; the overlay predicates `cms:suggestedTag`,
`cms:dismissedTag`, `cms:zoteroItem`, `cms:zoteroAttachment`, `cms:readerUrl`. Every
subject is a skolem IRI (`urn:cms:bookmark:{fnv}`, `urn:cms:book:{sha256}`,
`urn:cms:presentation:{slug}`); no face emits a blank node.

Caching is the room's one performance contract, and the test pins it timing-free: the
expensive parses (`urn:cms:graph:books`, `urn:cms:graph:presentations`) are cached under
the golden threads of the files they read — a `urn:kernel:cut urn:cms:src:zotero`
recomputes the books after a re-export — while `urn:cms:graph`, the union, is live BY
DESIGN: it is where the tag overlays join. Joining an overlay into a cached part instead
is the ~2000× regression this room once shipped, and it is now a red test.

`urn:cms:graph:bookmarks` is live too, on purpose: the bookmarks org file is written
outside the kernel (org-capture, a Dropbox sync) and nothing watches it, so a cached copy
was both stale and destructive (a purge struck it and wrote it back over the edit). Reading
it fresh costs ~10 ms per read of a 2.1 MB file; a watcher that cuts `urn:cms:src:{path}`
would earn the cache back.

## Status

**All three layers ship.** The kernel spine is the library; `cms-server` (the `server`
feature) serves it over WebTransport and hosts the page; the passkey gate is a
`webauthn-rs` relying party that raises the connection's capability ceiling.

To run it you need the WASM wire codec built into `dist/` (step 1 of "Run the reading
room"), a `cms.toml` naming `src_dir` and `dist`, and a passkey enrolled on first use.
The page binds loopback, and moving it off loopback is refused unless a TLS terminator
fronts the room — see "Reaching the room from another machine" for why that is
arithmetic rather than caution.

### 2026-10-08: the audit releases

- **Dependencies.** `ikigai-llm` 0.13 (audit round 5: ports enforced, keys sent, URL
  credentials redacted, live probes no longer cached), `ikigai-cms` 0.1.4 (every heading is a
  record boundary; org link escapes removed), `ikigai-secret` 0.1.8, `ikigai-time` 0.4 (a
  scheduled pass now states the authority it fires under), `ikigai-conformance` 0.5. Three of
  those pins were ceilings under Cargo's 0.x caret rules and could not see the release.
- **Removing a link matches the graph.** Remove-by-URL used its own reading of an org
  heading, so an escaped or wrapped link target, or an indented heading, could not be removed,
  and a heading of prose about a link could. It now asks `ikigai-cms` which URL a heading
  carries, the same `dc:identifier` the room shows.
- **MSRV declared** (`rust-version = "1.89"`, for `File::lock`), so CI checks it.
- **The launchd plist is versioned** in `deploy/macos/`, with its install pointer fixed and
  its comment well-formed XML.

Reinstall: `cargo install --path . --features server --locked --force` from this checkout,
then re-register `dev.ikigai-rs.cms` (see "Install as a service").

### 2026-10-09: a time budget on the anonymous door

- **`ikigai-sparql` 0.1.12**: every evaluation runs under a time budget (5 s by default) and a
  query's algebra is bounded before planning.
- **Signed-out SPARQL gets 1 s** (`anonymous_sparql_budget_ms`, see "The anonymous SPARQL
  budget"), on both faces, and a caller's `budget=` can only lower it.

Reinstall the same way: `cargo install --path . --features server --locked --force`, then
re-register `dev.ikigai-rs.cms`. No `cms.toml` change is needed for the 1 s default.
