//! Graph maintenance: a kernel that composes the CMS graph with outbound HTTP, plus
//! `urn:cms:linkcheck` — a HEAD-check whose result is **golden-thread cached for a week**.
//!
//! In a persistent host (the maintenance daemon), a timer fires a pass that resolves
//! `urn:cms:linkcheck` per bookmark; a URL checked within the week is a cache hit (no
//! network), so re-runs only re-check what has gone stale.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ikigai_core::{
    Description, Endpoint, EndpointSpace, Exact, Fallback, Invocation, Kernel, ReprType,
    Representation, Result, Space, SystemClock, Verb,
};
use ikigai_http::{HttpRequest, HttpResponse, HttpTransport, Method};

/// How long a link-check result stays valid (the golden-thread expiry).
const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// A link-checking transport that flags **only genuine breakage** as `Err` (so
/// `urn:httpHead`'s Exists = "the bookmark still points at something"):
/// - `Ok` — 2xx/3xx, and also 401/403/405/429/5xx: the server *answered*, so the site is
///   alive even if it blocks HEAD or is transiently erroring. Not broken.
/// - `Err` — 404/410 (gone) and network failures (DNS, refused, timeout: unreachable).
///
/// The conservative bias avoids false positives — many live sites 403/405/520 a bare HEAD
/// from a bot, and removing those would lose good bookmarks.
pub struct CheckTransport;

#[async_trait]
impl HttpTransport for CheckTransport {
    async fn send(&self, req: HttpRequest) -> std::result::Result<HttpResponse, String> {
        let call = ureq::request(req.method.as_str(), &req.url)
            .timeout(Duration::from_secs(12))
            .call();
        match call {
            Ok(resp) => Ok(HttpResponse {
                status: resp.status(),
                headers: Vec::new(),
                body: Vec::new(),
            }),
            Err(ureq::Error::Status(404, _)) => Err("HTTP 404 (gone)".to_string()),
            Err(ureq::Error::Status(410, _)) => Err("HTTP 410 (gone)".to_string()),
            Err(ureq::Error::Status(code, _)) => Ok(HttpResponse {
                status: code,
                headers: Vec::new(),
                body: Vec::new(),
            }),
            Err(ureq::Error::Transport(t)) => Err(unreachable_reason(&t)),
        }
    }
}

/// A short reason for a transport (network) failure, without the URL noise ureq prepends.
fn unreachable_reason(t: &ureq::Transport) -> String {
    match t.kind() {
        ureq::ErrorKind::Dns => "DNS: host not found".to_string(),
        ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Io => {
            "connection failed / timed out".to_string()
        }
        other => format!("unreachable ({other:?})"),
    }
}

/// `urn:cms:linkcheck` — HEAD-check the `url` arg and report `ok` or `broken\t<reason>`,
/// **cacheable for a week**: resolving the same URL again inside the window is a cache
/// hit, no network. It does the HEAD via the transport directly (rather than resolving
/// `urn:httpHead`) precisely so it stays cacheable — an inner live web read would taint
/// this result as uncacheable through the kernel's dependency tracking.
struct LinkCheck {
    transport: Arc<dyn HttpTransport>,
}

#[async_trait]
impl Endpoint for LinkCheck {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let url = inv.inline_str("url")?.to_string();
        let request = HttpRequest {
            method: Method::Head,
            url,
            headers: Vec::new(),
            body: Vec::new(),
        };
        // Ok = reachable; Err = broken (the transport classifies gone/unreachable).
        let body = match self.transport.send(request).await {
            Ok(_) => "ok".to_string(),
            Err(reason) => format!("broken\t{reason}"),
        };
        let mut repr = Representation::new(ReprType::new("text/plain"), body.into_bytes());
        if let Some(now) = inv.now() {
            repr = repr.cacheable_until(now.plus_millis(WEEK_MS));
        }
        Ok(repr)
    }

    fn name(&self) -> &str {
        "cms-linkcheck"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:linkcheck")
            .summary("HEAD-check a bookmark URL (ok | broken); the result is cached for a week.")
            .verb(Verb::Source)
    }
}

/// A maintenance kernel: the CMS graph spaces + outbound HTTP (via `transport`) +
/// `urn:cms:linkcheck`, with a system clock so the week-long cache deadline is honored.
/// `transport` is injectable so a test can count network calls.
pub fn maintenance_kernel(src_dir: PathBuf, transport: Arc<dyn HttpTransport>) -> Kernel {
    let mut spaces = crate::cms_spaces(src_dir, None);
    spaces.push(Arc::new(
        EndpointSpace::new().bind(Exact::new("urn:cms:linkcheck"), LinkCheck { transport }),
    ) as Arc<dyn Space>);
    Kernel::new(Arc::new(Fallback::new(spaces))).with_clock(Arc::new(SystemClock))
}

/// [`maintenance_kernel`] over the real [`CheckTransport`] (ureq).
pub fn build_maintenance_kernel(src_dir: PathBuf) -> Kernel {
    maintenance_kernel(src_dir, Arc::new(CheckTransport))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikigai_core::{ArgRef, Iri, Request};
    use ikigai_resolve::Resolver;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A transport that counts sends, so we can prove the week-cache stops re-checking.
    struct Counting(Arc<AtomicUsize>);

    #[async_trait]
    impl HttpTransport for Counting {
        async fn send(&self, _req: HttpRequest) -> std::result::Result<HttpResponse, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            })
        }
    }

    fn linkcheck(kernel: &Kernel, url: &str) -> String {
        let req = Request::new(Verb::Source, Iri::parse("urn:cms:linkcheck").unwrap())
            .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()));
        let (repr, _) = Resolver::issue(kernel, req).expect("linkcheck resolves");
        String::from_utf8(repr.bytes).unwrap()
    }

    #[test]
    fn a_url_is_checked_once_then_served_from_the_week_cache() {
        let dir = tempfile::tempdir().unwrap();
        let bm = dir.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(&bm, "* Bookmarks\n").unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let kernel =
            maintenance_kernel(dir.path().to_path_buf(), Arc::new(Counting(calls.clone())));

        assert_eq!(linkcheck(&kernel, "https://example.com/a"), "ok");
        assert_eq!(linkcheck(&kernel, "https://example.com/a"), "ok"); // within the week
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the second check is served from the golden-thread cache, no network"
        );
        // A different URL is a distinct cache entry → one more network call.
        assert_eq!(linkcheck(&kernel, "https://example.com/b"), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
