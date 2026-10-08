//! Secrets in the OS keyring: Secret Service on Linux, the Keychain on macOS,
//! the Credential Manager on Windows.
//!
//! Two kinds of entry, under the service [`crate::paths::APP_ID`]:
//!
//! - `app`: the Slack app's client id, client secret and app-level token;
//! - `workspace:<team id>`: that workspace's user token, and its refresh
//!   token when the app rotates tokens;
//! - `teams:<team id>`: Microsoft Teams credentials (access, refresh, skype tokens);
//! - `cache-key`: the random key the offline cache is encrypted with (see
//!   [`crate::offline`]).
//!
//! Keyring calls can block (an unlock prompt, a slow D-Bus), so they all run
//! in order on one thread of their own and answer through oneshot channels.
//! The interface and the network never wait on them.

use std::collections::HashMap;
use std::sync::mpsc;

use tokio::sync::oneshot;

use crate::paths::APP_ID;
use crate::slack::Token;

/// The Slack app the user registered (see `slack-app-manifest.json`).
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppCredentials {
    pub client_id: String,
    /// Optional and never sent: the manifest turns PKCE on, and Slack wants
    /// no secret from a PKCE app. Kept so a secret saved by an older build,
    /// or typed in by habit, still round-trips through the keyring.
    #[serde(default)]
    pub client_secret: String,
    /// The app-level token (`xapp-`) with `connections:write`, for Socket Mode.
    pub app_token: String,
}

/// Shows only the client id; the secret and app token never print.
impl std::fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &crate::redact::REDACTED)
            .field("app_token", &crate::redact::REDACTED)
            .finish()
    }
}

impl AppCredentials {
    /// Whether these are enough for the OAuth sign-in: with PKCE, the
    /// client id alone.
    pub fn can_sign_in(&self) -> bool {
        !self.client_id.trim().is_empty()
    }

    /// What refreshing a rotating token needs, once the app can sign in.
    pub fn oauth(&self) -> Option<crate::slack::OauthApp> {
        self.can_sign_in().then(|| crate::slack::OauthApp {
            client_id: self.client_id.trim().to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("the keyring is locked")]
    Locked,
    #[error("no keyring is available")]
    Unavailable,
    #[error("a stored secret is damaged")]
    Damaged,
}

impl From<Error> for crate::failure::Keyring {
    fn from(error: Error) -> Self {
        match error {
            Error::Locked => Self::Locked,
            Error::Unavailable => Self::Unavailable,
            Error::Damaged => Self::Damaged,
        }
    }
}

trait Store: Send {
    fn read(&mut self, key: &str) -> Result<Option<Vec<u8>>, Error>;
    fn write(&mut self, key: &str, secret: &[u8]) -> Result<(), Error>;
    fn delete(&mut self, key: &str) -> Result<(), Error>;
}

/// Keeps secrets in memory only: demos and tests.
#[derive(Default)]
struct MemoryStore(HashMap<String, Vec<u8>>);

impl Store for MemoryStore {
    fn read(&mut self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.0.get(key).cloned())
    }
    fn write(&mut self, key: &str, secret: &[u8]) -> Result<(), Error> {
        self.0.insert(key.to_owned(), secret.to_vec());
        Ok(())
    }
    fn delete(&mut self, key: &str) -> Result<(), Error> {
        self.0.remove(key);
        Ok(())
    }
}

#[derive(Default)]
struct NativeStore {
    store: Option<std::sync::Arc<keyring_core::api::CredentialStore>>,
}

fn native_error(error: keyring_core::Error) -> Error {
    // Provider errors can carry secrets or platform data; never pass their
    // text along.
    match error {
        keyring_core::Error::NoStorageAccess(_) => Error::Locked,
        _ => Error::Unavailable,
    }
}

impl NativeStore {
    fn entry(&mut self, key: &str) -> Result<keyring_core::Entry, Error> {
        if self.store.is_none() {
            #[cfg(target_os = "linux")]
            let store = zbus_secret_service_keyring_store::Store::new();
            #[cfg(target_os = "macos")]
            let store = apple_native_keyring_store::keychain::Store::new();
            #[cfg(windows)]
            let store = windows_native_keyring_store::Store::new();
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            let store: Result<std::sync::Arc<keyring_core::api::CredentialStore>, _> =
                Err(keyring_core::Error::NoDefaultStore);
            self.store = Some(store.map_err(native_error)?);
        }
        self.store
            .as_ref()
            .ok_or(Error::Unavailable)?
            .build(APP_ID, key, None)
            .map_err(native_error)
    }
}

impl Store for NativeStore {
    fn read(&mut self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        match self.entry(key)?.get_secret() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(native_error(error)),
        }
    }
    fn write(&mut self, key: &str, secret: &[u8]) -> Result<(), Error> {
        self.entry(key)?.set_secret(secret).map_err(native_error)
    }
    fn delete(&mut self, key: &str) -> Result<(), Error> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(native_error(error)),
        }
    }
}

type Job = Box<dyn FnOnce(&mut dyn Store) + Send>;

/// The keyring, through its thread. Cheap to clone.
#[derive(Clone)]
pub struct Credentials {
    jobs: mpsc::Sender<Job>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials")
    }
}

impl Credentials {
    /// The OS keyring. Secret Service runs its D-Bus calls on `runtime`.
    pub fn native(runtime: Option<tokio::runtime::Handle>) -> Self {
        Self::spawn(runtime, || Box::new(NativeStore::default()))
    }

    /// Secrets that last only as long as the process.
    pub fn memory() -> Self {
        Self::spawn(None, || Box::new(MemoryStore::default()))
    }

    fn spawn(
        runtime: Option<tokio::runtime::Handle>,
        store: impl FnOnce() -> Box<dyn Store> + Send + 'static,
    ) -> Self {
        let (jobs, receiver) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("noslacking-keyring".into())
            .spawn(move || {
                let _entered = runtime.as_ref().map(tokio::runtime::Handle::enter);
                let mut store = store();
                while let Ok(job) = receiver.recv() {
                    job(store.as_mut());
                }
            });
        if let Err(error) = spawned {
            log::error!("could not start the keyring thread: {error}");
        }
        Self { jobs }
    }

    async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut dyn Store) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let (reply, answer) = oneshot::channel();
        let job: Job = Box::new(move |store| {
            let _ = reply.send(job(store));
        });
        self.jobs.send(job).map_err(|_| Error::Unavailable)?;
        answer.await.map_err(|_| Error::Unavailable)?
    }

    async fn read_json<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        key: String,
    ) -> Result<Option<T>, Error> {
        self.run(move |store| match store.read(&key)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| Error::Damaged),
            None => Ok(None),
        })
        .await
    }

    async fn write_json<T: serde::Serialize>(&self, key: String, value: &T) -> Result<(), Error> {
        let bytes = serde_json::to_vec(value).map_err(|_| Error::Damaged)?;
        self.run(move |store| store.write(&key, &bytes)).await
    }

    pub async fn load_app(&self) -> Result<Option<AppCredentials>, Error> {
        self.read_json("app".into()).await
    }

    pub async fn save_app(&self, app: &AppCredentials) -> Result<(), Error> {
        self.write_json("app".into(), app).await
    }

    pub async fn load_token(&self, team: &str) -> Result<Option<Token>, Error> {
        self.read_json(format!("workspace:{team}")).await
    }

    pub async fn save_token(&self, team: &str, token: &Token) -> Result<(), Error> {
        self.write_json(format!("workspace:{team}"), token).await
    }

    /// The offline cache's key, made and stored on first use. A stored
    /// key of the wrong length is replaced, which only costs the cache.
    pub async fn cache_key(&self) -> Result<crate::offline::CacheKey, Error> {
        self.run(|store| {
            if let Some(bytes) = store.read("cache-key")?
                && let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice())
            {
                return Ok(crate::offline::CacheKey(key));
            }
            let key = crate::offline::CacheKey::random();
            store.write("cache-key", &key.0)?;
            Ok(key)
        })
        .await
    }

    pub async fn delete_token(&self, team: &str) -> Result<(), Error> {
        let key = format!("workspace:{team}");
        self.run(move |store| store.delete(&key)).await
    }

    #[cfg(feature = "teams")]
    /// Loads Microsoft Teams credentials for a workspace or tenant.
    pub async fn load_teams_token(
        &self,
        team: &str,
    ) -> Result<Option<crate::teams::auth::TeamsCredentials>, Error> {
        self.read_json(format!("teams:{team}")).await
    }

    #[cfg(feature = "teams")]
    /// Saves Microsoft Teams credentials for a workspace or tenant.
    pub async fn save_teams_token(
        &self,
        team: &str,
        creds: &crate::teams::auth::TeamsCredentials,
    ) -> Result<(), Error> {
        self.write_json(format!("teams:{team}"), creds).await
    }

    #[cfg(feature = "teams")]
    /// Deletes Microsoft Teams credentials for a workspace or tenant.
    pub async fn delete_teams_token(&self, team: &str) -> Result<(), Error> {
        let key = format!("teams:{team}");
        self.run(move |store| store.delete(&key)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_cache_key_is_made_once() {
        let credentials = Credentials::memory();
        let first = credentials.cache_key().await.expect("made");
        assert_eq!(credentials.cache_key().await, Ok(first));
    }

    #[tokio::test]
    async fn secrets_round_trip_through_the_thread() {
        let credentials = Credentials::memory();
        assert_eq!(credentials.load_app().await, Ok(None));
        let app = AppCredentials {
            client_id: "1.2".into(),
            client_secret: "s".into(),
            app_token: "xapp-1".into(),
        };
        credentials.save_app(&app).await.expect("saves");
        assert_eq!(credentials.load_app().await, Ok(Some(app)));
        let token = Token::plain("xoxp-1");
        credentials.save_token("T1", &token).await.expect("saves");
        assert_eq!(credentials.load_token("T1").await, Ok(Some(token)));
        credentials.delete_token("T1").await.expect("deletes");
        assert_eq!(credentials.load_token("T1").await, Ok(None));

        #[cfg(feature = "teams")]
        teams_round_trip(&credentials).await;
    }

    #[cfg(feature = "teams")]
    async fn teams_round_trip(credentials: &Credentials) {
        let teams_creds = crate::teams::auth::TeamsCredentials {
            access_token: "teams_access".into(),
            refresh_token: Some("teams_refresh".into()),
            skype_token: Some("skype_tok".into()),
            expires_at: Some(1800000000),
            tenant_id: Some("tenant-1".into()),
            ..Default::default()
        };
        credentials
            .save_teams_token("tenant-1", &teams_creds)
            .await
            .expect("saves teams creds");
        assert_eq!(
            credentials.load_teams_token("tenant-1").await,
            Ok(Some(teams_creds))
        );
        credentials
            .delete_teams_token("tenant-1")
            .await
            .expect("deletes teams creds");
        assert_eq!(credentials.load_teams_token("tenant-1").await, Ok(None));
    }
}
