//! Configuration for the CMS bins — `cms.toml` in the ikigai config home plus CLI flags,
//! flags winning. Where that home IS belongs to `ikigai_core::config`, not here
//! (`$XDG_CONFIG_HOME/ikigai`, else `~/.config/ikigai`); this crate used to join
//! `~/.config/ikigai` itself and was the one of four ikigai config readers that ignored the
//! variable. No environment variables of our own: the file states the durable posture (ports,
//! source paths, which passes run), a flag overrides it for one run.
//!
//! Fail-loud rules: a config file that exists but does not parse (or carries an unknown
//! key) is an error, never a silent fallback to defaults; a path that was *explicitly*
//! configured but does not exist is an error. Only the built-in default locations probe
//! quietly (a machine without a Zotero library simply has no books).
//!
//! The same rule covers `bind` and `rp_origin` together, and that pair is the one place in this
//! file where a *legal-looking* combination has to be refused: `http://localhost` is a **secure
//! context** and `http://192.168.1.5` is not, so a room bound to a non-loopback address and
//! served over plain HTTP has a passkey gate that cannot function at all. See
//! [`check_reachable`] — a room whose front door can never open must not start.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The raw, all-optional shape both the TOML file and the CLI flags fill.
/// `None` = "not stated" — [`Raw::resolve`] applies defaults and validates.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    /// The WebTransport (QUIC) port the wire listens on. Internal — the page reads it
    /// from `cert.json`.
    wire_port: Option<u16>,
    /// The reading-room page port (the URL you open). The passkey RP origin follows it.
    page_port: Option<u16>,
    /// The address the reading-room page listens on. Default `127.0.0.1` — loopback, the only
    /// bind that is a secure context over plain HTTP. Anything wider requires an `https`
    /// `rp_origin` (a TLS terminator in front); see [`check_reachable`].
    bind: Option<String>,
    /// Extra subject-alternative names for the WebTransport certificate, on top of the ones
    /// derived from `bind` and `rp_origin`. Only needed when the browser dials the wire at a
    /// name neither of those states (a host with several addresses, say).
    cert_sans: Option<Vec<String>>,
    /// The CMS source jail (org files). Default `~/Dropbox/org-mode-files`.
    src_dir: Option<String>,
    /// The bookmarks org file, as a sub-path under `src_dir`.
    bookmarks: Option<String>,
    /// The Zotero RDF export (books). Default probes
    /// `~/Dropbox/Documents/Zotero/My Library.rdf`.
    zotero: Option<String>,
    /// The lectern presentations root. Default probes `~/git-personal/lectern-presentations`.
    presentations: Option<String>,
    /// Base URL a static server exposes the presentations tree at; `""` = file:// links.
    deck_base: Option<String>,
    /// WebAuthn relying-party id (default `localhost`).
    rp_id: Option<String>,
    /// WebAuthn relying-party origin (default `http://localhost:{page_port}`).
    rp_origin: Option<String>,
    /// The directory the reading-room page is served from.
    dist: Option<String>,
    /// Ungate the HTTP face for localhost dev (no passkey). Default off.
    dev_open: Option<bool>,
    /// Run the daily link-check pass in cms-server. Default off.
    linkcheck: Option<bool>,
    /// Run the daily tag-suggest pass in cms-server. Default off.
    tagsuggest: Option<bool>,
    /// The link-status cache path (default: the maintenance module's).
    linkstatus: Option<String>,
    /// Which `llm.json` provider the maintenance passes use (default: that registry's
    /// own default). Validated against the registry at kernel build, not here.
    llm_provider: Option<String>,
    /// The approved-tag overlay path (default `~/.ikigai/cms-tags-approved.ttl`).
    tags_approved: Option<String>,
    /// The tag-suggestions overlay path (default `~/.ikigai/cms-tag-suggestions.ttl`).
    tags_suggestions: Option<String>,
    /// The dismissed-tag overlay path (default `~/.ikigai/cms-tag-dismissed.ttl`).
    tags_dismissed: Option<String>,
    /// The Zotero link overlay path (default `~/.ikigai/cms-zotero-links.ttl`) — written by the
    /// `cms-zotero-links` pass, read by the books graph.
    zotero_links: Option<String>,
}

/// The resolved configuration the bins consume.
#[derive(Debug)]
pub struct CmsConfig {
    pub wire_port: u16,
    pub page_port: u16,
    /// Where the reading-room page listens. Validated against `rp_origin` at load
    /// ([`check_reachable`]): a non-loopback bind is only legal behind an `https` origin.
    pub bind: IpAddr,
    pub src_dir: PathBuf,
    pub bookmarks: Option<String>,
    pub zotero: Option<PathBuf>,
    pub presentations: Option<PathBuf>,
    /// `None` = file:// links (an explicit empty `deck_base`).
    pub deck_base: Option<String>,
    pub rp_id: String,
    pub rp_origin: String,
    pub dist: PathBuf,
    pub dev_open: bool,
    pub linkcheck: bool,
    pub tagsuggest: bool,
    pub linkstatus: Option<PathBuf>,
    pub llm_provider: Option<String>,
    /// The tag-overlay store (approved / suggestions / dismissed), resolved to explicit
    /// paths — threaded into the kernels so nothing reads process-global state.
    pub tags: crate::tagstore::TagPaths,
    /// Operator-stated extra certificate SANs, on top of the derived ones. See
    /// [`CmsConfig::cert_sans`].
    pub extra_cert_sans: Vec<String>,
    /// Flag-only (`--limit N`): cap a maintenance pass's fan-out. Never in the file.
    pub limit: Option<String>,
}

impl CmsConfig {
    /// The subject-alternative names the WebTransport certificate must carry.
    ///
    /// **The SANs follow the bind.** The browser dials the wire by whatever host the page came
    /// from; a certificate that does not name that host is rejected, and the failure surfaces in
    /// the browser's console nowhere near the config line that caused it. So the list is
    /// derived, not written by hand: the loopback names always (the page is reached that way in
    /// dev, over an ssh tunnel, and from a same-host proxy), plus the bind address when it is a
    /// specific non-loopback one, plus the `rp_origin` host when that is not a loopback name
    /// (the reverse-proxy case), plus anything `cert_sans` adds.
    ///
    /// With the defaults this is exactly `["localhost", "127.0.0.1", "::1"]` — the list that was
    /// compiled into the server before it was derived.
    pub fn cert_sans(&self) -> Vec<String> {
        let mut sans: Vec<String> = ["localhost", "127.0.0.1", "::1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut push = |name: String| {
            if !name.is_empty() && !sans.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
                sans.push(name);
            }
        };
        // An unspecified bind (0.0.0.0 / ::) names no address to certify — the operator has to
        // say which one the browser dials, via rp_origin or cert_sans.
        if !self.bind.is_loopback() && !self.bind.is_unspecified() {
            push(self.bind.to_string());
        }
        if let Ok((_, host)) = parse_origin(&self.rp_origin) {
            if !is_loopback_host(&host) {
                push(host);
            }
        }
        for name in &self.extra_cert_sans {
            push(name.clone());
        }
        sans
    }

    /// The host the page should dial the WebTransport wire at, written into `cert.json`.
    /// `None` = "the page's own hostname" — the right answer whenever the page is reached at
    /// some other name (a proxy) or the bind names no single address.
    ///
    /// With the defaults this is `Some("127.0.0.1")`, the literal the page used to hard-code.
    pub fn wire_dial_host(&self) -> Option<String> {
        let origin_is_local = parse_origin(&self.rp_origin)
            .map(|(_, host)| is_loopback_host(&host))
            .unwrap_or(false);
        if !origin_is_local || self.bind.is_unspecified() {
            return None;
        }
        Some(self.bind.to_string())
    }

    /// A startup warning for the one legal arrangement that still has a sharp edge: a
    /// non-loopback bind fronted by a TLS terminator. The proxy's origin works; the bind address
    /// reached directly over plain HTTP does not, and nothing about a running server says so.
    /// `None` when the room is loopback-only.
    pub fn exposure_note(&self) -> Option<String> {
        if self.bind.is_loopback() {
            return None;
        }
        Some(format!(
            "note: bind {} is reachable from the network and this server speaks PLAIN HTTP.\n      \
             A browser reaching http://{}:{} directly is not in a secure context, so\n      \
             its sign-in cannot complete. The only working front door is your TLS\n      \
             terminator: {}",
            self.bind,
            display_host(self.bind),
            self.page_port,
            self.rp_origin,
        ))
    }
}

/// `[::1]`-style bracketing for an IPv6 literal in a URL.
fn display_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// Load the configuration: `cms.toml` in the ikigai config home (or `--config <path>`) merged
/// under the remaining CLI flags. See the module docs for the fail-loud rules.
pub fn load(args: impl Iterator<Item = String>) -> Result<CmsConfig, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    load_with_home(std::env::var_os("XDG_CONFIG_HOME"), &home, args)
}

/// [`load`] with the environment passed in — the injected seam every test uses. Both variables
/// arrive as arguments because the process environment is global: a test that let an ambient
/// `XDG_CONFIG_HOME` reach in would read the developer's real config home instead of its tempdir.
fn load_with_home(
    xdg: Option<std::ffi::OsString>,
    home: &Path,
    args: impl Iterator<Item = String>,
) -> Result<CmsConfig, String> {
    let mut args: Vec<String> = args.collect();

    // --config first: it decides which file the rest of the flags override.
    let config_path = match take_valued_flag(&mut args, "--config")? {
        Some(p) => {
            let p = expand(home, &p);
            if !p.is_file() {
                return Err(format!("--config {}: no such file", p.display()));
            }
            Some(p)
        }
        None => ikigai_core::config::config_home_from(xdg, Some(home.as_os_str().to_os_string()))
            .map(|dir| dir.join("cms.toml"))
            .filter(|p| p.is_file()),
    };

    let mut raw = match &config_path {
        Some(p) => {
            let text =
                std::fs::read_to_string(p).map_err(|e| format!("reading {}: {e}", p.display()))?;
            toml::from_str::<Raw>(&text).map_err(|e| format!("parsing {}: {e}", p.display()))?
        }
        None => Raw::default(),
    };

    let limit = take_valued_flag(&mut args, "--limit")?;
    apply_flags(&mut raw, &args)?;
    let mut cfg = resolve(home, raw)?;
    cfg.limit = limit;
    Ok(cfg)
}

/// Pull `--flag <value>` out of `args` (so it can be handled ahead of, or apart from,
/// the generic pass). Errors if the flag is present without a value.
fn take_valued_flag(args: &mut Vec<String>, flag: &str) -> Result<Option<String>, String> {
    match args.iter().position(|a| a == flag) {
        Some(i) if i + 1 < args.len() => {
            args.remove(i);
            Ok(Some(args.remove(i)))
        }
        Some(_) => Err(format!("{flag} needs a value")),
        None => Ok(None),
    }
}

fn apply_flags(raw: &mut Raw, args: &[String]) -> Result<(), String> {
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        // Boolean flags take no value.
        match flag.as_str() {
            "--dev-open" => {
                raw.dev_open = Some(true);
                continue;
            }
            "--linkcheck" => {
                raw.linkcheck = Some(true);
                continue;
            }
            "--tagsuggest" => {
                raw.tagsuggest = Some(true);
                continue;
            }
            "--help" | "-h" => return Err(USAGE.to_string()),
            _ => {}
        }
        let value = it
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?
            .clone();
        match flag.as_str() {
            "--wire-port" => raw.wire_port = Some(parse_port(flag, &value)?),
            "--page-port" => raw.page_port = Some(parse_port(flag, &value)?),
            "--bind" => raw.bind = Some(value),
            // Additive, and repeatable: `--cert-san a --cert-san b` adds both, on top of
            // anything the file states. An extra SAN only ever widens what the cert names.
            "--cert-san" => raw.cert_sans.get_or_insert_with(Vec::new).push(value),
            "--src-dir" => raw.src_dir = Some(value),
            "--bookmarks" => raw.bookmarks = Some(value),
            "--zotero" => raw.zotero = Some(value),
            "--presentations" => raw.presentations = Some(value),
            "--deck-base" => raw.deck_base = Some(value),
            "--rp-id" => raw.rp_id = Some(value),
            "--rp-origin" => raw.rp_origin = Some(value),
            "--dist" => raw.dist = Some(value),
            "--linkstatus" => raw.linkstatus = Some(value),
            "--llm-provider" => raw.llm_provider = Some(value),
            "--tags-approved" => raw.tags_approved = Some(value),
            "--tags-suggestions" => raw.tags_suggestions = Some(value),
            "--tags-dismissed" => raw.tags_dismissed = Some(value),
            "--zotero-links" => raw.zotero_links = Some(value),
            _ => return Err(format!("unknown flag {flag}\n{USAGE}")),
        }
    }
    Ok(())
}

fn parse_port(flag: &str, value: &str) -> Result<u16, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} {value}: not a port"))
}

/// A bind address: an IP literal, or `localhost` as a spelling of `127.0.0.1`. A hostname is
/// deliberately *not* accepted — a bind is an address on this machine, and resolving a name to
/// pick one would silently choose which interface the room is exposed on.
fn parse_bind(value: &str) -> Result<IpAddr, String> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    v.parse().map_err(|_| {
        format!(
            "bind {value}: not an IP address (use 127.0.0.1 for loopback, 0.0.0.0 for every \
             interface, or a specific address of this host — not a hostname)"
        )
    })
}

/// The `(scheme, host)` of an origin like `https://room.example.com` or `http://localhost:8080`.
///
/// Strict on purpose: an origin is a scheme, a host and an optional port. A path, userinfo, or a
/// missing host is a config error rather than something to trim off — *a bound must refuse, not
/// truncate*, and an rp_origin the browser reads differently from us is exactly the failure this
/// check exists to prevent.
fn parse_origin(origin: &str) -> Result<(String, String), String> {
    let (scheme, rest) = origin.split_once("://").ok_or_else(|| {
        format!("rp_origin {origin}: not an origin (expected scheme://host[:port])")
    })?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "rp_origin {origin}: scheme must be http or https, not {scheme}"
        ));
    }
    if rest.contains('/') {
        return Err(format!(
            "rp_origin {origin}: an origin carries no path — use {scheme}://<host>[:port]"
        ));
    }
    if rest.contains('@') {
        return Err(format!("rp_origin {origin}: an origin carries no userinfo"));
    }
    let host = match rest.strip_prefix('[') {
        // An IPv6 host is bracketed in a URL: [::1]:8080.
        Some(after) => {
            let end = after
                .find(']')
                .ok_or_else(|| format!("rp_origin {origin}: unterminated [IPv6] host"))?;
            after[..end].to_string()
        }
        None => rest.split(':').next().unwrap_or_default().to_string(),
    };
    if host.is_empty() {
        return Err(format!(
            "rp_origin {origin}: no host (an IPv6 literal must be bracketed: http://[::1]:8080)"
        ));
    }
    Ok((scheme, host))
}

/// Does the browser treat plain `http` at this host as a **secure context**? The W3C
/// potentially-trustworthy rule: `localhost` and `*.localhost`, plus the loopback IP ranges
/// (`127.0.0.0/8`, `::1`). Nothing else — a private LAN address is *not* trustworthy to a
/// browser, which is the whole point of [`check_reachable`].
fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    h.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Refuse to start a room whose front door can never open.
///
/// WebAuthn runs only in a secure context, so outside one the passkey gate is not merely weaker
/// — it is **inoperable**: nobody can register, nobody can sign in. A silent start is the worst
/// outcome, because the deploy looks healthy right up until the first sign-in attempt.
///
/// The legal combinations, and why:
///
/// - **loopback bind + `http://<loopback>` origin** — legal; today's default. Loopback over
///   plain http *is* a secure context. This is also the ssh-tunnel deployment: the client still
///   sees `http://localhost`, so no passkey is disturbed.
/// - **loopback bind + `https://…` origin** — legal; a TLS reverse proxy on this host, the
///   safest proxied arrangement (nothing but the proxy can reach the room).
/// - **non-loopback bind + `https://…` origin** — legal, with a warning
///   ([`CmsConfig::exposure_note`]); a TLS terminator elsewhere reaches this bind.
/// - **non-loopback bind + `http://…` origin** — refused. The room is reachable at a plain-http
///   non-loopback URL, where WebAuthn cannot run at all.
/// - **any bind + `http://<non-loopback>` origin** — refused. The stated origin is not a secure
///   context whatever the bind, so the ceremony could never complete there.
///
/// Plus: `dev_open` needs a loopback bind. It ungates the HTTP face entirely (no passkey), so on
/// a network-reachable bind it would serve the whole room to anyone who can route here. That one
/// is not a WebAuthn problem — it is the same class of mistake with a worse blast radius, and it
/// was a comment ("localhost dev only") that nothing enforced.
fn check_reachable(
    bind: IpAddr,
    rp_origin: &str,
    page_port: u16,
    dev_open: bool,
) -> Result<(), String> {
    let (scheme, host) = parse_origin(rp_origin)?;
    let exposed = !bind.is_loopback();

    if dev_open && exposed {
        return Err(format!(
            "dev_open with bind {bind}: refusing to start.\n\n\
             dev_open ungates the HTTP face — no passkey at all — and {bind} is reachable from \
             the network,\nso this would serve the whole reading room to anyone who can route to \
             this host.\nIt is a localhost-development switch: drop dev_open, or set \
             bind = \"127.0.0.1\"."
        ));
    }

    if scheme == "https" {
        // The operator has arranged a genuinely secure origin in front. Whether the TLS
        // terminator is real is theirs to get right; we cannot check it from here.
        return Ok(());
    }

    if !is_loopback_host(&host) {
        return Err(format!(
            "rp_origin {rp_origin} is plain http on a non-loopback host: refusing to start.\n\n\
             WebAuthn only runs in a SECURE CONTEXT — https, or http on \
             localhost/127.0.0.1/::1.\nAs stated, no browser could complete the passkey ceremony \
             at this origin, so the room\nwould start with a front door that can never open.\n\n\
             Either set rp_origin to the http://localhost:{page_port} page you actually open, \
             or — if a\nTLS reverse proxy fronts this room — give its https origin and the \
             matching rp_id.\n⚠ A passkey is bound to its rp_id: changing rp_id INVALIDATES \
             EVERY ENROLLED PASSKEY, and\n  they must all be re-enrolled. It is a one-way door; \
             choose the hostname once."
        ));
    }

    if exposed {
        return Err(reachability_refusal(bind, rp_origin, &host, page_port));
    }

    Ok(())
}

/// The refusal an operator actually hits: they moved `bind` off loopback to reach the room from
/// another machine. It carries the answer, not just the "no" — the two arrangements that work,
/// and which of them is a one-way door.
fn reachability_refusal(bind: IpAddr, rp_origin: &str, host: &str, page_port: u16) -> String {
    let addr = display_host(bind);
    format!(
        "bind {bind} is not a loopback address and rp_origin {rp_origin} is plain http: \
         refusing to start.\n\n\
         WebAuthn only runs in a SECURE CONTEXT. http://{host} is one; http://{addr} is not.\n\
         So a room reached at http://{addr}:{page_port} has a passkey gate that cannot function \
         AT ALL —\nnot \"less secure\", inoperable: nobody can register and nobody can sign in. \
         Starting anyway\nwould look like a healthy deploy right up until the first sign-in \
         attempt.\n\n\
         Two arrangements do work:\n\n\
         1. SSH TUNNEL — keeps localhost, and keeps every passkey already enrolled.\n\
         \x20  Leave bind = \"127.0.0.1\" and forward the page port from the client machine:\n\
         \x20      ssh -N -L {page_port}:127.0.0.1:{page_port} <this-host>\n\
         \x20  The browser still sees http://localhost:{page_port}, so rp_id and rp_origin do not \
         change\n\x20  and no credential is invalidated. (The WebTransport wire is QUIC/UDP and \
         does not ride\n\x20  an `ssh -L` tunnel; the room's HTTP face serves the whole reading \
         room on its own.)\n\n\
         2. REVERSE PROXY terminating TLS in front — ⚠ A ONE-WAY DOOR.\n\
         \x20  Point a proxy you run at this room and set both:\n\
         \x20      rp_id     = \"room.example.com\"\n\
         \x20      rp_origin = \"https://room.example.com\"\n\
         \x20  A passkey is bound to its rp_id. Changing rp_id INVALIDATES EVERY ENROLLED \
         PASSKEY:\n\x20  every user must re-enroll against the new origin, and the old \
         credentials cannot be\n\x20  recovered or migrated. Choose the hostname once.\n\n\
         This server never terminates TLS itself, by design — option 2 needs a proxy in front."
    )
}

fn resolve(home: &Path, raw: Raw) -> Result<CmsConfig, String> {
    let src_dir = match &raw.src_dir {
        Some(s) => {
            let p = expand(home, s);
            if !p.is_dir() {
                return Err(format!("src_dir {}: no such directory", p.display()));
            }
            p
        }
        None => {
            let p = home.join("Dropbox/org-mode-files");
            if !p.is_dir() {
                return Err(format!(
                    "default src_dir {} missing — set src_dir in cms.toml or --src-dir",
                    p.display()
                ));
            }
            p
        }
    };

    // Explicitly configured ⇒ must exist; the built-in default location probes quietly.
    let zotero = probe_path(home, raw.zotero.as_deref(), "zotero", Path::is_file, || {
        home.join("Dropbox/Documents/Zotero/My Library.rdf")
    })?;
    let presentations = probe_path(
        home,
        raw.presentations.as_deref(),
        "presentations",
        Path::is_dir,
        || home.join("git-personal/lectern-presentations"),
    )?;

    // The tag-overlay store: the ikigai state dir by default, each file overridable
    // (they're outputs — created on first write, so no existence probe). A home that names
    // nothing has no state dir; fail loud with the same words `load` uses rather than write
    // the overlays to a relative `.ikigai` nothing will read back.
    let mut tags = crate::tagstore::TagPaths::in_state_dir(home).ok_or("HOME is not set")?;
    if let Some(s) = raw.tags_approved {
        tags.approved = expand(home, &s);
    }
    if let Some(s) = raw.tags_suggestions {
        tags.suggestions = expand(home, &s);
    }
    if let Some(s) = raw.tags_dismissed {
        tags.dismissed = expand(home, &s);
    }
    if let Some(s) = raw.zotero_links {
        tags.zotero_links = expand(home, &s);
    }

    let page_port = raw.page_port.unwrap_or(8080);
    let bind = match &raw.bind {
        Some(s) => parse_bind(s)?,
        None => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    let rp_origin = raw
        .rp_origin
        .unwrap_or_else(|| format!("http://localhost:{page_port}"));
    let dev_open = raw.dev_open.unwrap_or(false);
    // Where the room is reachable and where the passkey ceremony can run are one decision, not
    // two: refuse the pairs that cannot work rather than start a room nobody can sign in to.
    check_reachable(bind, &rp_origin, page_port, dev_open)?;

    Ok(CmsConfig {
        wire_port: raw.wire_port.unwrap_or(4433),
        page_port,
        bind,
        src_dir,
        bookmarks: raw.bookmarks,
        zotero,
        presentations,
        deck_base: match raw.deck_base {
            Some(s) if s.is_empty() => None,
            Some(s) => Some(s),
            None => Some("http://localhost:8000".to_string()),
        },
        rp_id: raw.rp_id.unwrap_or_else(|| "localhost".to_string()),
        rp_origin,
        dist: raw
            .dist
            .map(|s| expand(home, &s))
            .unwrap_or_else(|| PathBuf::from("dist")),
        dev_open,
        linkcheck: raw.linkcheck.unwrap_or(false),
        tagsuggest: raw.tagsuggest.unwrap_or(false),
        linkstatus: raw.linkstatus.map(|s| expand(home, &s)),
        llm_provider: raw.llm_provider,
        tags,
        extra_cert_sans: raw.cert_sans.unwrap_or_default(),
        limit: None,
    })
}

fn probe_path(
    home: &Path,
    explicit: Option<&str>,
    what: &str,
    exists: fn(&Path) -> bool,
    default: impl FnOnce() -> PathBuf,
) -> Result<Option<PathBuf>, String> {
    match explicit {
        Some(s) => {
            let p = expand(home, s);
            if exists(&p) {
                Ok(Some(p))
            } else {
                Err(format!("{what} {}: does not exist", p.display()))
            }
        }
        None => Ok(Some(default()).filter(|p| exists(p))),
    }
}

/// Expand a leading `~/` to the home directory.
fn expand(home: &Path, s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(s),
    }
}

const USAGE: &str = "\
usage: cms-server [flags]   (also cms-linkcheck / cms-tag-suggest)
config: cms.toml in the ikigai config home ($XDG_CONFIG_HOME/ikigai, else
        ~/.config/ikigai) — flags override it, one run at a time
  --config <path>          use a different config file
  --wire-port <port>       WebTransport port (default 4433)
  --page-port <port>       reading-room page port (default 8080)
  --bind <addr>            page listen address (default 127.0.0.1). A non-loopback
                           bind needs an https rp_origin — plain http off loopback
                           is not a secure context, so WebAuthn cannot run there
  --cert-san <name>        extra WebTransport cert SAN (repeatable); the bind and
                           rp_origin hosts are already included
  --src-dir <dir>          CMS source jail (default ~/Dropbox/org-mode-files)
  --bookmarks <sub-path>   bookmarks org file under src-dir
  --zotero <file>          Zotero RDF export (books)
  --presentations <dir>    lectern presentations root
  --deck-base <url>        deck static-server base URL ('' = file:// links)
  --rp-id <id>             WebAuthn relying-party id
  --rp-origin <origin>     WebAuthn relying-party origin
  --dist <dir>             reading-room page directory (default ./dist)
  --dev-open               ungate the HTTP face (localhost dev)
  --linkcheck              run the daily link-check pass
  --tagsuggest             run the daily tag-suggest pass
  --linkstatus <file>      link-status cache path
  --llm-provider <name>    llm.json provider for the maintenance passes
  --tags-approved <file>   approved-tag overlay (default ~/.ikigai/cms-tags-approved.ttl)
  --tags-suggestions <file> tag-suggestions overlay (default ~/.ikigai/cms-tag-suggestions.ttl)
  --tags-dismissed <file>  dismissed-tag overlay (default ~/.ikigai/cms-tag-dismissed.ttl)
  --zotero-links <file>    Zotero link overlay (default ~/.ikigai/cms-zotero-links.ttl)
  --limit <n>              cap a maintenance pass (cms-linkcheck / cms-tag-suggest)";

#[cfg(test)]
mod tests {
    use super::*;

    /// A home dir with the default source jail present, so `resolve` has its floor.
    fn fake_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join("Dropbox/org-mode-files")).unwrap();
        home
    }

    /// No `XDG_CONFIG_HOME`: the config home is `{home}/.config/ikigai`, the shape every
    /// test below was written against.
    fn load_args(home: &Path, args: &[&str]) -> Result<CmsConfig, String> {
        load_with_home(None, home, args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_apply_without_a_file() {
        let home = fake_home();
        let cfg = load_args(home.path(), &[]).expect("defaults load");
        assert_eq!(cfg.wire_port, 4433);
        assert_eq!(cfg.page_port, 8080);
        assert_eq!(cfg.rp_origin, "http://localhost:8080");
        assert_eq!(cfg.deck_base.as_deref(), Some("http://localhost:8000"));
        assert!(!cfg.dev_open);
    }

    #[test]
    fn file_sets_and_flags_override() {
        let home = fake_home();
        let dir = home.path().join(".config/ikigai");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cms.toml"),
            "page_port = 8090\nwire_port = 4434\nbookmarks = \"bookmarks-src.org\"\n",
        )
        .unwrap();
        let cfg = load_args(home.path(), &[]).expect("file load");
        assert_eq!((cfg.page_port, cfg.wire_port), (8090, 4434));
        assert_eq!(cfg.bookmarks.as_deref(), Some("bookmarks-src.org"));
        // The RP origin follows the configured page port.
        assert_eq!(cfg.rp_origin, "http://localhost:8090");

        let cfg = load_args(home.path(), &["--page-port", "9001"]).expect("flag load");
        assert_eq!(cfg.page_port, 9001);
        assert_eq!(cfg.wire_port, 4434); // file value survives beside the flag
    }

    #[test]
    fn tag_overlay_paths_default_to_the_state_dir_and_override_per_file() {
        let home = fake_home();
        let cfg = load_args(home.path(), &[]).expect("defaults load");
        assert_eq!(
            cfg.tags.approved,
            home.path().join(".ikigai/cms-tags-approved.ttl")
        );
        assert_eq!(
            cfg.tags.suggestions,
            home.path().join(".ikigai/cms-tag-suggestions.ttl")
        );
        assert_eq!(
            cfg.tags.dismissed,
            home.path().join(".ikigai/cms-tag-dismissed.ttl")
        );

        // A file key moves one overlay; a flag moves another; the third keeps its default.
        let dir = home.path().join(".config/ikigai");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cms.toml"), "tags_approved = \"~/tags/a.ttl\"\n").unwrap();
        let cfg = load_args(home.path(), &["--tags-suggestions", "~/tags/s.ttl"]).expect("load");
        assert_eq!(cfg.tags.approved, home.path().join("tags/a.ttl"));
        assert_eq!(cfg.tags.suggestions, home.path().join("tags/s.ttl"));
        assert_eq!(
            cfg.tags.dismissed,
            home.path().join(".ikigai/cms-tag-dismissed.ttl")
        );
    }

    /// The default config file comes from `ikigai_core::config`, not from a local
    /// `{home}/.config/ikigai` join — so an `XDG_CONFIG_HOME` moves it. This asserts only
    /// that cms-web *calls* the rule; what the rule resolves to is core's own tested
    /// contract. Injected, not `set_var`: the process environment is global.
    #[test]
    fn the_config_home_is_the_one_from_core() {
        let home = fake_home();
        let xdg = tempfile::tempdir().unwrap();
        let dir = xdg.path().join("ikigai");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cms.toml"), "page_port = 8123\n").unwrap();

        // The file under XDG_CONFIG_HOME is the one that is read...
        let cfg = load_with_home(
            Some(xdg.path().as_os_str().to_os_string()),
            home.path(),
            std::iter::empty(),
        )
        .expect("xdg load");
        assert_eq!(cfg.page_port, 8123);

        // ...and it is not merely the fallback finding nothing: `{home}/.config/ikigai` holds
        // a different port, and the XDG file still wins.
        let under_home = home.path().join(".config/ikigai");
        std::fs::create_dir_all(&under_home).unwrap();
        std::fs::write(under_home.join("cms.toml"), "page_port = 8456\n").unwrap();
        let cfg = load_with_home(
            Some(xdg.path().as_os_str().to_os_string()),
            home.path(),
            std::iter::empty(),
        )
        .expect("xdg load");
        assert_eq!(cfg.page_port, 8123);
        assert_eq!(
            load_args(home.path(), &[]).expect("home load").page_port,
            8456
        );
    }

    #[test]
    fn unknown_key_fails_loud() {
        let home = fake_home();
        let dir = home.path().join(".config/ikigai");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cms.toml"), "prot = 8090\n").unwrap();
        let err = load_args(home.path(), &[]).unwrap_err();
        assert!(err.contains("cms.toml"), "names the file: {err}");
    }

    #[test]
    fn unknown_flag_fails_loud() {
        let home = fake_home();
        let err = load_args(home.path(), &["--prot", "8090"]).unwrap_err();
        assert!(err.contains("unknown flag --prot"), "{err}");
    }

    #[test]
    fn explicit_missing_path_fails_probe_default_is_quiet() {
        let home = fake_home();
        // Explicit zotero that doesn't exist → error.
        let err = load_args(home.path(), &["--zotero", "~/nope.rdf"]).unwrap_err();
        assert!(err.contains("nope.rdf"), "{err}");
        // No zotero stated and the default absent → quietly None.
        let cfg = load_args(home.path(), &[]).expect("load");
        assert_eq!(cfg.zotero, None);
    }

    /// The default is loopback and its certificate names exactly what the compiled-in list
    /// used to: a reader who changes nothing sees no difference at all.
    #[test]
    fn the_default_bind_and_sans_reproduce_the_compiled_in_behavior() {
        let home = fake_home();
        let cfg = load_args(home.path(), &[]).expect("defaults load");
        assert_eq!(cfg.bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(cfg.cert_sans(), ["localhost", "127.0.0.1", "::1"]);
        assert_eq!(cfg.wire_dial_host().as_deref(), Some("127.0.0.1"));
        assert_eq!(cfg.exposure_note(), None);
    }

    /// THE refusal. Its entire value is that it fires: a non-loopback bind over plain http is a
    /// room whose passkey gate cannot run, and starting it looks healthy until someone tries to
    /// sign in.
    #[test]
    fn a_non_loopback_bind_over_plain_http_is_refused_with_both_ways_out() {
        let home = fake_home();
        let err = load_args(home.path(), &["--bind", "192.168.1.20"]).unwrap_err();
        assert!(err.contains("192.168.1.20"), "names the bind: {err}");
        assert!(err.contains("SECURE CONTEXT"), "names the reason: {err}");
        // The message carries the answer, not just the "no".
        assert!(err.contains("SSH TUNNEL"), "offers the tunnel: {err}");
        assert!(err.contains("ssh -N -L 8080:127.0.0.1:8080"), "{err}");
        assert!(err.contains("REVERSE PROXY"), "offers the proxy: {err}");
        assert!(
            err.contains("INVALIDATES EVERY ENROLLED PASSKEY"),
            "states the one-way door: {err}"
        );
        // And it fires from the config file just the same, not only from the flag.
        let dir = home.path().join(".config/ikigai");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cms.toml"), "bind = \"0.0.0.0\"\n").unwrap();
        let err = load_args(home.path(), &[]).unwrap_err();
        assert!(err.contains("SECURE CONTEXT"), "{err}");
    }

    /// The other half of the same rule: an origin that is not a secure context is refused
    /// whatever the bind — a loopback room whose rp_origin claims a plain-http LAN host has the
    /// same unusable front door.
    #[test]
    fn a_plain_http_non_loopback_origin_is_refused_even_on_a_loopback_bind() {
        let home = fake_home();
        let err = load_args(home.path(), &["--rp-origin", "http://room.example.com"]).unwrap_err();
        assert!(err.contains("SECURE CONTEXT"), "{err}");
        assert!(
            err.contains("INVALIDATES EVERY ENROLLED PASSKEY"),
            "points at the proxy answer and its cost: {err}"
        );
    }

    /// Both legal proxy shapes start, and the exposed one warns rather than refusing.
    #[test]
    fn an_https_origin_makes_a_non_loopback_bind_legal() {
        let home = fake_home();
        // Same-host proxy: loopback bind, https origin. No warning — nothing is exposed.
        let cfg = load_args(
            home.path(),
            &[
                "--rp-origin",
                "https://room.example.com",
                "--rp-id",
                "room.example.com",
            ],
        )
        .expect("loopback behind TLS is legal");
        assert_eq!(cfg.exposure_note(), None);
        // The cert follows: the proxy's host is what the page dials the wire at.
        assert!(cfg.cert_sans().contains(&"room.example.com".to_string()));
        // ...and the page is told to dial its own hostname rather than 127.0.0.1.
        assert_eq!(cfg.wire_dial_host(), None);

        // Proxy on another host: exposed bind, https origin — legal, with the warning.
        let cfg = load_args(
            home.path(),
            &[
                "--bind",
                "10.0.0.4",
                "--rp-origin",
                "https://room.example.com",
                "--rp-id",
                "room.example.com",
            ],
        )
        .expect("exposed behind TLS is legal");
        let note = cfg.exposure_note().expect("an exposed bind warns");
        assert!(note.contains("PLAIN HTTP"), "{note}");
        assert!(note.contains("10.0.0.4"), "{note}");
        // The SANs carry both the bind address and the proxy host.
        let sans = cfg.cert_sans();
        assert!(sans.contains(&"10.0.0.4".to_string()), "{sans:?}");
        assert!(sans.contains(&"room.example.com".to_string()), "{sans:?}");
        assert!(sans.contains(&"localhost".to_string()), "{sans:?}");
    }

    /// `dev_open` ungates the HTTP face entirely; off loopback that is the whole room served to
    /// the network. It was a comment before, and a comment enforces nothing.
    #[test]
    fn dev_open_needs_a_loopback_bind() {
        let home = fake_home();
        let err = load_args(
            home.path(),
            &[
                "--bind",
                "0.0.0.0",
                "--rp-origin",
                "https://room.example.com",
                "--dev-open",
            ],
        )
        .unwrap_err();
        assert!(err.contains("dev_open"), "{err}");
        assert!(err.contains("no passkey at all"), "{err}");
        // Loopback + dev_open is the dev posture and stays legal.
        assert!(
            load_args(home.path(), &["--dev-open"])
                .expect("local dev")
                .dev_open
        );
    }

    /// An unspecified bind names no address to certify, so the page must dial its own hostname,
    /// and `cert_sans` is the escape hatch for the name it will use.
    #[test]
    fn an_unspecified_bind_certifies_nothing_of_its_own_and_cert_sans_adds_names() {
        let home = fake_home();
        let cfg = load_args(
            home.path(),
            &[
                "--bind",
                "0.0.0.0",
                "--rp-origin",
                "https://room.example.com",
                "--cert-san",
                "10.0.0.4",
                "--cert-san",
                "room.internal",
            ],
        )
        .expect("load");
        assert_eq!(cfg.wire_dial_host(), None);
        let sans = cfg.cert_sans();
        assert!(!sans.contains(&"0.0.0.0".to_string()), "{sans:?}");
        assert!(sans.contains(&"10.0.0.4".to_string()), "{sans:?}");
        assert!(sans.contains(&"room.internal".to_string()), "{sans:?}");
        // No duplicates, whatever route a name arrived by.
        let mut seen = sans.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), sans.len(), "{sans:?}");
    }

    #[test]
    fn a_malformed_bind_or_origin_fails_loud() {
        let home = fake_home();
        // A hostname is not a bind address — resolving it would silently pick an interface.
        let err = load_args(home.path(), &["--bind", "room.example.com"]).unwrap_err();
        assert!(err.contains("not an IP address"), "{err}");
        // An origin is scheme://host[:port] — no path, no scheme-less string, no other scheme.
        for bad in [
            "room.example.com",
            "https://room.example.com/room",
            "ftp://room.example.com",
        ] {
            let err = load_args(home.path(), &["--rp-origin", bad]).unwrap_err();
            assert!(err.contains(bad), "names the origin: {err}");
        }
    }

    #[test]
    fn loopback_host_classification_matches_the_secure_context_rule() {
        for yes in [
            "localhost",
            "LocalHost.",
            "app.localhost",
            "127.0.0.1",
            "127.1.2.3",
            "::1",
        ] {
            assert!(is_loopback_host(yes), "{yes} is potentially trustworthy");
        }
        for no in [
            "192.168.1.5",
            "10.0.0.4",
            "0.0.0.0",
            "room.example.com",
            "notlocalhost",
        ] {
            assert!(!is_loopback_host(no), "{no} is not");
        }
    }

    #[test]
    fn tilde_expands_and_empty_deck_base_means_file_links() {
        let home = fake_home();
        std::fs::write(home.path().join("lib.rdf"), "x").unwrap();
        let cfg = load_args(
            home.path(),
            &["--zotero", "~/lib.rdf", "--deck-base", "", "--limit", "5"],
        )
        .expect("load");
        assert_eq!(cfg.zotero.as_deref(), Some(&*home.path().join("lib.rdf")));
        assert_eq!(cfg.deck_base, None);
        assert_eq!(cfg.limit.as_deref(), Some("5"));
    }
}
