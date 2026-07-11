// Passkey auth for the reading room over HTTP — the ONLY app-specific JavaScript in the page,
// and it does nothing but the standard WebAuthn ceremony: fetch a challenge, hand it to
// navigator.credentials, post the result back. The room itself stays pure htmx.
//
// It uses the native WebAuthn JSON API (PublicKeyCredential.parse*OptionsFromJSON + cred.toJSON),
// so there is no hand-rolled base64url/ArrayBuffer marshalling here. Needs a recent Chromium
// (Chrome/Edge) — which is what the room targets anyway (http://localhost is a secure context).
(function () {
  const $ = (id) => document.getElementById(id);

  // POST a JSON body (or nothing) to an /auth/* route and read the JSON reply. Same-origin, so
  // the cms_session cookie rides along automatically.
  async function api(path, body) {
    const res = await fetch(path, {
      method: "POST",
      headers: body ? { "Content-Type": "application/json" } : {},
      body: body ? JSON.stringify(body) : undefined,
    });
    return res.json();
  }

  function fail(msg) {
    $("auth-status").textContent = msg || "";
  }

  // Reveal the room (and hand control to htmx to load it) or the login panel.
  function show(open, canSignOut) {
    $("auth").hidden = open;
    $("nav").hidden = !open;
    $("auth-logout").hidden = !canSignOut;
    if (open) {
      htmx.ajax("GET", "/r/urn:cms:tags", { target: "#room", swap: "innerHTML" });
    } else {
      $("room").innerHTML = "";
    }
  }

  async function register() {
    fail("");
    try {
      const opts = await api("/auth/register/start", { name: "ikigai" });
      if (opts.error) return fail(opts.error);
      const cred = await navigator.credentials.create({
        publicKey: PublicKeyCredential.parseCreationOptionsFromJSON(opts.publicKey),
      });
      const done = await api("/auth/register/finish", cred.toJSON());
      if (done.error) return fail(done.error);
      await login(); // enrolled — now sign in with it
    } catch (e) {
      fail("enroll failed: " + e.message);
    }
  }

  async function login() {
    fail("");
    try {
      const opts = await api("/auth/login/start");
      if (opts.error) return fail(opts.error);
      const cred = await navigator.credentials.get({
        publicKey: PublicKeyCredential.parseRequestOptionsFromJSON(opts.publicKey),
      });
      const done = await api("/auth/login/finish", cred.toJSON());
      if (done.error) return fail(done.error);
      show(true, true);
      // Let the optional WebTransport shim bring the wire up for this new session.
      document.dispatchEvent(new CustomEvent("ikigai:authed"));
    } catch (e) {
      fail("sign-in failed: " + e.message);
    }
  }

  async function logout() {
    await api("/auth/logout");
    document.dispatchEvent(new CustomEvent("ikigai:deauthed"));
    show(false, false);
  }

  // On load, ask the server what state we're in and show the right thing.
  async function init() {
    let st = {};
    try {
      st = await (await fetch("/auth/status")).json();
    } catch (_) {}
    // First run (nothing enrolled) → offer enroll; otherwise → offer sign-in.
    $("auth-enroll").hidden = !!st.enrolled;
    $("auth-login").hidden = !st.enrolled;
    // dev-open (localhost) skips auth entirely; a real login shows the sign-out.
    show(!!(st.authenticated || st.dev_open), !!st.authenticated);
  }

  $("auth-login").addEventListener("click", login);
  $("auth-enroll").addEventListener("click", register);
  $("auth-logout").addEventListener("click", logout);
  init();
})();
