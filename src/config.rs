//! Configuration for the CMS bins — the config home (`~/.config/ikigai/cms.toml`) plus
//! CLI flags, flags winning. No environment variables: the file states the durable
//! posture (ports, source paths, which passes run), a flag overrides it for one run.
//!
//! Fail-loud rules: a config file that exists but does not parse (or carries an unknown
//! key) is an error, never a silent fallback to defaults; a path that was *explicitly*
//! configured but does not exist is an error. Only the built-in default locations probe
//! quietly (a machine without a Zotero library simply has no books).

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
}

/// The resolved configuration the bins consume.
#[derive(Debug)]
pub struct CmsConfig {
    pub wire_port: u16,
    pub page_port: u16,
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
    /// Flag-only (`--limit N`): cap a maintenance pass's fan-out. Never in the file.
    pub limit: Option<String>,
}

/// Load the configuration: `~/.config/ikigai/cms.toml` (or `--config <path>`) merged
/// under the remaining CLI flags. See the module docs for the fail-loud rules.
pub fn load(args: impl Iterator<Item = String>) -> Result<CmsConfig, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    load_with_home(&home, args)
}

fn load_with_home(home: &Path, args: impl Iterator<Item = String>) -> Result<CmsConfig, String> {
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
        None => {
            let p = home.join(".config/ikigai/cms.toml");
            p.is_file().then_some(p)
        }
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
    // (they're outputs — created on first write, so no existence probe).
    let mut tags = crate::tagstore::TagPaths::in_state_dir(home);
    if let Some(s) = raw.tags_approved {
        tags.approved = expand(home, &s);
    }
    if let Some(s) = raw.tags_suggestions {
        tags.suggestions = expand(home, &s);
    }
    if let Some(s) = raw.tags_dismissed {
        tags.dismissed = expand(home, &s);
    }

    let page_port = raw.page_port.unwrap_or(8080);
    Ok(CmsConfig {
        wire_port: raw.wire_port.unwrap_or(4433),
        page_port,
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
        rp_origin: raw
            .rp_origin
            .unwrap_or_else(|| format!("http://localhost:{page_port}")),
        dist: raw
            .dist
            .map(|s| expand(home, &s))
            .unwrap_or_else(|| PathBuf::from("dist")),
        dev_open: raw.dev_open.unwrap_or(false),
        linkcheck: raw.linkcheck.unwrap_or(false),
        tagsuggest: raw.tagsuggest.unwrap_or(false),
        linkstatus: raw.linkstatus.map(|s| expand(home, &s)),
        llm_provider: raw.llm_provider,
        tags,
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
config: ~/.config/ikigai/cms.toml — flags override it, one run at a time
  --config <path>          use a different config file
  --wire-port <port>       WebTransport port (default 4433)
  --page-port <port>       reading-room page port (default 8080)
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

    fn load_args(home: &Path, args: &[&str]) -> Result<CmsConfig, String> {
        load_with_home(home, args.iter().map(|s| s.to_string()))
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
