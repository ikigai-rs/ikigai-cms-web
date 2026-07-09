//! A WebTransport server for the CMS reading room, with a passkey relying party.
//!
//! Serves [`ikigai_cms_web::build_cms_kernel`] over WebTransport (HTTP/3 over QUIC),
//! speaking the `ikigai-wire` `Call`/`Reply` protocol. Each connection carries a
//! **ceiling** capability: it starts *public* (grants nothing — the whole graph is
//! behind the fs-read cap, so the room is gated), and a verified passkey raises it to
//! the credential's entitlement (rung 3a's `dispatch` clamp then enforces it).
//!
//! The WebAuthn ceremony rides over the same wire as `urn:auth:*` resources, intercepted
//! here by the session layer (not the kernel). It binds to the *page* origin (where
//! `navigator.credentials` runs, default `http://localhost:8080`), configurable via
//! `CMS_RP_ID` / `CMS_RP_ORIGIN`; the `{Passkey → scopes}` store persists at
//! `CMS_PASSKEYS` (default `<src_dir>/.cms-passkeys.json`).
//!
//! Run: `cargo run --features server --bin cms-server -- [port] [src_dir]`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ikigai_core::{ArgRef, Capability, Kernel, ReprType, Representation, Request};
use ikigai_resolve::{CacheStatus, Resolver};
use ikigai_wire::{decode, encode, Call, Reply};
use tokio::io::AsyncReadExt;
use webauthn_rs::prelude::{PasskeyAuthentication, PasskeyRegistration};
use wtransport::endpoint::IncomingSession;
use wtransport::{Endpoint, Identity, ServerConfig};

use ikigai_cms_web::session::Rp;

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

    // The entitlement a verified passkey is granted: read the CMS source jail (the whole
    // room's chain bottoms out in this fs read).
    let entitlement = Arc::new(vec![format!("urn:cap:fs:read:{}", src_dir.display())]);

    // The relying party. rp_id + page origin default to local dev; the passkey store
    // persists beside the sources.
    let rp_id = std::env::var("CMS_RP_ID").unwrap_or_else(|_| "localhost".to_string());
    let rp_origin =
        std::env::var("CMS_RP_ORIGIN").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let store_path = std::env::var_os("CMS_PASSKEYS")
        .map(PathBuf::from)
        .unwrap_or_else(|| src_dir.join(".cms-passkeys.json"));
    let rp = Arc::new(Rp::new(&rp_id, &rp_origin, store_path)?);

    let identity = Identity::self_signed(["localhost", "127.0.0.1", "::1"])?;
    let cert_hash = identity.certificate_chain().as_slice()[0].hash();
    let hash_hex: String = cert_hash
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    println!("ikigai CMS reading-room server  →  https://127.0.0.1:{port}");
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
    println!("open the reading room with  #cert={hash_hex}  in the URL");

    let kernel = Arc::new(ikigai_cms_web::build_cms_kernel(src_dir));

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
        let entitlement = Arc::clone(&entitlement);
        tokio::spawn(async move {
            if let Err(e) = serve(incoming, kernel, rp, entitlement).await {
                eprintln!("session ended: {e}");
            }
        });
    }
}

/// Per-connection session state: the current ceiling (raised by a verified login) and
/// the in-progress WebAuthn ceremony state (held here, never persisted).
struct Session {
    ceiling: Capability,
    auth_state: Option<PasskeyAuthentication>,
    reg_state: Option<PasskeyRegistration>,
}

/// Accept one WebTransport session and answer `Call`s on its bidi streams until the
/// client disconnects.
async fn serve(
    incoming: IncomingSession,
    kernel: Arc<Kernel>,
    rp: Arc<Rp>,
    entitlement: Arc<Vec<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = incoming.await?.accept().await?;
    // Public until a verified passkey raises the ceiling.
    let mut session = Session {
        ceiling: Capability::scoped(Vec::<String>::new()),
        auth_state: None,
        reg_state: None,
    };
    loop {
        let (mut send, recv) = match connection.accept_bi().await {
            Ok(stream) => stream,
            Err(_) => return Ok(()), // client closed the connection
        };
        let mut bytes = Vec::new();
        recv.take(MAX_CALL as u64).read_to_end(&mut bytes).await?;
        let reply = handle(&kernel, &rp, &entitlement, &mut session, &bytes);
        send.write_all(&reply).await?;
        send.finish().await?;
    }
}

/// Decode a `Call` and answer it. `urn:auth:*` Calls are handled by the session layer
/// (the passkey ceremony); everything else resolves against the kernel **under the
/// connection's ceiling** — so the room is gated until a verified passkey raises it.
fn handle(
    kernel: &Kernel,
    rp: &Rp,
    entitlement: &[String],
    session: &mut Session,
    bytes: &[u8],
) -> Vec<u8> {
    let reply = match decode::<Call>(bytes) {
        Ok(Call::Issue(req)) if req.target.as_str().starts_with("urn:auth:") => {
            handle_auth(rp, entitlement, session, &req)
        }
        Ok(Call::Issue(req)) => resolve(kernel, &session.ceiling, req),
        // A client may carry a capability to attenuate below the ceiling; clamp it.
        Ok(Call::IssueAs(req, carried)) => resolve(kernel, &session.ceiling.clamp(&carried), req),
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

/// The passkey ceremony, over the wire as `urn:auth:*` resources. Registration is a
/// trust-on-first-use bootstrap: allowed only while no passkey is enrolled (the first
/// credential claims the room; further enrollment is a later, cap-gated concern).
fn handle_auth(rp: &Rp, entitlement: &[String], session: &mut Session, req: &Request) -> Reply {
    match req.target.as_str() {
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
                Ok(cap) => {
                    session.ceiling = cap; // raise the connection to the verified entitlement
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
