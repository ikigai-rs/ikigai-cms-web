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

use std::sync::{Arc, Mutex};

use ikigai_core::Capability;
use ikigai_secret::Backend;
use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::*;

/// The keystore item name the passkey store lives under (on macOS, a Keychain item).
const STORE_NAME: &str = "cms-passkeys";

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

/// The relying party: the WebAuthn verifier + the enrolled-credential store, persisted
/// through the OS keystore (macOS Keychain) rather than a plaintext file.
pub struct Rp {
    webauthn: Webauthn,
    store: Mutex<Store>,
    backend: Arc<dyn Backend>,
}

impl Rp {
    /// Build the RP. `rp_id` is the registrable domain (e.g. `localhost`); `rp_origin` is
    /// the page origin where the ceremony runs (e.g. `http://localhost:8080`); `backend`
    /// is the secret store the `{Passkey → scopes}` table persists to.
    pub fn new(rp_id: &str, rp_origin: &str, backend: Arc<dyn Backend>) -> Result<Self, String> {
        let origin = Url::parse(rp_origin).map_err(|e| format!("bad rp_origin: {e}"))?;
        let webauthn = WebauthnBuilder::new(rp_id, &origin)
            .map_err(|e| format!("webauthn builder: {e}"))?
            .build()
            .map_err(|e| format!("webauthn build: {e}"))?;
        let store = load_store(backend.as_ref())?;
        Ok(Rp {
            webauthn,
            store: Mutex::new(store),
            backend,
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

    /// Begin a login: the challenge options as JSON (for `navigator.credentials.get`) +
    /// the in-progress state to hold on the connection until `login_finish`.
    pub fn login_start(&self) -> Result<(String, PasskeyAuthentication), String> {
        let keys = self.passkeys();
        if keys.is_empty() {
            return Err("no enrolled passkeys".into());
        }
        let (challenge, state) = self
            .webauthn
            .start_passkey_authentication(&keys)
            .map_err(|e| format!("login start: {e}"))?;
        Ok((json(&challenge)?, state))
    }

    /// Finish a login: verify the browser's assertion (JSON) against the held state, then
    /// map the authenticated credential to its granted capability.
    pub fn login_finish(
        &self,
        credential_json: &[u8],
        state: &PasskeyAuthentication,
    ) -> Result<Capability, String> {
        let cred: PublicKeyCredential =
            serde_json::from_slice(credential_json).map_err(|e| format!("bad credential: {e}"))?;
        let auth = self
            .webauthn
            .finish_passkey_authentication(&cred, state)
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

    /// Begin registration — the challenge options as JSON (for `navigator.credentials.create`)
    /// + the in-progress state to hold until `register_finish`.
    pub fn register_start(&self, user_name: &str) -> Result<(String, PasskeyRegistration), String> {
        let exclude = self
            .passkeys()
            .iter()
            .map(|k| k.cred_id().clone())
            .collect();
        let (challenge, state) = self
            .webauthn
            .start_passkey_registration(Uuid::new_v4(), user_name, user_name, Some(exclude))
            .map_err(|e| format!("register start: {e}"))?;
        Ok((json(&challenge)?, state))
    }

    /// Finish registration: verify the browser's response (JSON) + persist the new
    /// credential with its granted scopes (its entitlement).
    ///
    /// Gated by biometric presence at the **server machine** (Touch ID): enrolling a
    /// passkey isn't just first-come, it needs someone at the box to approve — so a
    /// remote first-comer can't claim the room. The gate is here, not at `start`: the
    /// browser's `navigator.credentials.create()` must run on the click's transient
    /// activation, and a native Touch-ID dialog mid-`start` steals focus and voids it
    /// (the browser then refuses `create()` with `NotAllowedError`). By `finish`,
    /// `create()` has already succeeded, so the gate is free to prompt.
    pub fn register_finish(
        &self,
        credential_json: &[u8],
        state: &PasskeyRegistration,
        scopes: Vec<String>,
    ) -> Result<(), String> {
        ikigai_secret::require_biometric("Enroll a passkey for the ikigai reading room")
            .map_err(|e| format!("{e}"))?;
        let cred: RegisterPublicKeyCredential =
            serde_json::from_slice(credential_json).map_err(|e| format!("bad credential: {e}"))?;
        let passkey = self
            .webauthn
            .finish_passkey_registration(&cred, state)
            .map_err(|e| format!("register finish: {e}"))?;
        let mut store = self.store.lock().unwrap();
        store.credentials.push(Enrolled { passkey, scopes });
        save_store(self.backend.as_ref(), &store)
    }
}

/// Serialize a WebAuthn challenge to the JSON the browser's WebAuthn API consumes.
fn json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| format!("serialize challenge: {e}"))
}

/// Load the passkey store from the secret backend (empty if absent).
fn load_store(backend: &dyn Backend) -> Result<Store, String> {
    match backend
        .get(STORE_NAME)
        .map_err(|e| format!("read passkey store: {e}"))?
    {
        Some(bytes) => {
            serde_json::from_slice(&bytes).map_err(|e| format!("parse passkey store: {e}"))
        }
        None => Ok(Store::default()),
    }
}

/// Persist the passkey store to the secret backend.
fn save_store(backend: &dyn Backend, store: &Store) -> Result<(), String> {
    let bytes = serde_json::to_vec(store).map_err(|e| e.to_string())?;
    backend
        .set(STORE_NAME, &bytes)
        .map_err(|e| format!("save passkey store: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(dir: &tempfile::TempDir) -> Arc<dyn Backend> {
        Arc::new(ikigai_secret::FileBackend::new(dir.path()))
    }

    #[test]
    fn rp_builds_and_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let rp = Rp::new("localhost", "http://localhost:8080", backend(&dir)).expect("rp builds");
        assert!(!rp.is_enrolled(), "no credentials enrolled yet");
        // With nothing enrolled, a login can't start — the room stays gated.
        assert!(rp.login_start().is_err(), "no passkeys ⇒ no login");
    }

    #[test]
    fn a_bad_origin_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Rp::new("localhost", "not a url", backend(&dir)).is_err());
    }
}
