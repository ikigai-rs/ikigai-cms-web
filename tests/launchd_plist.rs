//! The launchd plist this repo versions (`deploy/macos/dev.ikigai-rs.cms.plist`), held to what
//! ledger #208 found wrong in the copy that lived only in `~/Library/LaunchAgents/`: an install
//! line that installs nothing, a claim that the binary does not exist, and a `--` inside an XML
//! comment, which makes every standard XML parser reject the file.

const PLIST: &str = include_str!("../deploy/macos/dev.ikigai-rs.cms.plist");
const README: &str = include_str!("../README.md");

/// The one install command, as the README states it and the plist points to it.
const INSTALL: &str = "cargo install --path . --features server --locked --force";

/// The bodies of every `<!-- … -->` comment.
fn comments(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find("<!--") {
        let body = &rest[open + 4..];
        let close = body.find("-->").expect("every comment closes");
        out.push(&body[..close]);
        rest = &body[close + 3..];
    }
    out
}

#[test]
fn no_comment_carries_a_double_hyphen() {
    let found = comments(PLIST);
    assert!(!found.is_empty(), "the plist documents itself in a comment");
    for body in found {
        // XML 1.0 §2.5: the string "--" must not occur within a comment. plutil accepts it;
        // plistlib, expat and libxml2 refuse the whole document.
        assert!(!body.contains("--"), "`--` inside an XML comment: {body}");
        assert!(
            !body.ends_with('-'),
            "a comment ending in `-` makes `--->`: {body}"
        );
    }
}

#[test]
fn the_plist_runs_the_installed_binary_and_points_at_the_readme_install() {
    assert!(
        PLIST.contains("<string>/Users/brian/.cargo/bin/cms-server</string>"),
        "launchd runs the ~/.cargo/bin binary, never a target/ build"
    );
    assert!(
        PLIST.contains("\"Install as a service\""),
        "the plist names the README section that holds the install command"
    );
    assert!(
        !PLIST.contains("NOT YET INSTALLABLE"),
        "the binary has existed since 2026-09-01"
    );
}

#[test]
fn the_readme_install_builds_the_server_feature_against_the_lock() {
    assert!(
        README.contains("## Install as a service"),
        "the section the plist points at"
    );
    assert!(
        README.contains(INSTALL),
        "every binary needs a feature (without one the install installs nothing), and \
         `--locked` keeps the committed graph: {INSTALL}"
    );
}
