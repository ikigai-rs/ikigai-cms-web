//! Each scheduled maintenance pass fires under its own authority, never more (ledger #889).
//!
//! The server schedules `urn:cms:linkcheck` and `urn:cms:tag-suggest` on daily `urn:time` jobs.
//! ikigai-time 0.4 fires a job under the capability it was SCHEDULED with (ledger #79), so that
//! argument is the pass's whole authority for every run until the process exits. These tests
//! drive the server's own scheduling through a real `JobRegistry`, fire each job by hand, and read
//! back the capability the tick actually arrived under.
//!
//! What a narrowed capability lets the pass DO (and what the kernel refuses it) is the
//! maintenance module's test, over the real maintenance kernel; this one pins the wiring: that
//! the capability a job fires under is the one the authority names.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ikigai_cms_web::maintenance::PassAuthority;
use ikigai_core::{Capability, Error, ReprType, Representation, Request, SpaceEntry, SystemClock};
use ikigai_resolve::{CacheStatus, Resolver};
use ikigai_time::{JobRegistry, TimerBackend, TimerHandle};

use super::{schedule_pass, scheduled_passes};

/// Captures each job's tick so the test fires it by hand, out of band (as the thread backend
/// does): firing from inside `start` would re-enter the registry's lock.
#[derive(Default)]
struct ManualBackend {
    ticks: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
}

impl TimerBackend for ManualBackend {
    fn start(
        &self,
        _interval: Duration,
        _recurring: bool,
        on_tick: Arc<dyn Fn() + Send + Sync>,
    ) -> TimerHandle {
        self.ticks.lock().unwrap().push(on_tick);
        TimerHandle::new(|| {})
    }
}

/// Records the target and capability of every request it is handed, and answers each with an
/// empty body: what a job fires under, not what the pass does.
#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<(String, Capability)>>,
}

impl Resolver for Recorder {
    fn issue(&self, request: Request) -> Result<(Representation, CacheStatus), Error> {
        self.issue_as(request, &Capability::root())
    }

    fn issue_as(
        &self,
        request: Request,
        capability: &Capability,
    ) -> Result<(Representation, CacheStatus), Error> {
        self.seen
            .lock()
            .unwrap()
            .push((request.target.as_str().to_string(), capability.clone()));
        Ok((
            Representation::new(ReprType::new("text/plain"), Vec::new()),
            CacheStatus::Uncacheable,
        ))
    }

    fn is_cached(&self, _: &Request, _: &Capability) -> bool {
        false
    }

    fn entries(&self) -> Option<Vec<SpaceEntry>> {
        None
    }
}

/// Schedule every pass the server would (both turned on), fire each job once, and return the
/// capability each pass's tick arrived under, by pass IRI.
fn fired(authority: &PassAuthority) -> Vec<(String, Capability)> {
    let backend = Arc::new(ManualBackend::default());
    let registry = JobRegistry::new(Arc::clone(&backend) as _, Arc::new(SystemClock));
    let recorder = Arc::new(Recorder::default());
    registry.set_resolver(Arc::clone(&recorder) as Arc<dyn Resolver>);
    for pass in scheduled_passes(authority, true, true) {
        schedule_pass(&registry, &pass).expect("scheduled");
    }
    for tick in backend.ticks.lock().unwrap().iter() {
        tick();
    }
    let seen = recorder.seen.lock().unwrap().clone();
    seen
}

fn fired_for(authority: &PassAuthority, iri: &str) -> Capability {
    fired(authority)
        .into_iter()
        .find_map(|(target, cap)| (target == iri).then_some(cap))
        .unwrap_or_else(|| panic!("{iri} never fired"))
}

fn authority(src_dir: &Path) -> PassAuthority {
    PassAuthority::new(src_dir, &ikigai_llm::OpenAiConfig::ollama("llama3.2"))
        .expect("an authority for the default Ollama provider")
}

/// The reproduction: the tag-suggest job used to fire at ROOT, so a daily pass that reads the
/// CMS graph and asks OpenLibrary and the local LLM could read the keystore, write any file the
/// process can, and reach any host. It fires under exactly the three grants it needs.
#[test]
fn the_tag_suggest_job_fires_under_its_own_scopes_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let cap = fired_for(&authority(dir.path()), "urn:cms:tag-suggest");
    assert!(!cap.is_root(), "the tag-suggest job fired at root");
    let expected: std::collections::BTreeSet<String> = [
        format!("urn:cap:fs:read:{}", dir.path().display()),
        "urn:cap:net:openlibrary.org".to_string(),
        "urn:cap:net:localhost:11434".to_string(),
    ]
    .into_iter()
    .collect();
    assert_eq!(cap.scopes(), Some(&expected));

    // What those scopes mean to the net ACL the pass's requests are judged by.
    let reach = |host: &str, port: u16| ikigai_http::net_allows_port(&cap, host, Some(port), "/");
    assert!(reach("openlibrary.org", 443), "OpenLibrary is the pass's");
    assert!(reach("localhost", 11434), "the LLM provider is the pass's");
    assert!(!reach("example.com", 443), "an arbitrary host is not");
    assert!(
        !reach("localhost", 1062),
        "another service on the LLM's host is not"
    );
    assert!(
        !cap.allows("urn:cap:secret:read:zotero-api-key"),
        "the keystore is not the pass's"
    );
}

/// Link-check keeps root, and says why: its hosts are whatever the bookmarks name, and the net
/// ACL has no grant for "any host" short of root (ikigai-http: "there is no wildcard-allow
/// rule"). The maintenance module's test shows what a narrowed link-check would do instead.
#[test]
fn the_link_check_job_fires_at_root_because_its_hosts_are_the_bookmarks() {
    let dir = tempfile::tempdir().unwrap();
    let cap = fired_for(&authority(dir.path()), "urn:cms:linkcheck");
    assert!(cap.is_root());
}
