//! The server-side WebAuthn relying party (rung 3b).
//!
//! Verifies a passkey assertion and maps the authenticated credential to its granted
//! capability scopes — the connection ceiling the wire `dispatch` clamps against
//! (rung 3a). The ceremony is bound to the **page origin** (where `navigator.credentials`
//! runs, e.g. `http://localhost:8080`), *not* the WebTransport channel; the channel just
//! ferries the assertion bytes to this verifier.
//!
//! `login_start` / `login_finish` are driven over the wire as `urn:auth:login:*`
//! resources intercepted by the session layer in the server bin. The in-progress
//! `Passkey{Authentication,Registration}` state is held per-connection (never persisted),
//! so no `danger-allow-state-serialisation`; only the enrolled `Passkey`s persist.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ikigai_core::Capability;
use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::*;

/// One enrolled credential and the capability scopes it grants.
#[derive(Serialize, Deserialize)]
struct Enrolled {
    passkey: Passkey,
    scopes: Vec<String>,
}

/// The persisted `{Passkey → scopes}` store — the `{principal → entitlement}` table.
#[derive(Default, Serialize, Deserialize)]
struct Store {
    credentials: Vec<Enrolled>,
}

/// The relying party: the WebAuthn verifier + the enrolled-credential store.
pub struct Rp {
    webauthn: Webauthn,
    store: Mutex<Store>,
    store_path: PathBuf,
}

impl Rp {
    /// Build the RP. `rp_id` is the registrable domain (e.g. `localhost`); `rp_origin` is
    /// the page origin where the ceremony runs (e.g. `http://localhost:8080`).
    pub fn new(rp_id: &str, rp_origin: &str, store_path: PathBuf) -> Result<Self, String> {
        let origin = Url::parse(rp_origin).map_err(|e| format!("bad rp_origin: {e}"))?;
        let webauthn = WebauthnBuilder::new(rp_id, &origin)
            .map_err(|e| format!("webauthn builder: {e}"))?
            .build()
            .map_err(|e| format!("webauthn build: {e}"))?;
        Ok(Rp {
            webauthn,
            store: Mutex::new(load_store(&store_path)),
            store_path,
        })
    }

    /// All enrolled passkeys (for `start_passkey_authentication`).
    fn passkeys(&self) -> Vec<Passkey> {
        self.store
            .lock()
            .unwrap()
            .credentials
            .iter()
            .map(|e| e.passkey.clone())
            .collect()
    }

    /// Whether any passkey is enrolled (no enrollment ⇒ nobody can log in yet).
    pub fn is_enrolled(&self) -> bool {
        !self.store.lock().unwrap().credentials.is_empty()
    }

    /// Begin a login: the challenge options to send to the browser + the in-progress
    /// state to hold on the connection until `login_finish`.
    pub fn login_start(&self) -> Result<(RequestChallengeResponse, PasskeyAuthentication), String> {
        let keys = self.passkeys();
        if keys.is_empty() {
            return Err("no enrolled passkeys".into());
        }
        self.webauthn
            .start_passkey_authentication(&keys)
            .map_err(|e| format!("login start: {e}"))
    }

    /// Finish a login: verify the browser's assertion against the held state, then map the
    /// authenticated credential to its granted capability.
    pub fn login_finish(
        &self,
        cred: &PublicKeyCredential,
        state: &PasskeyAuthentication,
    ) -> Result<Capability, String> {
        let auth = self
            .webauthn
            .finish_passkey_authentication(cred, state)
            .map_err(|e| format!("login finish: {e}"))?;
        let store = self.store.lock().unwrap();
        let scopes = store
            .credentials
            .iter()
            .find(|e| e.passkey.cred_id() == auth.cred_id())
            .map(|e| e.scopes.clone())
            .ok_or_else(|| "authenticated credential is not enrolled".to_string())?;
        Ok(Capability::scoped(scopes))
    }

    /// Begin registration — enroll a new passkey.
    pub fn register_start(
        &self,
        user_name: &str,
    ) -> Result<(CreationChallengeResponse, PasskeyRegistration), String> {
        let exclude = self
            .passkeys()
            .iter()
            .map(|k| k.cred_id().clone())
            .collect();
        self.webauthn
            .start_passkey_registration(Uuid::new_v4(), user_name, user_name, Some(exclude))
            .map_err(|e| format!("register start: {e}"))
    }

    /// Finish registration: verify + persist the new credential with its granted scopes.
    pub fn register_finish(
        &self,
        cred: &RegisterPublicKeyCredential,
        state: &PasskeyRegistration,
        scopes: Vec<String>,
    ) -> Result<(), String> {
        let passkey = self
            .webauthn
            .finish_passkey_registration(cred, state)
            .map_err(|e| format!("register finish: {e}"))?;
        let mut store = self.store.lock().unwrap();
        store.credentials.push(Enrolled { passkey, scopes });
        save_store(&self.store_path, &store)
    }
}

fn load_store(path: &Path) -> Store {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_store(path: &Path, store: &Store) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(path, bytes).map_err(|e| format!("save store: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rp_builds_and_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let rp = Rp::new(
            "localhost",
            "http://localhost:8080",
            dir.path().join("passkeys.json"),
        )
        .expect("rp builds");
        assert!(!rp.is_enrolled(), "no credentials enrolled yet");
        // With nothing enrolled, a login can't start — the room stays gated.
        assert!(rp.login_start().is_err(), "no passkeys ⇒ no login");
    }

    #[test]
    fn a_bad_origin_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Rp::new("localhost", "not a url", dir.path().join("s.json")).is_err());
    }
}
