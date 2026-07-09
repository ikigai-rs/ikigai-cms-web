//! A WebTransport server for the CMS reading room (rung 2).
//!
//! Serves the same kernel the reading room composes ([`ikigai_cms_web::build_cms_kernel`])
//! over the network: the browser opens a WebTransport (HTTP/3 over QUIC) connection,
//! sends an `ikigai-wire` `Call` on a bidirectional stream, and gets a `Reply` back —
//! the exact protocol `ikigai-ipc` and `ikigai-quic` speak. The SPARQL over the
//! assembled bookmark graph runs here, server-side; the browser just renders the result.
//!
//! Run: `cargo run --features server --bin cms-server -- [port] [src_dir]`
//! (`src_dir` defaults to `$CMS_SRC_DIR`, then `~/Dropbox/org-mode-files`.)
//!
//! TLS is a self-signed cert; the browser trusts it via WebTransport's
//! `serverCertificateHashes` (printed below). Authentication is deferred to rung 3 (a
//! passkey relying party): this server resolves under root, like `ikigai-web-demo`'s —
//! run it only on a trusted host until the relying party lands.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ikigai_core::{Capability, Kernel};
use ikigai_resolve::Resolver;
use ikigai_wire::{decode, encode, Call, Reply};
use tokio::io::AsyncReadExt;
use wtransport::endpoint::IncomingSession;
use wtransport::{Endpoint, Identity, ServerConfig};

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

    // Self-signed cert valid for localhost; the browser pins its SHA-256.
    let identity = Identity::self_signed(["localhost", "127.0.0.1", "::1"])?;
    let cert_hash = identity.certificate_chain().as_slice()[0].hash();
    let hash_hex: String = cert_hash
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    println!("ikigai CMS reading-room server  →  https://127.0.0.1:{port}");
    println!("source jail: {}", src_dir.display());
    println!("cert sha-256: {hash_hex}");
    println!("open the reading room with  #cert={hash_hex}  in the URL");

    let kernel = Arc::new(ikigai_cms_web::build_cms_kernel(src_dir));
    // The session ceiling: the most a client on this connection may hold. Root for now
    // (the trusted-host posture); rung 3's relying party lowers it, per connection, to
    // the verified passkey principal's entitlement — after which a carried capability is
    // clamped to it (a client can attenuate, never exceed).
    let ceiling = Arc::new(Capability::root());

    let config = ServerConfig::builder()
        .with_bind_default(port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();
    let server = Endpoint::server(config)?;

    loop {
        let incoming = server.accept().await;
        let kernel = Arc::clone(&kernel);
        let ceiling = Arc::clone(&ceiling);
        tokio::spawn(async move {
            if let Err(e) = serve(incoming, kernel, ceiling).await {
                eprintln!("session ended: {e}");
            }
        });
    }
}

/// Accept one WebTransport session and answer `Call`s on its bidi streams until the
/// client disconnects.
async fn serve(
    incoming: IncomingSession,
    kernel: Arc<Kernel>,
    ceiling: Arc<Capability>,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = incoming.await?.accept().await?;
    loop {
        let (mut send, recv) = match connection.accept_bi().await {
            Ok(stream) => stream,
            Err(_) => return Ok(()), // client closed the connection
        };
        let mut bytes = Vec::new();
        recv.take(MAX_CALL as u64).read_to_end(&mut bytes).await?;
        let reply = dispatch(&kernel, &ceiling, &bytes);
        send.write_all(&reply).await?;
        send.finish().await?;
    }
}

/// Decode a `Call`, resolve it against the kernel (SPARQL over the CMS graph runs
/// here), and encode the `Reply`. The stream boundary frames the message.
fn dispatch(kernel: &Kernel, ceiling: &Capability, bytes: &[u8]) -> Vec<u8> {
    let reply = match decode::<Call>(bytes) {
        Ok(Call::Issue(request)) => match Resolver::issue(kernel, request) {
            Ok((representation, status)) => Reply::Resolved(representation, status),
            Err(e) => Reply::Error(e),
        },
        // Capability-on-the-wire: clamp the client's carried capability to this session's
        // ceiling before resolving, so it can only *attenuate*, never exceed. The ceiling
        // is root today (a no-op clamp — the trusted-host posture); rung 3's relying party
        // sets it to the verified principal's entitlement, at which point this line is the
        // enforcement boundary. The CMS view chain already honors it (an fs read down the
        // chain is denied without the grant).
        Ok(Call::IssueAs(request, carried)) => {
            let effective = ceiling.clamp(&carried);
            match Resolver::issue_as(kernel, request, &effective) {
                Ok((representation, status)) => Reply::Resolved(representation, status),
                Err(e) => Reply::Error(e),
            }
        }
        Ok(Call::IsCached(request)) => {
            Reply::Cached(Resolver::is_cached(kernel, &request, &Capability::root()))
        }
        Ok(Call::Entries) => Reply::Entries(Resolver::entries(kernel)),
        Err(e) => Reply::Error(format!("undecodable call: {e}")),
    };
    encode(&reply).unwrap_or_default()
}
