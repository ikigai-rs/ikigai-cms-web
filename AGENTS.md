# Working in this repository

`ikigai-cms-web` is the semantic-CMS reading room: a kernel over a personal library (bookmarks,
books, presentations) as one RDF graph, served as a passkey-gated htmx page over HTTP and
WebTransport by `cms-server`. These rules are for every agent and every person working here.
`CLAUDE.md` is a link to this file.

- **Read [ACCESSIBILITY.md](ACCESSIBILITY.md) before any change to UI or rendered output**: the
  page shell and its passkey script in `dist/`, the stylesheets in `styles/`, and the markup the
  server renders into the room. It lists what has been tested, what has not, and the known
  barriers.
- Never run a test server against the configuration, data or passkey store of a room someone
  uses: give it a scratch `--config`, scratch paths and a scratch `--keystore-dir` (an existing
  empty directory), so the passkey store is not the Keychain.
- Security reports go through the organization's security policy (GitHub private vulnerability
  reporting), never a public issue.
- Every change lands through a pull request with green CI (`.github/workflows/ci.yml`: fmt,
  clippy with warnings denied, tests with the `server` and `maintenance` features, and the wasm
  build of the library).
- Never invent testing, tool results or conformance evidence, and never weaken a test to make a
  change pass.
- Use American spelling in everything you write: code comments, docs, commit messages and pull
  requests.
