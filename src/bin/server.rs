//! A WebTransport server for the CMS reading room, with a passkey relying party.
//!
//! Serves [`ikigai_cms_web::build_cms_kernel`] over WebTransport (HTTP/3 over QUIC),
//! speaking the `ikigai-wire` `Call`/`Reply` protocol. Each connection carries a
//! **ceiling** capability: it starts *public* (grants nothing — the whole graph is
//! behind the fs-read cap, so the room is gated), and a verified passkey raises it to
//! the credential's entitlement (rung 3a's `dispatch` clamp then enforces it).
//!
//! The same reading room is also served over plain **HTTP** on the page port (so vanilla htmx
//! can drive it): `/r/{iri}?args` resolves a fragment under the caller's session capability,
//! `/auth/*` runs the passkey ceremony, and a `cms_session` cookie carries the granted
//! entitlement between requests. Both faces reuse the one transport-agnostic [`Rp`], so the room
//! is passkey-gated identically over the wire and over HTTP (`CMS_DEV_OPEN=1` ungates the HTTP
//! face for localhost dev).
//!
//! The WebAuthn ceremony binds to the *page* origin (where `navigator.credentials` runs, default
//! `http://localhost:8080`), configurable via `CMS_RP_ID` / `CMS_RP_ORIGIN`; the
//! `{Passkey → scopes}` store persists through the OS keystore (macOS Keychain via
//! `ikigai-secret`), not a plaintext file.
//!
//! Run: `cargo run --features server --bin cms-server -- [port] [src_dir]`

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use uuid::Uuid;

use ikigai_core::{ArgRef, Capability, Iri, Kernel, ReprType, Representation, Request, Verb};
use ikigai_resolve::{CacheStatus, Resolver};
use ikigai_wire::{decode, encode, Call, Reply};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use webauthn_rs::prelude::{PasskeyAuthentication, PasskeyRegistration};
use wtransport::endpoint::IncomingSession;
use wtransport::{Endpoint, Identity, ServerConfig};

use ikigai_cms_web::session::{Recent, RecentLog, Rp};

/// Largest `Call` we'll read off a stream — a guard against a runaway client.
const MAX_CALL: usize = 8 * 1024 * 1024;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(4433);
    let src_dir: PathBuf = args
        .next()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CMS_SRC_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join("Dropbox/org-mode-files"))
                .unwrap_or_default()
        });

    // The reading-room PAGE port — cms-server serves `dist/` here itself (no separate
    // static server). CMS_PORT overrides; default 8080. This is the URL you open in the
    // browser. The WebTransport port (positional arg 1, 4433) is internal — the page reads
    // it from cert.json and connects there.
    let page_port: u16 = std::env::var("CMS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);

    // The entitlement a verified passkey is granted: read the CMS source jail (the whole
    // room's chain bottoms out in this fs read). The presentations dir is appended once
    // resolved (below), so decks — read through `urn:cms:deck:*` — are cap-gated too.
    let mut entitlement = vec![format!("urn:cap:fs:read:{}", src_dir.display())];

    // The relying party. rp_id + page origin default to local dev; the passkey store
    // persists through the OS keystore (macOS Keychain), not a plaintext file.
    let rp_id = std::env::var("CMS_RP_ID").unwrap_or_else(|_| "localhost".to_string());
    // Default the passkey relying-party origin to the page we serve — same host+port — so it
    // cannot drift from the URL you actually open (the #1 sign-in failure). Override only for
    // a non-localhost deployment.
    let rp_origin =
        std::env::var("CMS_RP_ORIGIN").unwrap_or_else(|_| format!("http://localhost:{page_port}"));
    let rp = Arc::new(Rp::new(
        &rp_id,
        &rp_origin,
        ikigai_secret::default_backend(),
    )?);

    let identity = Identity::self_signed(["localhost", "127.0.0.1", "::1"])?;
    let cert_hash = identity.certificate_chain().as_slice()[0].hash();
    let hash_hex: String = cert_hash
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    println!("ikigai reading room  →  http://localhost:{page_port}   (open this in Chrome/Edge)");
    println!("  (webtransport on https://127.0.0.1:{port} — internal; the page connects there)");
    println!("source jail: {}", src_dir.display());
    println!(
        "relying party: {rp_origin} (rp_id {rp_id}){}",
        if rp.is_enrolled() {
            ""
        } else {
            "  — no passkey enrolled yet; register one from the page"
        }
    );
    println!("cert sha-256: {hash_hex}");
    // Write the cert hash where the page can fetch it, so no one pastes `#cert=` by hand.
    // The static server serving `dist/` serves this too; the page reads `cert.json` on
    // load and connects automatically. `#cert=` in the URL still overrides it.
    let dist = std::env::var_os("CMS_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("dist"));
    let cert_json = serde_json::json!({ "cert": hash_hex, "port": port }).to_string();
    match std::fs::write(dist.join("cert.json"), cert_json) {
        Ok(()) => println!(
            "wrote {}/cert.json — open the page (served from dist/) and it connects automatically",
            dist.display()
        ),
        Err(e) => println!(
            "note: couldn't write {}/cert.json ({e}); open the page with #cert={hash_hex}",
            dist.display()
        ),
    }

    // The Zotero library (books): CMS_ZOTERO overrides; else the default location. Only
    // used if the file exists — otherwise the graph is bookmarks-only.
    let zotero: Option<PathBuf> = std::env::var_os("CMS_ZOTERO")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join("Dropbox/Documents/Zotero/My Library.rdf"))
        })
        .filter(|p| p.exists());
    println!(
        "zotero library: {}",
        zotero
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none — bookmarks only)".to_string())
    );

    // Lectern presentations (decks): CMS_PRESENTATIONS overrides the root; else the default
    // repo. Only used if the directory exists. CMS_DECK_BASE is the base URL a static server
    // exposes that tree at, so a presentation card's link opens the built deck; default
    // `http://localhost:8000` (serve the decks with e.g. `python3 -m http.server 8000` in
    // the presentations dir). Set CMS_DECK_BASE="" for locatable-but-unclickable file:// links.
    let presentations = std::env::var_os("CMS_PRESENTATIONS")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join("git-personal/lectern-presentations"))
        })
        .filter(|p| p.is_dir())
        .map(|root| {
            let base = std::env::var("CMS_DECK_BASE")
                .unwrap_or_else(|_| "http://localhost:8000".to_string());
            ikigai_cms_web::Presentations {
                root,
                base_url: (!base.is_empty()).then_some(base),
            }
        });
    println!(
        "presentations: {}",
        presentations
            .as_ref()
            .map(|p| format!(
                "{} → {}",
                p.root.display(),
                p.base_url.as_deref().unwrap_or("file:// links")
            ))
            .unwrap_or_else(|| "(none)".to_string())
    );

    if let Some(cfg) = &presentations {
        entitlement.push(format!("urn:cap:fs:read:{}", cfg.root.display()));
    }
    let entitlement = Arc::new(entitlement);

    // The bookmarks org file, as a sub-path relative to the source jail (CMS_SRC_DIR).
    // CMS_BOOKMARKS overrides it; unset uses the built-in default (pinboard-bookmarks.org).
    let bookmarks = std::env::var("CMS_BOOKMARKS").ok();
    println!(
        "bookmarks: {}",
        bookmarks
            .as_deref()
            .unwrap_or("(default: old-org/pinboard-bookmarks.org)")
    );

    // Cloned for the optional maintenance kernel (below), before the serving kernel consumes them.
    let src_dir_maint = src_dir.clone();
    let bookmarks_maint = bookmarks.clone();
    let kernel = Arc::new(ikigai_cms_web::build_cms_kernel_with(
        src_dir,
        zotero,
        presentations,
        bookmarks,
    ));
    // The recency trail, shared across connections and keyed per passkey identity.
    let recent = Arc::new(RecentLog::default());

    // The HTTP session table: the passkey ceremony + cookie sessions for the HTTP face, so the
    // room is passkey-gated over HTTP just as it is over the wire (both reuse the same `rp`).
    let http_auth = Arc::new(HttpAuth::default());

    // Serve the reading-room page (dist/), an HTTP resolve face (`/r/{iri}`), AND the passkey
    // ceremony (`/auth/*`) ourselves — no separate `python3 -m http.server`, the same resolution
    // the wire does, over plain HTTP (so vanilla htmx can drive it). A finished login grants the
    // session the room entitlement; unauthenticated requests resolve public (the gated graph →
    // nothing). CMS_DEV_OPEN=1 elevates the HTTP face to the full entitlement without login
    // (localhost dev only); the wire stays passkey-gated regardless. Default off.
    let dev_open = std::env::var("CMS_DEV_OPEN").as_deref() == Ok("1");
    if dev_open {
        println!("HTTP resolve face: /r/{{iri}} — DEV-OPEN (ungated; localhost only)");
    }
    tokio::spawn(serve_http(
        dist,
        page_port,
        Arc::clone(&kernel),
        Arc::clone(&rp),
        Arc::clone(&http_auth),
        Arc::clone(&recent),
        Arc::clone(&entitlement),
        dev_open,
    ));

    // Optionally run the link-check maintenance pass on a recurring `urn:time` job (CMS_LINKCHECK=1,
    // default off). The pass is `urn:cms:linkcheck` on a dedicated maintenance kernel (the CMS
    // graph + outbound HTTP); the time transport fires it daily, and we fire once on startup in the
    // background so a fresh start doesn't wait a day. The registry is held for the process lifetime
    // (the accept loop below never returns), which keeps its timer thread alive.
    let _linkcheck = if std::env::var("CMS_LINKCHECK").as_deref() == Ok("1") {
        let status_path = std::env::var("CMS_LINKSTATUS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| ikigai_cms_web::maintenance::default_status_path());
        println!(
            "link-check: enabled (daily); status {}",
            status_path.display()
        );
        let maint = Arc::new(ikigai_cms_web::maintenance::build_maintenance_kernel(
            src_dir_maint,
            bookmarks_maint,
            status_path,
        ));
        let registry = ikigai_time::JobRegistry::new(Arc::new(ikigai_time::ThreadTimer))
            .with_capability(Capability::root());
        registry.set_resolver(Arc::clone(&maint) as Arc<dyn Resolver>);
        match ikigai_time::parse_schedule("24h").and_then(|s| {
            registry.schedule_persistent("urn:cms:linkcheck".into(), Verb::Source, s, true)
        }) {
            Ok(id) => println!("link-check: scheduled daily (job {id})"),
            Err(e) => eprintln!("link-check: schedule failed: {e}"),
        }
        tokio::spawn(async move {
            let req = Request::new(
                Verb::Source,
                Iri::parse("urn:cms:linkcheck").expect("valid IRI"),
            );
            // The async, under-capability resolve (no block_on) — so the pass's fan-out parks.
            match maint.issue_as_async(req, &Capability::root()).await {
                Ok((r, _)) => {
                    println!(
                        "link-check (startup): {}",
                        String::from_utf8_lossy(&r.bytes)
                    )
                }
                Err(e) => eprintln!("link-check (startup): {e}"),
            }
        });
        Some(registry)
    } else {
        None
    };

    let config = ServerConfig::builder()
        .with_bind_default(port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();
    let server = Endpoint::server(config)?;

    loop {
        let incoming = server.accept().await;
        let kernel = Arc::clone(&kernel);
        let rp = Arc::clone(&rp);
        let http_auth = Arc::clone(&http_auth);
        let entitlement = Arc::clone(&entitlement);
        let recent = Arc::clone(&recent);
        tokio::spawn(async move {
            if let Err(e) = serve(incoming, kernel, rp, http_auth, entitlement, recent).await {
                eprintln!("session ended: {e}");
            }
        });
    }
}

/// One HTTP session, keyed by the `cms_session` cookie. It starts unauthenticated — it may
/// hold only an in-flight passkey ceremony — and a finished login fills in `scopes` (the
/// granted entitlement the room resolves under) and the `principal` the recency trail is keyed
/// to. In memory only: a server restart clears every session (re-login), which suits a
/// short-lived single-user room. The HTTP analogue of the wire's per-connection [`Session`].
#[derive(Default)]
struct HttpSession {
    scopes: Option<Vec<String>>,
    principal: Option<String>,
    pending: Option<Pending>,
    /// A bearer token the page can read (unlike the HttpOnly cookie) and present over the wire
    /// (`urn:auth:resume`) to raise a WebTransport connection to this session's capability — so
    /// one HTTP login covers both transports. Minted lazily on `status` once authenticated.
    wire_token: Option<String>,
}

/// A WebAuthn ceremony held between its `start` and `finish` HTTP requests (the wire holds the
/// equivalent on the connection; HTTP is stateless, so it lives in the session table instead).
enum Pending {
    Login(PasskeyAuthentication),
    Register(PasskeyRegistration),
}

/// The HTTP session table: `cms_session` cookie → [`HttpSession`]. A single-user reading room,
/// so lock contention is nil; the mutex is held only for the brief lookup/mutate, never across
/// a resolve or socket I/O.
#[derive(Default)]
struct HttpAuth {
    sessions: Mutex<HashMap<String, HttpSession>>,
}

/// The three things we need off a request line + headers: the method, the target, the cookie
/// jar, and the body length. Everything else is ignored.
struct ReqHead {
    method: String,
    target: String,
    cookies: HashMap<String, String>,
    content_length: usize,
}

/// Parse the request head (everything before the blank line) into a [`ReqHead`]. Header names
/// are matched case-insensitively; only `Cookie` and `Content-Length` are read.
fn parse_head(head: &str) -> ReqHead {
    let mut lines = head.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut cookies = HashMap::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some(v) = header_val(line, "cookie") {
            for kv in v.split(';') {
                if let Some((k, val)) = kv.trim().split_once('=') {
                    cookies.insert(k.trim().to_string(), val.trim().to_string());
                }
            }
        } else if let Some(v) = header_val(line, "content-length") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    ReqHead {
        method,
        target,
        cookies,
        content_length,
    }
}

/// The value of header `name` on `line` (`Name: value`), or `None` if it's a different header.
fn header_val<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let (k, v) = line.split_once(':')?;
    k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
}

/// The capability a `/r/` request resolves under, plus the principal to attribute its recency
/// to. An authenticated session grants its stored scopes; otherwise the room is public (empty
/// cap → the gated graph resolves to nothing), unless `dev_open` elevates localhost dev to the
/// full entitlement. The lock is released before the caller resolves.
fn session_cap(
    auth: &HttpAuth,
    sid: Option<&str>,
    entitlement: &[String],
    dev_open: bool,
) -> (Capability, Option<String>) {
    if let Some(sid) = sid {
        let sessions = auth.sessions.lock().unwrap();
        if let Some(sess) = sessions.get(sid) {
            if let Some(scopes) = &sess.scopes {
                return (Capability::scoped(scopes.clone()), sess.principal.clone());
            }
        }
    }
    if dev_open {
        (Capability::scoped(entitlement.to_vec()), None)
    } else {
        (Capability::scoped(Vec::<String>::new()), None)
    }
}

/// The `Set-Cookie` value for the session cookie. `HttpOnly` (no JS access) + `SameSite=Strict`
/// (only same-origin requests carry it). No `Secure` — the room is served over `http://localhost`
/// (a secure context, but not HTTPS); add `Secure` once it moves behind TLS/WebTransport.
fn session_cookie(id: &str) -> String {
    format!("cms_session={id}; Path=/; HttpOnly; SameSite=Strict")
}

/// The passkey ceremony over HTTP: `POST /auth/{login,register}/{start,finish}`, `POST
/// /auth/logout`, `GET /auth/status`. Reuses the transport-agnostic [`Rp`] (same verifier, same
/// Touch-ID enroll gate as the wire). The in-flight ceremony state lives in the session table,
/// keyed by the `cms_session` cookie minted on `start`; a finished login stores the granted
/// entitlement there. Returns `(status, content-type, body, Set-Cookie?)`.
#[allow(clippy::too_many_arguments)]
fn http_auth(
    route: &str,
    method: &str,
    body: &[u8],
    sid: Option<&str>,
    rp: &Rp,
    auth: &HttpAuth,
    entitlement: &[String],
    dev_open: bool,
) -> (&'static str, &'static str, Vec<u8>, Option<String>) {
    let json = "application/json; charset=utf-8";
    let err = |m: &str| {
        (
            "200 OK",
            json,
            serde_json::json!({ "error": m }).to_string().into_bytes(),
            None,
        )
    };
    // `status` is a GET; every other auth route is a POST.
    if route == "status" {
        // Mint the wire token lazily the first time an authenticated session asks its status,
        // so the page can bring the WebTransport connection up to the same capability.
        let (authed, principal, wire_token) = match sid {
            Some(sid) => {
                let mut sessions = auth.sessions.lock().unwrap();
                match sessions.get_mut(sid) {
                    Some(s) if s.scopes.is_some() => {
                        if s.wire_token.is_none() {
                            s.wire_token = Some(Uuid::new_v4().to_string());
                        }
                        (true, s.principal.clone(), s.wire_token.clone())
                    }
                    _ => (false, None, None),
                }
            }
            None => (false, None, None),
        };
        let body = serde_json::json!({
            "authenticated": authed,
            "principal": principal,
            "enrolled": rp.is_enrolled(),
            "dev_open": dev_open,
            "wire_token": wire_token,
        });
        return ("200 OK", json, body.to_string().into_bytes(), None);
    }
    if method != "POST" {
        return ("405 Method Not Allowed", json, b"{}".to_vec(), None);
    }
    match route {
        "login/start" => match rp.login_start() {
            Ok((challenge, state)) => {
                let (id, set) = ensure_session(auth, sid);
                auth.sessions.lock().unwrap().get_mut(&id).unwrap().pending =
                    Some(Pending::Login(state));
                ("200 OK", json, challenge.into_bytes(), set)
            }
            Err(e) => err(&e),
        },
        "login/finish" => {
            let Some(sid) = sid else {
                return err("no session");
            };
            let state = {
                let mut sessions = auth.sessions.lock().unwrap();
                match sessions.get_mut(sid).and_then(|s| s.pending.take()) {
                    Some(Pending::Login(state)) => state,
                    other => {
                        // put a non-login pending back so a concurrent register isn't lost
                        if let (Some(s), Some(p)) = (sessions.get_mut(sid), other) {
                            s.pending = Some(p);
                        }
                        return err("no login in progress");
                    }
                }
            };
            match rp.login_finish(body, &state) {
                // Grant the CURRENT server entitlement (not the scopes frozen at registration):
                // the room-wide grant grows as sources are added, and this is a single-user room.
                Ok((_stored, principal)) => {
                    let mut sessions = auth.sessions.lock().unwrap();
                    if let Some(s) = sessions.get_mut(sid) {
                        s.scopes = Some(entitlement.to_vec());
                        s.principal = Some(principal);
                    }
                    ("200 OK", json, br#"{"ok":true}"#.to_vec(), None)
                }
                Err(e) => err(&e),
            }
        }
        "register/start" => {
            if rp.is_enrolled() {
                return err("registration closed: a passkey is already enrolled");
            }
            let name = serde_json::from_slice::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v["name"].as_str().map(str::to_string))
                .unwrap_or_else(|| "ikigai".to_string());
            match rp.register_start(&name) {
                Ok((challenge, state)) => {
                    let (id, set) = ensure_session(auth, sid);
                    auth.sessions.lock().unwrap().get_mut(&id).unwrap().pending =
                        Some(Pending::Register(state));
                    ("200 OK", json, challenge.into_bytes(), set)
                }
                Err(e) => err(&e),
            }
        }
        "register/finish" => {
            if rp.is_enrolled() {
                return err("registration closed");
            }
            let Some(sid) = sid else {
                return err("no session");
            };
            let state = {
                let mut sessions = auth.sessions.lock().unwrap();
                match sessions.get_mut(sid).and_then(|s| s.pending.take()) {
                    Some(Pending::Register(state)) => state,
                    other => {
                        if let (Some(s), Some(p)) = (sessions.get_mut(sid), other) {
                            s.pending = Some(p);
                        }
                        return err("no registration in progress");
                    }
                }
            };
            // Touch-ID gate lives inside register_finish (someone must be at the server box).
            match rp.register_finish(body, &state, entitlement.to_vec()) {
                Ok(()) => ("200 OK", json, br#"{"ok":true}"#.to_vec(), None),
                Err(e) => err(&e),
            }
        }
        "logout" => {
            if let Some(sid) = sid {
                auth.sessions.lock().unwrap().remove(sid);
            }
            // Expire the cookie (Max-Age=0) so the browser drops it.
            (
                "200 OK",
                json,
                br#"{"ok":true}"#.to_vec(),
                Some("cms_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0".to_string()),
            )
        }
        other => err(&format!("unknown auth route `{other}`")),
    }
}

/// Look up the session for `sid`, or mint a fresh one. Returns its id and, when newly minted, a
/// `Set-Cookie` to send so the browser carries it on the follow-up `finish` request.
fn ensure_session(auth: &HttpAuth, sid: Option<&str>) -> (String, Option<String>) {
    let mut sessions = auth.sessions.lock().unwrap();
    if let Some(sid) = sid {
        if sessions.contains_key(sid) {
            return (sid.to_string(), None);
        }
    }
    let id = Uuid::new_v4().to_string();
    sessions.insert(id.clone(), HttpSession::default());
    let cookie = session_cookie(&id);
    (id, Some(cookie))
}

/// Serve the reading room over plain HTTP on `port`, localhost only — replacing the separate
/// `python3 -m http.server`. Routes: `/r/{iri}?args` resolves a resource to its fragment under
/// the caller's session capability (the same `resolve()` the wire does — so vanilla htmx can
/// drive the room over HTTP); `/auth/*` runs the passkey ceremony; everything else is a static
/// file from `dist/`. `http://localhost` is a secure context, so WebTransport + the passkey
/// ceremony work from it. One request per connection (`Connection: close`).
#[allow(clippy::too_many_arguments)]
async fn serve_http(
    dist: PathBuf,
    port: u16,
    kernel: Arc<Kernel>,
    rp: Arc<Rp>,
    auth: Arc<HttpAuth>,
    recent: Arc<RecentLog>,
    entitlement: Arc<Vec<String>>,
    dev_open: bool,
) {
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("reading-room server: cannot bind localhost:{port}: {e}");
            eprintln!("  (is the port already in use? set CMS_PORT to a free one)");
            return;
        }
    };
    loop {
        match listener.accept().await {
            Ok((sock, _)) => {
                let dist = dist.clone();
                let kernel = Arc::clone(&kernel);
                let rp = Arc::clone(&rp);
                let auth = Arc::clone(&auth);
                let recent = Arc::clone(&recent);
                let entitlement = Arc::clone(&entitlement);
                tokio::spawn(async move {
                    let _ = handle_http(
                        sock,
                        &dist,
                        &kernel,
                        &rp,
                        &auth,
                        &recent,
                        &entitlement,
                        dev_open,
                    )
                    .await;
                });
            }
            Err(e) => eprintln!("http accept: {e}"),
        }
    }
}

/// Answer one HTTP request. `/r/{iri}?args` → a fragment resolved under the caller's session
/// capability; `/auth/*` → the passkey ceremony; anything else → a `dist/` file (or 404). One
/// request per connection (`Connection: close`).
#[allow(clippy::too_many_arguments)]
async fn handle_http(
    mut sock: TcpStream,
    dist: &Path,
    kernel: &Kernel,
    rp: &Rp,
    auth: &HttpAuth,
    recent: &RecentLog,
    entitlement: &[String],
    dev_open: bool,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    // Read up to the end of the headers (the blank line).
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    };
    let req = parse_head(&String::from_utf8_lossy(&buf[..header_end]));
    // Read the body (POST) up to Content-Length.
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < req.content_length {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > 256 * 1024 {
            break;
        }
    }
    let sid = req.cookies.get("cms_session").map(String::as_str);

    let (status, ctype, out, set_cookie) = if let Some(route) = req.target.strip_prefix("/auth/") {
        http_auth(
            route,
            &req.method,
            &body,
            sid,
            rp,
            auth,
            entitlement,
            dev_open,
        )
    } else if req.target == "/purge" && req.method == "POST" {
        let (s, c, b) = handle_purge(kernel, auth, sid, dev_open, "urn:cms:purge");
        (s, c, b, None)
    } else if req.target == "/purge-unreachable" && req.method == "POST" {
        let (s, c, b) = handle_purge(kernel, auth, sid, dev_open, "urn:cms:purge-unreachable");
        (s, c, b, None)
    } else if let Some(target) = req.target.strip_prefix("/r/") {
        let (cap, principal) = session_cap(auth, sid, entitlement, dev_open);
        let (s, c, b) = resolve_http(kernel, recent, &cap, principal.as_deref(), target);
        (s, c, b, None)
    } else {
        let (s, c, b) = match safe_rel(&req.target) {
            Some(rel) => match std::fs::read(dist.join(&rel)) {
                Ok(bytes) => ("200 OK", content_type(&rel), bytes),
                Err(_) => (
                    "404 Not Found",
                    "text/plain; charset=utf-8",
                    b"not found".to_vec(),
                ),
            },
            None => (
                "400 Bad Request",
                "text/plain; charset=utf-8",
                b"bad path".to_vec(),
            ),
        };
        (s, c, b, None)
    };

    let cookie_line = set_cookie
        .map(|c| format!("Set-Cookie: {c}\r\n"))
        .unwrap_or_default();
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         {cookie_line}Cache-Control: no-cache\r\nConnection: close\r\n\r\n",
        out.len()
    );
    sock.write_all(header.as_bytes()).await?;
    sock.write_all(&out).await?;
    sock.flush().await
}

/// Resolve `/r/{iri}?args` to an HTML fragment via the kernel under `cap` — the same `resolve()`
/// `POST /purge` — execute the reviewed removal. Authorization is **"you're signed in"** (an
/// authenticated session, or `dev_open`); the purge itself runs **elevated** (`root`) so it can
/// write the bookmarks file, since the room's session capability is deliberately read-only. The
/// two-step confirm (Source prompt → this POST) plus the file backup are the guardrails.
fn handle_purge(
    kernel: &Kernel,
    auth: &HttpAuth,
    sid: Option<&str>,
    dev_open: bool,
    purge_iri: &str,
) -> (&'static str, &'static str, Vec<u8>) {
    let authed = dev_open
        || sid.is_some_and(|s| {
            auth.sessions
                .lock()
                .unwrap()
                .get(s)
                .is_some_and(|sess| sess.scopes.is_some())
        });
    if !authed {
        return (
            "200 OK",
            "text/html; charset=utf-8",
            b"<p class=\"cms-error\">Sign in to purge.</p>".to_vec(),
        );
    }
    let req = Request::new(Verb::Sink, Iri::parse(purge_iri).expect("valid IRI"));
    match Resolver::issue_as(kernel, req, &Capability::root()) {
        Ok((repr, _)) => ("200 OK", "text/html; charset=utf-8", repr.bytes),
        Err(e) => (
            "200 OK",
            "text/html; charset=utf-8",
            format!("<p class=\"cms-error\">purge failed: {e}</p>").into_bytes(),
        ),
    }
}

/// the wire runs. `urn:cms:recent` is a session resource (this principal's trail), rendered here
/// rather than in the kernel; every other resolved view is noted to the trail. Args ride as
/// query params. A resolve error becomes an inline error fragment so htmx swaps something visible.
fn resolve_http(
    kernel: &Kernel,
    recent: &RecentLog,
    cap: &Capability,
    principal: Option<&str>,
    target: &str,
) -> (&'static str, &'static str, Vec<u8>) {
    let (iri_enc, query) = target.split_once('?').unwrap_or((target, ""));
    let iri = percent_decode(iri_enc);
    // The recency trail is a session resource, not a kernel one — render it from this
    // principal's history through the shared `recent` stylesheet, exactly as the wire does.
    if iri == "urn:cms:recent" {
        return match render_recent_html(kernel, recent, principal, cap) {
            Ok(bytes) => ("200 OK", "text/html; charset=utf-8", bytes),
            Err(e) => (
                "200 OK",
                "text/html; charset=utf-8",
                format!("<p class=\"cms-error\">{e}</p>").into_bytes(),
            ),
        };
    }
    let Ok(resource) = Iri::parse(iri.clone()) else {
        return (
            "400 Bad Request",
            "text/plain; charset=utf-8",
            b"bad iri".to_vec(),
        );
    };
    // A tag opened inside a kind carries `type` — record it so the trail re-opens it scoped.
    let scope = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "type")
        .map(|(_, v)| percent_decode(v));
    let mut request = Request::new(Verb::Source, resource);
    for (k, v) in query.split('&').filter_map(|kv| kv.split_once('=')) {
        request = request.with_arg(k, ArgRef::Inline(percent_decode(v).into_bytes()));
    }
    match Resolver::issue_as(kernel, request, cap) {
        Ok((repr, _)) => {
            note_recent(recent, principal, &iri, scope.as_deref());
            ("200 OK", "text/html; charset=utf-8", repr.bytes)
        }
        Err(e) => (
            "200 OK",
            "text/html; charset=utf-8",
            format!("<p class=\"cms-error\">{e}</p>").into_bytes(),
        ),
    }
}

/// Minimal percent-decoding for a URL path/query segment (`%XX` and `+`→space).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve an HTTP request-target to a safe relative path under `dist/`: strip the query,
/// default `/` to `index.html`, and refuse any `..`/empty component (no traversal). `None`
/// for a rejected path.
fn safe_rel(target: &str) -> Option<String> {
    let path = target
        .split(['?', '#'])
        .next()
        .unwrap_or("/")
        .trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if path
        .split('/')
        .any(|c| c.is_empty() || c == ".." || c == ".")
    {
        return None;
    }
    Some(path.to_string())
}

/// Content type by file extension (the few the reading room serves).
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Per-connection session state: the current ceiling (raised by a verified login), the
/// in-progress WebAuthn ceremony state (held here, never persisted), and the signed-in
/// principal id (the recency trail's key; `None` until login).
struct Session {
    ceiling: Capability,
    auth_state: Option<PasskeyAuthentication>,
    reg_state: Option<PasskeyRegistration>,
    principal: Option<String>,
}

/// Accept one WebTransport session and answer `Call`s on its bidi streams until the
/// client disconnects.
async fn serve(
    incoming: IncomingSession,
    kernel: Arc<Kernel>,
    rp: Arc<Rp>,
    http_auth: Arc<HttpAuth>,
    entitlement: Arc<Vec<String>>,
    recent: Arc<RecentLog>,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = incoming.await?.accept().await?;
    // Public until a verified passkey raises the ceiling.
    let mut session = Session {
        ceiling: Capability::scoped(Vec::<String>::new()),
        auth_state: None,
        reg_state: None,
        principal: None,
    };
    loop {
        let (mut send, recv) = match connection.accept_bi().await {
            Ok(stream) => stream,
            Err(_) => return Ok(()), // client closed the connection
        };
        let mut bytes = Vec::new();
        recv.take(MAX_CALL as u64).read_to_end(&mut bytes).await?;
        let reply = handle(
            &kernel,
            &rp,
            &http_auth,
            &entitlement,
            &recent,
            &mut session,
            &bytes,
        );
        send.write_all(&reply).await?;
        send.finish().await?;
    }
}

/// Decode a `Call` and answer it. `urn:auth:*` Calls are handled by the session layer
/// (the passkey ceremony); everything else resolves against the kernel **under the
/// connection's ceiling** — so the room is gated until a verified passkey raises it.
#[allow(clippy::too_many_arguments)]
fn handle(
    kernel: &Kernel,
    rp: &Rp,
    http_auth: &HttpAuth,
    entitlement: &[String],
    recent: &RecentLog,
    session: &mut Session,
    bytes: &[u8],
) -> Vec<u8> {
    let reply = match decode::<Call>(bytes) {
        Ok(Call::Issue(req)) if req.target.as_str().starts_with("urn:auth:") => {
            handle_auth(rp, entitlement, http_auth, session, &req)
        }
        // The recency trail is a session resource, not a kernel one: rendered from what
        // this identity has viewed, through the `recent` stylesheet.
        Ok(Call::Issue(req)) if req.target.as_str() == "urn:cms:recent" => {
            render_recent(kernel, recent, session)
        }
        Ok(Call::Issue(req)) => {
            let iri = req.target.as_str().to_string();
            let scope = inline_arg(&req, "type")
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(str::to_string);
            let reply = resolve(kernel, &session.ceiling, req);
            if matches!(reply, Reply::Resolved(..)) {
                note_recent(recent, session.principal.as_deref(), &iri, scope.as_deref());
            }
            reply
        }
        // A client may carry a capability to attenuate below the ceiling; clamp it.
        Ok(Call::IssueAs(req, carried)) => {
            let iri = req.target.as_str().to_string();
            let scope = inline_arg(&req, "type")
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(str::to_string);
            let reply = resolve(kernel, &session.ceiling.clamp(&carried), req);
            if matches!(reply, Reply::Resolved(..)) {
                note_recent(recent, session.principal.as_deref(), &iri, scope.as_deref());
            }
            reply
        }
        Ok(Call::IsCached(req)) => {
            Reply::Cached(Resolver::is_cached(kernel, &req, &session.ceiling))
        }
        Ok(Call::Entries) => Reply::Entries(Resolver::entries(kernel)),
        Err(e) => Reply::Error(format!("undecodable call: {e}")),
    };
    encode(&reply).unwrap_or_default()
}

/// Resolve a request under `cap` and wrap the result as a `Reply`.
fn resolve(kernel: &Kernel, cap: &Capability, request: Request) -> Reply {
    match Resolver::issue_as(kernel, request, cap) {
        Ok((representation, status)) => Reply::Resolved(representation, status),
        Err(e) => Reply::Error(e),
    }
}

/// The display label and recorded scope for a recordable view, or `None` if this IRI
/// isn't something the recency trail tracks. Only pure-URI, re-openable views count (a tag
/// view — optionally scoped to a type; a type view) — search is excluded since re-opening
/// it needs its `q` arg. `scope` is the request's `type` arg (a tag opened inside a kind).
fn recordable(iri: &str, scope: Option<&str>) -> Option<(String, Option<String>)> {
    if let Some(tag) = iri.strip_prefix("urn:cms:view:") {
        return Some(match scope {
            Some(kind @ ("book" | "bookmark")) => {
                (format!("{kind} · #{tag}"), Some(kind.to_string()))
            }
            _ => (format!("#{tag}"), None),
        });
    }
    if let Some(kind) = iri.strip_prefix("urn:cms:type:") {
        return Some((format!("type: {kind}"), None));
    }
    None
}

/// If a signed-in `principal` just successfully opened a recordable view, add it to the trail
/// (a repeat visit moves it to the front). `scope` = the view's `type` arg, so a tag opened
/// inside a kind is recorded and re-opened within that kind. Callers invoke this only on a
/// successful resolve. Shared by both transports (wire session + HTTP session).
fn note_recent(recent: &RecentLog, principal: Option<&str>, iri: &str, scope: Option<&str>) {
    let (Some(principal), Some((label, scope))) = (principal, recordable(iri, scope)) else {
        return;
    };
    recent.record(
        principal,
        Recent {
            iri: iri.to_string(),
            scope,
            label,
        },
    );
}

/// Render `principal`'s recency trail to an htmx fragment through the shared `recent` stylesheet
/// resource — a view is a query; here the "query" is the principal's trail. The transport-neutral
/// core, resolved under `cap`; the wire wraps it in a `Reply`, HTTP serves the bytes directly.
fn render_recent_html(
    kernel: &Kernel,
    recent: &RecentLog,
    principal: Option<&str>,
    cap: &Capability,
) -> Result<Vec<u8>, String> {
    let items = principal.map(|p| recent.list(p)).unwrap_or_default();
    let xml = recent_xml(&items);
    let req = Request::new(
        Verb::Source,
        Iri::parse("urn:xslt:transform").expect("valid IRI"),
    )
    .with_arg("content", ArgRef::Inline(xml.into_bytes()))
    .with_arg(
        "stylesheet",
        ArgRef::Inline(b"urn:cms:style:recent".to_vec()),
    );
    Resolver::issue_as(kernel, req, cap)
        .map(|(repr, _)| repr.bytes)
        .map_err(|e| e.to_string())
}

/// The wire's recency reply: `render_recent_html` wrapped as an uncacheable `Reply`.
fn render_recent(kernel: &Kernel, recent: &RecentLog, session: &Session) -> Reply {
    match render_recent_html(
        kernel,
        recent,
        session.principal.as_deref(),
        &session.ceiling,
    ) {
        Ok(bytes) => Reply::Resolved(
            Representation::new(
                ReprType::new("text/html").with_param("charset", "utf-8"),
                bytes,
            ),
            CacheStatus::Uncacheable,
        ),
        Err(e) => Reply::Error(e),
    }
}

/// A tiny XML doc of the trail for the `recent` stylesheet to render. An empty trail is
/// its own element (so the stylesheet needs no conditionals — just a template match).
fn recent_xml(items: &[Recent]) -> String {
    let mut s = String::from("<recent xmlns=\"urn:cms:recent#\">");
    if items.is_empty() {
        s.push_str("<empty/>");
    } else {
        for it in items {
            s.push_str("<item iri=\"");
            xml_escape_into(&mut s, &it.iri);
            // Always emit scope (empty = unscoped) so the stylesheet can render it flatly.
            s.push_str("\" scope=\"");
            xml_escape_into(&mut s, it.scope.as_deref().unwrap_or(""));
            s.push_str("\">");
            xml_escape_into(&mut s, &it.label);
            s.push_str("</item>");
        }
    }
    s.push_str("</recent>");
    s
}

/// Escape a value into XML text / attribute context.
fn xml_escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// The passkey ceremony, over the wire as `urn:auth:*` resources. Registration is a
/// trust-on-first-use bootstrap: allowed only while no passkey is enrolled (the first
/// credential claims the room; further enrollment is a later, cap-gated concern).
fn handle_auth(
    rp: &Rp,
    entitlement: &[String],
    http_auth: &HttpAuth,
    session: &mut Session,
    req: &Request,
) -> Reply {
    match req.target.as_str() {
        "urn:auth:resume" => {
            // Bridge an existing HTTP login onto this wire connection: the page presents the
            // wire token it read from `/auth/status` (a bearer value it can read, unlike the
            // HttpOnly cookie), and this connection adopts that session's capability + principal.
            // One login then covers both transports.
            let Some(token) = inline_arg(req, "token").and_then(|b| std::str::from_utf8(b).ok())
            else {
                return error_reply("resume needs a `token`");
            };
            let sessions = http_auth.sessions.lock().unwrap();
            match sessions
                .values()
                .find(|s| s.scopes.is_some() && s.wire_token.as_deref() == Some(token))
            {
                Some(s) => {
                    session.ceiling = Capability::scoped(s.scopes.clone().unwrap_or_default());
                    session.principal = s.principal.clone();
                    json_reply(br#"{"ok":true}"#.to_vec())
                }
                None => error_reply("unknown or unauthenticated wire token"),
            }
        }
        "urn:auth:login:start" => match rp.login_start() {
            Ok((challenge, state)) => {
                session.auth_state = Some(state);
                json_reply(challenge.into_bytes())
            }
            Err(e) => Reply::Error(e),
        },
        "urn:auth:login:finish" => {
            let Some(state) = session.auth_state.take() else {
                return error_reply("no login in progress");
            };
            let Some(cred) = inline_arg(req, "credential") else {
                return error_reply("login:finish needs a `credential`");
            };
            match rp.login_finish(cred, &state) {
                Ok((_stored, principal)) => {
                    // Grant the CURRENT server entitlement — not the scopes frozen into the
                    // credential at registration (`_stored`). A single-user reading room's
                    // room-wide grant grows as sources are added (e.g. the presentations dir);
                    // freezing it at registration would silently deny newly-added sources
                    // until you re-registered. (Per-credential scoping is a future
                    // passkey-workspace concern; today every credential grants the same room.)
                    session.ceiling = Capability::scoped(entitlement.to_vec());
                    session.principal = Some(principal); // scope the recency trail to them
                    json_reply(br#"{"ok":true}"#.to_vec())
                }
                Err(e) => Reply::Error(e),
            }
        }
        "urn:auth:register:start" => {
            if rp.is_enrolled() {
                return Reply::Error("registration closed: a passkey is already enrolled".into());
            }
            let name = inline_arg(req, "name")
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("ikigai");
            match rp.register_start(name) {
                Ok((challenge, state)) => {
                    session.reg_state = Some(state);
                    json_reply(challenge.into_bytes())
                }
                Err(e) => Reply::Error(e),
            }
        }
        "urn:auth:register:finish" => {
            if rp.is_enrolled() {
                return Reply::Error("registration closed".into());
            }
            let Some(state) = session.reg_state.take() else {
                return error_reply("no registration in progress");
            };
            let Some(cred) = inline_arg(req, "credential") else {
                return error_reply("register:finish needs a `credential`");
            };
            match rp.register_finish(cred, &state, entitlement.to_vec()) {
                Ok(()) => json_reply(br#"{"ok":true}"#.to_vec()),
                Err(e) => Reply::Error(e),
            }
        }
        "urn:auth:logout" => {
            // Drop the connection back to the public ceiling; the enrolled passkey stays,
            // this session just loses its elevation until it signs in again.
            session.ceiling = Capability::scoped(Vec::<String>::new());
            session.auth_state = None;
            session.reg_state = None;
            session.principal = None; // stop recording; the stored trail persists for next login
            json_reply(br#"{"ok":true}"#.to_vec())
        }
        other => Reply::Error(format!("unknown auth resource `{other}`")),
    }
}

/// Read a request's inline argument bytes (the browser sends the WebAuthn JSON here).
fn inline_arg<'a>(req: &'a Request, name: &str) -> Option<&'a [u8]> {
    match req.args.get(name) {
        Some(ArgRef::Inline(bytes)) => Some(bytes.as_slice()),
        _ => None,
    }
}

/// Wrap JSON bytes as an uncacheable `Reply::Resolved` (the auth exchange is never cached).
fn json_reply(bytes: Vec<u8>) -> Reply {
    Reply::Resolved(
        Representation::new(
            ReprType::new("application/json").with_param("charset", "utf-8"),
            bytes,
        ),
        CacheStatus::Uncacheable,
    )
}

/// A `{"error": "..."}` JSON reply, for auth-flow errors the page reads as JSON.
fn error_reply(msg: &str) -> Reply {
    json_reply(serde_json::json!({ "error": msg }).to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::{
        content_type, ensure_session, handle_auth, http_auth, parse_head, resolve_http, safe_rel,
        serve_http, session_cap, HttpAuth, HttpSession, Session,
    };
    use ikigai_cms_web::session::{RecentLog, Rp};
    use ikigai_core::{ArgRef, Capability, Iri, Request, Verb};
    use ikigai_wire::Reply;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A fresh public (unelevated) wire session.
    fn public_session() -> Session {
        Session {
            ceiling: Capability::scoped(Vec::<String>::new()),
            auth_state: None,
            reg_state: None,
            principal: None,
        }
    }

    /// The body text of a `Reply` (auth replies are JSON; errors carry the message).
    fn reply_body(reply: Reply) -> String {
        match reply {
            Reply::Resolved(repr, _) => String::from_utf8_lossy(&repr.bytes).into_owned(),
            Reply::Error(e) => e,
            _ => String::new(),
        }
    }

    /// A CMS kernel over a temp bookmarks fixture, plus its fs-read entitlement.
    fn fixture(dir: &std::path::Path) -> (ikigai_core::Kernel, Vec<String>) {
        let bm = dir.join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(
            &bm,
            "* Bookmarks\n** [[https://sci.example][Science Bookmark]]\n   \
             :PROPERTIES:\n   :TAGS: science\n   :END:\n",
        )
        .unwrap();
        let kernel = ikigai_cms_web::build_cms_kernel(dir.to_path_buf(), None);
        let ent = vec![format!("urn:cap:fs:read:{}", dir.display())];
        (kernel, ent)
    }

    /// A relying party over a Keychain-free file backend (no biometric, empty store) — enough to
    /// exercise the HTTP auth routing that doesn't need a real authenticator.
    fn empty_rp(dir: &std::path::Path) -> Rp {
        Rp::new(
            "localhost",
            "http://localhost:8080",
            Arc::new(ikigai_secret::FileBackend::new(dir)),
        )
        .expect("rp builds")
    }

    #[test]
    fn the_http_resolve_route_renders_under_a_granted_cap_and_gates_without() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, ent) = fixture(dir.path());
        let recent = RecentLog::default();
        // A granted cap (an authenticated session's scopes) → the tag view renders the bookmark.
        let full = Capability::scoped(ent.clone());
        let (status, ctype, body) = resolve_http(
            &kernel,
            &recent,
            &full,
            None,
            "urn:cms:view:science?style=catalog",
        );
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, "200 OK");
        assert!(ctype.contains("text/html"));
        assert!(
            body.contains("Science Bookmark"),
            "a granted cap renders the view: {body}"
        );
        // A public cap → the whole graph is gated → nothing resolves.
        let public = Capability::scoped(Vec::<String>::new());
        let (_s, _c, gated) = resolve_http(
            &kernel,
            &recent,
            &public,
            None,
            "urn:cms:view:science?style=catalog",
        );
        assert!(
            !String::from_utf8_lossy(&gated).contains("Science Bookmark"),
            "public cap → gated: {}",
            String::from_utf8_lossy(&gated)
        );
        // A malformed IRI → 400.
        assert_eq!(
            resolve_http(&kernel, &recent, &full, None, "").0,
            "400 Bad Request"
        );
    }

    #[test]
    fn parse_head_extracts_method_target_cookies_and_length() {
        let head = "POST /auth/login/finish HTTP/1.1\r\nHost: x\r\n\
             Content-Length: 12\r\nCookie: a=1; cms_session=abc";
        let r = parse_head(head);
        assert_eq!(r.method, "POST");
        assert_eq!(r.target, "/auth/login/finish");
        assert_eq!(r.content_length, 12);
        assert_eq!(
            r.cookies.get("cms_session").map(String::as_str),
            Some("abc")
        );
        assert_eq!(r.cookies.get("a").map(String::as_str), Some("1"));
    }

    #[test]
    fn an_authenticated_session_resolves_the_room_and_gets_noted_to_the_trail() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, ent) = fixture(dir.path());
        let recent = RecentLog::default();
        let auth = HttpAuth::default();
        // Seed a signed-in session carrying the room entitlement.
        auth.sessions.lock().unwrap().insert(
            "sid1".into(),
            HttpSession {
                scopes: Some(ent.clone()),
                principal: Some("p".into()),
                pending: None,
                wire_token: None,
            },
        );
        // Its cap resolves the gated view, and the visit is recorded to that principal's trail.
        let (cap, who) = session_cap(&auth, Some("sid1"), &ent, false);
        assert_eq!(who.as_deref(), Some("p"));
        let (_s, _c, body) = resolve_http(
            &kernel,
            &recent,
            &cap,
            who.as_deref(),
            "urn:cms:view:science?style=catalog",
        );
        assert!(
            String::from_utf8_lossy(&body).contains("Science Bookmark"),
            "the session sees the room"
        );
        assert_eq!(recent.list("p").len(), 1, "the view was noted to the trail");
        // An unknown session, not dev-open → public cap → gated.
        let (cap2, _) = session_cap(&auth, Some("ghost"), &ent, false);
        let (_s, _c, g) = resolve_http(
            &kernel,
            &recent,
            &cap2,
            None,
            "urn:cms:view:science?style=catalog",
        );
        assert!(
            !String::from_utf8_lossy(&g).contains("Science Bookmark"),
            "no session → gated"
        );
    }

    #[test]
    fn http_auth_reports_status_and_guards_the_ceremony() {
        let dir = tempfile::tempdir().unwrap();
        let rp = empty_rp(dir.path());
        let auth = HttpAuth::default();
        let ent = vec!["urn:cap:fs:read:/x".to_string()];

        // status: nothing enrolled, unauthenticated, and the dev_open flag is echoed back.
        let (st, ctype, body, cookie) =
            http_auth("status", "GET", b"", None, &rp, &auth, &ent, true);
        assert_eq!(st, "200 OK");
        assert!(ctype.contains("json"));
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["enrolled"], false);
        assert_eq!(v["authenticated"], false);
        assert_eq!(v["dev_open"], true);
        assert!(cookie.is_none());

        // login/finish with no session/ceremony → error, no panic.
        let (_s, _c, body, _) =
            http_auth("login/finish", "POST", b"{}", None, &rp, &auth, &ent, false);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("no session"));

        // register/start (store empty → open) mints a session and sets the cookie.
        let (_s, _c, _b, cookie) = http_auth(
            "register/start",
            "POST",
            br#"{"name":"x"}"#,
            None,
            &rp,
            &auth,
            &ent,
            false,
        );
        let cookie = cookie.expect("register/start sets a session cookie");
        assert!(cookie.contains("cms_session="), "cookie: {cookie}");
        assert!(cookie.contains("HttpOnly"), "cookie: {cookie}");

        // A GET on a POST-only route → 405.
        assert_eq!(
            http_auth("logout", "GET", b"", None, &rp, &auth, &ent, false).0,
            "405 Method Not Allowed"
        );
    }

    #[test]
    fn the_wire_resume_bridges_an_authenticated_http_session_by_token() {
        let dir = tempfile::tempdir().unwrap();
        let rp = empty_rp(dir.path());
        let auth = HttpAuth::default();
        let ent = vec!["urn:cap:fs:read:/x".to_string()];
        // An authenticated HTTP session that has minted a wire token.
        auth.sessions.lock().unwrap().insert(
            "sid".into(),
            HttpSession {
                scopes: Some(ent.clone()),
                principal: Some("p".into()),
                pending: None,
                wire_token: Some("tok-123".into()),
            },
        );
        let resume = |tok: &str, session: &mut Session| {
            let req = Request::new(Verb::Source, Iri::parse("urn:auth:resume").unwrap())
                .with_arg("token", ArgRef::Inline(tok.as_bytes().to_vec()));
            handle_auth(&rp, &ent, &auth, session, &req)
        };
        // The right token adopts the session's principal (and, with it, its capability).
        let mut s = public_session();
        assert!(reply_body(resume("tok-123", &mut s)).contains("\"ok\":true"));
        assert_eq!(s.principal.as_deref(), Some("p"));
        // A bogus token is refused and leaves the connection unelevated (still public).
        let mut fresh = public_session();
        assert!(reply_body(resume("nope", &mut fresh)).contains("error"));
        assert_eq!(fresh.principal, None);
    }

    #[test]
    fn ensure_session_reuses_a_known_id_and_mints_otherwise() {
        let auth = HttpAuth::default();
        let (id1, c1) = ensure_session(&auth, None);
        let c1 = c1.expect("a fresh session sets a cookie");
        assert!(c1.contains(&id1));
        // The same id → reused, no new cookie.
        let (id2, c2) = ensure_session(&auth, Some(&id1));
        assert_eq!(id1, id2);
        assert!(c2.is_none());
        // An unknown id → a fresh one is minted.
        let (id3, c3) = ensure_session(&auth, Some("ghost"));
        assert_ne!(id3, "ghost");
        assert!(c3.is_some());
    }

    #[test]
    fn safe_rel_defaults_root_and_refuses_traversal() {
        assert_eq!(safe_rel("/").as_deref(), Some("index.html"));
        assert_eq!(safe_rel("/index.html").as_deref(), Some("index.html"));
        assert_eq!(safe_rel("/cert.json?v=2").as_deref(), Some("cert.json"));
        assert_eq!(safe_rel("/styles/a.css").as_deref(), Some("styles/a.css"));
        assert_eq!(safe_rel("/../etc/passwd"), None);
        assert_eq!(safe_rel("/a/../../b"), None);
        assert_eq!(safe_rel("/a//b"), None); // empty component
        assert_eq!(safe_rel("/./secret"), None);
    }

    #[test]
    fn content_type_by_extension() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("cert.json"), "application/json; charset=utf-8");
        assert_eq!(content_type("ikigai_cms_web_bg.wasm"), "application/wasm");
        assert_eq!(content_type("mod.mjs"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("x.svg"), "image/svg+xml");
        assert_eq!(content_type("noext"), "application/octet-stream");
    }

    #[tokio::test]
    async fn the_page_server_serves_dist_resolves_r_and_404s_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>reading room</h1>").unwrap();
        let (kernel, ent) = fixture(dir.path());
        // A free ephemeral port.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        tokio::spawn(serve_http(
            dir.path().to_path_buf(),
            port,
            Arc::new(kernel),
            Arc::new(empty_rp(dir.path())),
            Arc::new(HttpAuth::default()),
            Arc::new(RecentLog::default()),
            Arc::new(ent),
            true, // dev-open, so /r/ resolves under the entitlement
        ));

        async fn get(port: u16, target: &str) -> String {
            // retry until the server is listening
            let mut s = None;
            for _ in 0..50 {
                if let Ok(c) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                    s = Some(c);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let mut c = s.expect("page server listening");
            c.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut buf = Vec::new();
            c.read_to_end(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        }

        let root = get(port, "/").await;
        assert!(root.contains("200 OK"), "root serves index.html: {root}");
        assert!(root.contains("reading room"), "body: {root}");
        assert!(root.contains("text/html"), "content-type: {root}");

        // The resource face: /r/{iri} resolves the same fragment the wire would.
        let view = get(port, "/r/urn:cms:view:science?style=catalog").await;
        assert!(view.contains("200 OK"), "/r/ resolves: {view}");
        assert!(
            view.contains("Science Bookmark"),
            "/r/ renders the tag view over HTTP: {view}"
        );

        assert!(
            get(port, "/nope.html").await.contains("404"),
            "missing → 404"
        );
        assert!(
            get(port, "/../Cargo.toml").await.contains("400"),
            "traversal → 400"
        );
    }
}
