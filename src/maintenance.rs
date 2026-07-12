//! Graph maintenance: a kernel that composes the CMS graph with **outbound HTTP** (the real
//! `ikigai-http` module), so a maintenance pass can check a bookmark by resolving
//! `urn:httpHead` with `Verb::Exists` — `"true"` (reachable) / `"false"` (gone), or a typed
//! error (unreachable). `ikigai-http` reports the status honestly; the *policy* about what's
//! "dead" lives in the caller (the link-checker).
//!
//! The read is cacheable: the caller passes `max_age`, so in a long-lived host a URL checked
//! within the window is a cache hit. The transport is **async** (reqwest) — it parks on its
//! socket, so thousands of checks run concurrently on one runtime with no thread pool.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ikigai_core::{Fallback, Kernel, Space, SystemClock};
use ikigai_http::{HttpRequest, HttpResponse, HttpTransport};
use std::path::PathBuf;

/// A reqwest-backed async HTTP transport. It performs the request and reports the response
/// **faithfully** (status + headers) — no classification here; `ikigai-http` maps the status
/// onto the typed-error taxonomy, and the link-checker decides what a status means.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// A client with a per-request timeout (a hung host shouldn't stall the pass) and a
    /// polite user-agent. Redirects are followed by default, so the endpoint sees the final
    /// status.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .user_agent("ikigai-cms-linkcheck")
            .build()
            .expect("build reqwest client");
        ReqwestTransport { client }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, req: HttpRequest) -> std::result::Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(req.method.as_str().as_bytes())
            .map_err(|e| e.to_string())?;
        let mut builder = self.client.request(method, &req.url);
        for (name, value) in &req.headers {
            builder = builder.header(name, value);
        }
        if !req.body.is_empty() {
            builder = builder.body(req.body);
        }
        let response = builder.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_string(), v.to_string()))
            })
            .collect();
        let body = response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// A maintenance kernel: the CMS graph spaces + the `ikigai-http` outbound endpoints over
/// `transport`, with a system clock so cacheable reads honor their deadlines. `bookmarks` is the
/// org sub-path to check (`None` = the built-in default). `transport` is injectable so a test can
/// supply a canned one.
pub fn maintenance_kernel(
    src_dir: PathBuf,
    bookmarks: Option<String>,
    transport: Arc<dyn HttpTransport>,
) -> Kernel {
    let mut spaces = crate::cms_spaces_with(src_dir, None, None, bookmarks);
    spaces.push(Arc::new(ikigai_http::space(transport)) as Arc<dyn Space>);
    Kernel::new(Arc::new(Fallback::new(spaces))).with_clock(Arc::new(SystemClock))
}

/// [`maintenance_kernel`] over the real reqwest transport, checking the given bookmarks org
/// sub-path (`None` = the built-in default).
pub fn build_maintenance_kernel(src_dir: PathBuf, bookmarks: Option<String>) -> Kernel {
    maintenance_kernel(src_dir, bookmarks, Arc::new(ReqwestTransport::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikigai_core::{ArgRef, Capability, Clock, Iri, Request, Time, Verb};
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    /// A canned transport returning a fixed status, counting sends — so we can prove existence
    /// mapping and that the caller `max_age` caches the check.
    struct Canned {
        status: u16,
        sends: Arc<AtomicU32>,
    }
    #[async_trait]
    impl HttpTransport for Canned {
        async fn send(&self, _req: HttpRequest) -> std::result::Result<HttpResponse, String> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(HttpResponse {
                status: self.status,
                headers: Vec::new(),
                body: Vec::new(),
            })
        }
    }

    #[derive(Clone)]
    struct TestClock(Arc<AtomicU64>);
    impl Clock for TestClock {
        fn now(&self) -> Time {
            Time::from_millis(self.0.load(Ordering::SeqCst))
        }
    }

    fn kernel_over(status: u16, sends: Arc<AtomicU32>, clock: TestClock) -> Kernel {
        let dir = tempfile::tempdir().unwrap();
        let bm = dir.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(&bm, "* Bookmarks\n").unwrap();
        // Leak the tempdir for the test's lifetime (the kernel reads through it lazily).
        let src = dir.keep();
        let mut spaces = crate::cms_spaces_with(src, None, None, None);
        spaces.push(
            Arc::new(ikigai_http::space(Arc::new(Canned { status, sends }))) as Arc<dyn Space>,
        );
        Kernel::new(Arc::new(Fallback::new(spaces))).with_clock(Arc::new(clock))
    }

    fn head(kernel: &Kernel, url: &str, max_age: &str) -> String {
        let req = Request::new(Verb::Exists, Iri::parse("urn:httpHead").unwrap())
            .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()))
            .with_arg("max_age", ArgRef::Inline(max_age.as_bytes().to_vec()));
        let repr =
            futures::executor::block_on(kernel.issue(req, &Capability::root())).expect("resolves");
        String::from_utf8(repr.bytes).unwrap()
    }

    #[test]
    fn head_exists_reports_reachable_or_gone() {
        let clock = TestClock(Arc::new(AtomicU64::new(0)));
        let alive = kernel_over(200, Arc::new(AtomicU32::new(0)), clock.clone());
        assert_eq!(head(&alive, "https://ok.example/x", "0"), "true");
        let gone = kernel_over(404, Arc::new(AtomicU32::new(0)), clock);
        assert_eq!(head(&gone, "https://gone.example/x", "0"), "false");
    }

    #[test]
    fn a_caller_max_age_caches_the_check_so_a_re_run_skips_the_network() {
        let sends = Arc::new(AtomicU32::new(0));
        let clock = TestClock(Arc::new(AtomicU64::new(0)));
        let kernel = kernel_over(200, sends.clone(), clock.clone());
        // A week-long caller window: the second check inside it is a cache hit (no send).
        let week = (7 * 24 * 60 * 60).to_string();
        assert_eq!(head(&kernel, "https://ok.example/x", &week), "true");
        assert_eq!(head(&kernel, "https://ok.example/x", &week), "true");
        assert_eq!(
            sends.load(Ordering::SeqCst),
            1,
            "the caller max_age cached the reachability check"
        );
        // Past the window → re-checked.
        clock
            .0
            .store((8 * 24 * 60 * 60 * 1000) as u64, Ordering::SeqCst);
        assert_eq!(head(&kernel, "https://ok.example/x", &week), "true");
        assert_eq!(
            sends.load(Ordering::SeqCst),
            2,
            "re-checked after the window"
        );
    }
}
