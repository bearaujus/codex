use chrono::DateTime;
use chrono::Utc;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
#[cfg(test)]
use serial_test::serial;
use sha2::Digest;
use sha2::Sha256;
use std::env;
use std::fmt::Debug;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tracing::instrument;

use codex_agent_identity::ChatGptEnvironment;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::ModelProviderAuthInfo;
use codex_protocol::protocol::RateLimitSnapshot;

use super::access_token::CodexAccessToken;
use super::access_token::classify_codex_access_token;
use super::agent_identity::ManagedChatGptAgentIdentityBinding;
use super::agent_identity::agent_identity_authapi_base_url;
use super::agent_identity::classify_bootstrap_error;
use super::agent_identity::record_matches_managed_chatgpt_binding;
use super::agent_identity::record_needs_task_registration;
use super::agent_identity::register_managed_chatgpt_agent_identity;
use super::agent_identity::require_agent_identity_authapi_base_url;
use super::agent_identity::verified_record_from_jwt;
use super::external_bearer::BearerTokenRefresher;
use super::revoke::revoke_auth_tokens;
use crate::account_pool::ChatgptAccountPool;
use crate::account_pool::ChatgptAccountPoolAuthStatus;
use crate::account_pool::ChatgptAccountPoolError;
use crate::account_pool::ChatgptAccountPoolSelectionOutcome;
use crate::account_pool::snapshot_contributes_account_cooldown;
use crate::account_pool::snapshot_indicates_account_cooldown;
use crate::auth::AuthHeaders;
pub use crate::auth::agent_identity::AgentIdentityAuth;
pub use crate::auth::agent_identity::AgentIdentityAuthError;
pub use crate::auth::storage::AgentIdentityAuthRecord;
pub use crate::auth::storage::AgentIdentityStorage;
pub use crate::auth::storage::AuthDotJson;
pub use crate::auth::storage::AuthKeyringBackendKind;
use crate::auth::storage::AuthStorageBackend;
use crate::auth::storage::create_auth_storage;
use crate::auth::util::try_parse_error_message;
use crate::default_client::create_client;
use crate::default_client::create_default_auth_client;
use crate::outbound_proxy::AuthRouteConfig;
use crate::token_data::TokenData;
use crate::token_data::derive_pool_account_id;
use crate::token_data::parse_chatgpt_jwt_claims;
use codex_config::types::AuthCredentialsStoreMode;
use codex_http_client::HttpClient;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::RefreshTokenFailedError;
use codex_protocol::auth::RefreshTokenFailedReason;
use codex_protocol::protocol::SessionSource;
use serde_json::Value;
use thiserror::Error;

/// Authentication mechanism used by the current user.
#[derive(Debug, Clone)]
pub enum CodexAuth {
    Chatgpt(ChatgptAuth),
    Headers(AuthHeaders),
    AgentIdentity(AgentIdentityAuth),
}

/// Policy for resolving Agent Identity auth from a broader Codex auth snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentIdentityAuthPolicy {
    /// Use Agent Identity auth only when the current auth is already Agent Identity.
    JwtOnly,
    /// Allow managed ChatGPT auth to register or reuse Agent Identity auth.
    ChatGptAuth,
}

const AGENT_IDENTITY_BOOTSTRAP_FAILURE_COOLDOWN: Duration = Duration::from_secs(60 * 60);

#[derive(Debug)]
struct CachedAgentIdentityBootstrapFailure {
    account_id: String,
    authapi_base_url: String,
    retry_at: Instant,
    error: AgentIdentityAuthError,
}

#[derive(Debug, Default)]
struct AgentIdentityBootstrapCooldown {
    failure: Option<CachedAgentIdentityBootstrapFailure>,
}

impl AgentIdentityBootstrapCooldown {
    fn error_for(
        &mut self,
        account_id: &str,
        authapi_base_url: &str,
        now: Instant,
    ) -> Option<AgentIdentityAuthError> {
        let error = self
            .failure
            .as_ref()
            .filter(|failure| {
                failure.account_id == account_id
                    && failure.authapi_base_url == authapi_base_url
                    && failure.retry_at > now
            })
            .map(|failure| failure.error.clone());
        if error.is_none() {
            self.clear();
        }
        error
    }

    fn record_failure(
        &mut self,
        account_id: String,
        authapi_base_url: String,
        error: AgentIdentityAuthError,
        now: Instant,
    ) {
        self.failure = Some(CachedAgentIdentityBootstrapFailure {
            account_id,
            authapi_base_url,
            retry_at: now + AGENT_IDENTITY_BOOTSTRAP_FAILURE_COOLDOWN,
            error,
        });
    }

    fn clear(&mut self) {
        self.failure = None;
    }
}

impl PartialEq for CodexAuth {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Headers(a), Self::Headers(b)) => a == b,
            _ => self.api_auth_mode() == other.api_auth_mode(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatgptAuth {
    state: ChatgptAuthState,
    storage: Arc<dyn AuthStorageBackend>,
}

#[derive(Debug, Clone)]
struct ChatgptAuthState {
    auth_dot_json: Arc<Mutex<Option<AuthDotJson>>>,
    client: HttpClient,
}

const REFRESH_TOKEN_EXPIRED_MESSAGE: &str = "Your access token could not be refreshed because your refresh token has expired. Please log out and sign in again.";
const REFRESH_TOKEN_REUSED_MESSAGE: &str = "Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.";
const REFRESH_TOKEN_INVALIDATED_MESSAGE: &str = "Your access token could not be refreshed because your refresh token was revoked. Please log out and sign in again.";
const REFRESH_TOKEN_UNKNOWN_MESSAGE: &str =
    "Your access token could not be refreshed. Please log out and sign in again.";
const REFRESH_TOKEN_ACCOUNT_MISMATCH_MESSAGE: &str = "Your access token could not be refreshed because you have since logged out or signed in to another account. Please sign in again.";
const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub(super) const REVOKE_TOKEN_URL: &str = "https://auth.openai.com/oauth/revoke";
pub const REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
pub const REVOKE_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REVOKE_TOKEN_URL_OVERRIDE";
pub const CLIENT_ID_OVERRIDE_ENV_VAR: &str = "CODEX_APP_SERVER_LOGIN_CLIENT_ID";
static NEXT_DUMMY_AUTH_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub enum RefreshTokenError {
    #[error("{0}")]
    Permanent(#[from] RefreshTokenFailedError),
    #[error(transparent)]
    Transient(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalAuthRefreshReason {
    Unauthorized,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalAuthRefreshContext {
    pub reason: ExternalAuthRefreshReason,
    pub previous_account_id: Option<String>,
}

/// Pluggable auth provider used by `AuthManager` for externally managed auth flows.
///
/// Implementations own the current auth value and any source-specific refresh mechanism.
pub trait ExternalAuth: Send + Sync {
    /// Returns the provider's current auth value.
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth>;

    /// Refreshes auth and makes the returned value current for future `resolve()` calls.
    fn refresh(&self, context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth>;
}

pub type ExternalAuthFuture<'a, T> = Pin<Box<dyn Future<Output = std::io::Result<T>> + Send + 'a>>;

impl RefreshTokenError {
    pub fn failed_reason(&self) -> Option<RefreshTokenFailedReason> {
        match self {
            Self::Permanent(error) => Some(error.reason),
            Self::Transient(_) => None,
        }
    }
}

impl From<RefreshTokenError> for std::io::Error {
    fn from(err: RefreshTokenError) -> Self {
        match err {
            RefreshTokenError::Permanent(failed) => std::io::Error::other(failed),
            RefreshTokenError::Transient(inner) => inner,
        }
    }
}

impl CodexAuth {
    pub(crate) async fn from_auth_dot_json(
        codex_home: &Path,
        auth_dot_json: AuthDotJson,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        chatgpt_base_url: Option<&str>,
        keyring_backend_kind: AuthKeyringBackendKind,
        agent_identity_authapi_base_url: Option<&str>,
        auth_route_config: Option<&AuthRouteConfig>,
    ) -> std::io::Result<Self> {
        let auth_mode = auth_dot_json.resolved_mode()?;
        if auth_mode == AuthMode::AgentIdentity {
            let Some(agent_identity) = auth_dot_json.agent_identity.clone() else {
                return Err(std::io::Error::other(
                    "agent identity auth is missing agent identity auth material.",
                ));
            };
            let base_url = chatgpt_base_url
                .unwrap_or(ChatGptEnvironment::default().chatgpt_base_url())
                .trim_end_matches('/')
                .to_string();
            let agent_identity_authapi_base_url =
                require_agent_identity_authapi_base_url(agent_identity_authapi_base_url)?;
            match agent_identity {
                AgentIdentityStorage::Jwt(jwt) => {
                    let auth = AgentIdentityAuth::from_jwt(
                        &jwt,
                        &base_url,
                        agent_identity_authapi_base_url,
                        auth_route_config,
                    )
                    .await?;
                    return Ok(Self::AgentIdentity(auth));
                }
                AgentIdentityStorage::Record(record) => {
                    let auth = AgentIdentityAuth::from_record(
                        record,
                        agent_identity_authapi_base_url,
                        auth_route_config,
                    )
                    .await?;
                    return Ok(Self::AgentIdentity(auth));
                }
            }
        }
        if auth_mode == AuthMode::Headers {
            return Err(std::io::Error::other(
                "externally provided auth cannot be loaded from auth storage.",
            ));
        }

        let storage_mode = auth_dot_json.storage_mode(auth_credentials_store_mode);
        // Pool-managed ChatGPT tokens are stored in the account-pool DB (the single
        // source of truth). The `auth_dot_json` passed here is already the freshest
        // copy read from the DB, so we do NOT re-read a per-account auth.json file —
        // those files no longer exist and reading one could resurrect a stale token.
        // The storage backend root is the top-level codex_home (matching non-pool
        // ChatGPT auth); pool accounts are never refreshed by the CLI, so this
        // backend is effectively write-dead for them.
        let auth_storage_root = codex_home.to_path_buf();
        let client = create_default_auth_client(&refresh_token_endpoint(), auth_route_config)?;
        let state = ChatgptAuthState {
            auth_dot_json: Arc::new(Mutex::new(Some(auth_dot_json))),
            client,
        };

        match auth_mode {
            AuthMode::Chatgpt => {
                let storage =
                    create_auth_storage(auth_storage_root, storage_mode, keyring_backend_kind);
                Ok(Self::Chatgpt(ChatgptAuth { state, storage }))
            }
            AuthMode::Headers => {
                unreachable!("externally provided auth is never loaded from auth storage")
            }
            AuthMode::AgentIdentity => unreachable!("agent identity mode is handled above"),
        }
    }

    pub async fn from_auth_storage(
        codex_home: &Path,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        chatgpt_base_url: Option<&str>,
        keyring_backend_kind: AuthKeyringBackendKind,
        auth_route_config: Option<&AuthRouteConfig>,
    ) -> std::io::Result<Option<Self>> {
        let agent_identity_authapi_base_url =
            agent_identity_authapi_base_url(chatgpt_base_url).ok();
        load_auth(
            codex_home,
            auth_credentials_store_mode,
            /*forced_chatgpt_workspace_id*/ None,
            chatgpt_base_url,
            keyring_backend_kind,
            agent_identity_authapi_base_url.as_deref(),
            auth_route_config,
        )
        .await
    }

    pub async fn from_agent_identity_jwt(
        jwt: &str,
        chatgpt_base_url: Option<&str>,
        auth_route_config: Option<&AuthRouteConfig>,
    ) -> std::io::Result<Self> {
        let agent_identity_authapi_base_url = agent_identity_authapi_base_url(chatgpt_base_url)?;
        Self::from_agent_identity_jwt_with_authapi_base_url(
            jwt,
            chatgpt_base_url,
            &agent_identity_authapi_base_url,
            auth_route_config,
        )
        .await
    }

    async fn from_agent_identity_jwt_with_authapi_base_url(
        jwt: &str,
        chatgpt_base_url: Option<&str>,
        agent_identity_authapi_base_url: &str,
        auth_route_config: Option<&AuthRouteConfig>,
    ) -> std::io::Result<Self> {
        let base_url = chatgpt_base_url
            .unwrap_or(ChatGptEnvironment::default().chatgpt_base_url())
            .trim_end_matches('/')
            .to_string();
        Ok(Self::AgentIdentity(
            AgentIdentityAuth::from_jwt(
                jwt,
                &base_url,
                agent_identity_authapi_base_url,
                auth_route_config,
            )
            .await?,
        ))
    }

    /// Returns the effective backend auth mode.
    ///
    /// Externally managed ChatGPT tokens are normalized to [`AuthMode::Chatgpt`].
    pub fn auth_mode(&self) -> AuthMode {
        match self {
            Self::Chatgpt(_) => AuthMode::Chatgpt,
            Self::Headers(_) => AuthMode::Headers,
            Self::AgentIdentity(_) => AuthMode::AgentIdentity,
        }
    }

    /// Returns the precise kind of credentials backing this authentication.
    pub fn api_auth_mode(&self) -> AuthMode {
        self.auth_mode()
    }

    pub fn is_chatgpt_auth(&self) -> bool {
        self.api_auth_mode().has_chatgpt_account()
    }

    pub fn uses_codex_backend(&self) -> bool {
        self.api_auth_mode().uses_codex_backend()
    }

    fn supports_unauthorized_recovery(&self) -> bool {
        matches!(self, Self::Chatgpt(_) | Self::Headers(_))
    }

    /// Legacy helper removed with API-key auth; always returns `None`.
    pub fn api_key(&self) -> Option<&str> {
        None
    }

    /// Returns auth request headers when this snapshot is [`Self::Headers`].
    pub fn headers(&self) -> Option<&reqwest::header::HeaderMap> {
        match self {
            Self::Headers(auth) => Some(auth.headers()),
            Self::Chatgpt(_) | Self::AgentIdentity(_) => None,
        }
    }

    /// Returns `Err` if token-backed ChatGPT auth is unavailable.
    pub fn get_token_data(&self) -> Result<TokenData, std::io::Error> {
        let auth_dot_json: Option<AuthDotJson> = self.get_current_auth_json();
        match auth_dot_json {
            Some(AuthDotJson {
                tokens: Some(tokens),
                last_refresh: Some(_),
                ..
            }) => Ok(tokens),
            _ => Err(std::io::Error::other("Token data is not available.")),
        }
    }

    /// Returns the token string used for bearer authentication.
    pub fn get_token(&self) -> Result<String, std::io::Error> {
        match self {
            Self::Chatgpt(_) => {
                let access_token = self.get_token_data()?.access_token;
                Ok(access_token)
            }
            Self::AgentIdentity(_) => Err(std::io::Error::other(
                "agent identity auth does not expose a bearer token",
            )),
            Self::Headers(_) => Err(std::io::Error::other(
                "header auth does not expose a bearer token",
            )),
        }
    }

    /// Returns `None` if Codex backend auth does not expose an account id.
    pub fn get_account_id(&self) -> Option<String> {
        match self {
            Self::Headers(_) => None,
            Self::AgentIdentity(auth) => Some(auth.account_id().to_string()),
            Self::Chatgpt(_) => self.get_current_token_data().and_then(|t| t.account_id),
        }
    }

    /// Returns the pool row identifier used for ChatGPT account-pool storage.
    ///
    /// Only the explicit `pool_account_id` on the auth payload counts. ChatGPT
    /// credentials are always activated through the account pool; derived
    /// workspace ids are used only when registering into the pool.
    pub fn get_pool_account_id(&self) -> Option<String> {
        match self {
            Self::AgentIdentity(_) => None,
            _ => self
                .get_current_auth_json()
                .and_then(|auth| auth.pool_account_id),
        }
    }

    /// Whether this is a ChatGPT credential whose access token is stale
    /// (expired or within the refresh skew window) and should be refreshed
    /// before use. Returns `false` for non-ChatGPT auth or a still-fresh token.
    pub(crate) fn chatgpt_access_token_is_stale(&self) -> bool {
        let Self::Chatgpt(chatgpt_auth) = self else {
            return false;
        };
        match chatgpt_auth.current_auth_json() {
            Some(auth_dot_json) => {
                ChatgptAccountPool::account_auth_needs_token_refresh(&auth_dot_json, Utc::now())
            }
            None => false,
        }
    }

    /// Returns false if Codex backend auth omits the FedRAMP claim.
    pub fn is_fedramp_account(&self) -> bool {
        match self {
            Self::Headers(_) => false,
            Self::AgentIdentity(auth) => auth.is_fedramp_account(),
            Self::Chatgpt(_) => self
                .get_current_token_data()
                .is_some_and(|t| t.id_token.is_fedramp_account()),
        }
    }

    /// Returns `None` if Codex backend auth does not expose an account email.
    pub fn get_account_email(&self) -> Option<String> {
        match self {
            Self::Headers(_) => None,
            Self::AgentIdentity(auth) => auth.email().map(str::to_string),
            Self::Chatgpt(_) => self.get_current_token_data().and_then(|t| t.id_token.email),
        }
    }

    /// Returns `None` if Codex backend auth does not expose a ChatGPT user id.
    pub fn get_chatgpt_user_id(&self) -> Option<String> {
        match self {
            Self::Headers(_) => None,
            Self::AgentIdentity(auth) => Some(auth.chatgpt_user_id().to_string()),
            Self::Chatgpt(_) => self
                .get_current_token_data()
                .and_then(|t| t.id_token.chatgpt_user_id),
        }
    }

    /// Account-facing plan classification derived from the current auth.
    /// Returns a high-level `AccountPlanType` (e.g., Free/Plus/Pro/Team/…)
    /// for UI or product decisions based on the user's subscription.
    pub fn account_plan_type(&self) -> Option<AccountPlanType> {
        match self {
            Self::Headers(_) => None,
            Self::AgentIdentity(auth) => Some(auth.plan_type()),
            Self::Chatgpt(_) => self.get_current_token_data().map(|t| {
                t.id_token
                    .chatgpt_plan_type
                    .map(AccountPlanType::from)
                    .unwrap_or(AccountPlanType::Unknown)
            }),
        }
    }

    pub fn is_workspace_account(&self) -> bool {
        self.account_plan_type()
            .is_some_and(AccountPlanType::is_workspace_account)
    }

    /// Returns `None` if token-backed ChatGPT auth is unavailable.
    fn get_current_auth_json(&self) -> Option<AuthDotJson> {
        let state = match self {
            Self::Chatgpt(auth) => &auth.state,
            Self::Headers(_) | Self::AgentIdentity(_) => return None,
        };
        #[expect(clippy::unwrap_used)]
        state.auth_dot_json.lock().unwrap().clone()
    }

    /// Returns `None` if token-backed ChatGPT auth is unavailable.
    fn get_current_token_data(&self) -> Option<TokenData> {
        self.get_current_auth_json().and_then(|t| t.tokens)
    }

    fn stored_managed_chatgpt_agent_identity_record(
        &self,
        account_id: &str,
    ) -> Option<AgentIdentityAuthRecord> {
        self.get_current_auth_json()
            .and_then(|auth| auth.agent_identity)
            .and_then(|identity| identity.as_record().cloned())
            .filter(|identity| identity.account_id == account_id)
    }

    fn persist_managed_chatgpt_agent_identity_record(
        &self,
        record: AgentIdentityAuthRecord,
    ) -> std::io::Result<()> {
        if let Self::Chatgpt(chatgpt_auth) = self {
            chatgpt_auth.persist_agent_identity_record(record)?;
        }
        Ok(())
    }

    async fn agent_identity_auth(
        &self,
        policy: AgentIdentityAuthPolicy,
        agent_identity_authapi_base_url: Option<&str>,
        forced_chatgpt_workspace_id: Option<Vec<String>>,
        auth_route_config: Option<&AuthRouteConfig>,
        session_source: SessionSource,
    ) -> std::io::Result<Option<AgentIdentityAuth>> {
        match self {
            Self::AgentIdentity(auth) => Ok(Some(auth.clone())),
            Self::Headers(_) => Ok(None),
            Self::Chatgpt(_) => {
                if policy == AgentIdentityAuthPolicy::JwtOnly {
                    return Ok(None);
                }
                self.ensure_managed_chatgpt_agent_identity(
                    require_agent_identity_authapi_base_url(agent_identity_authapi_base_url)?,
                    forced_chatgpt_workspace_id,
                    auth_route_config,
                    session_source,
                )
                .await
                .map(Some)
            }
        }
    }

    async fn ensure_managed_chatgpt_agent_identity(
        &self,
        agent_identity_authapi_base_url: &str,
        forced_chatgpt_workspace_id: Option<Vec<String>>,
        auth_route_config: Option<&AuthRouteConfig>,
        session_source: SessionSource,
    ) -> std::io::Result<AgentIdentityAuth> {
        let binding =
            ManagedChatGptAgentIdentityBinding::from_auth(self, forced_chatgpt_workspace_id)
                .ok_or_else(|| std::io::Error::other("ChatGPT auth is unavailable"))?;

        // JWT auth is loaded as CodexAuth::AgentIdentity; this path only reuses
        // records created by the managed ChatGPT Agent Identity bootstrap.
        if let Some(record) = self.stored_managed_chatgpt_agent_identity_record(&binding.account_id)
            && record_matches_managed_chatgpt_binding(&record, &binding)
        {
            let should_persist = record_needs_task_registration(&record);
            let auth = AgentIdentityAuth::from_record(
                record,
                agent_identity_authapi_base_url,
                auth_route_config,
            )
            .await
            .map_err(|err| classify_bootstrap_error("agent task registration", err))?;
            if should_persist {
                self.persist_managed_chatgpt_agent_identity_record(auth.record().clone())?;
            }
            return Ok(auth);
        }

        let auth = register_managed_chatgpt_agent_identity(
            binding,
            agent_identity_authapi_base_url,
            session_source,
            auth_route_config,
        )
        .await?;
        self.persist_managed_chatgpt_agent_identity_record(auth.record().clone())?;
        Ok(auth)
    }

    /// Consider this private to integration tests.
    pub fn create_dummy_chatgpt_auth_for_testing() -> Self {
        let auth_dot_json = AuthDotJson {
            auth_mode: Some(AuthMode::Chatgpt),
            tokens: Some(TokenData {
                id_token: Default::default(),
                access_token: "Access Token".to_string(),
                refresh_token: "test".to_string(),
                account_id: Some("account_id".to_string()),
            }),
            pool_account_id: Some(derive_pool_account_id("account_id", None)),
            last_refresh: Some(Utc::now()),
            agent_identity: None,
        };

        let state = ChatgptAuthState {
            auth_dot_json: Arc::new(Mutex::new(Some(auth_dot_json))),
            client: create_client(),
        };
        let dummy_auth_id = NEXT_DUMMY_AUTH_ID.fetch_add(1, Ordering::Relaxed);
        let storage = create_auth_storage(
            PathBuf::from(format!("dummy-chatgpt-auth-{dummy_auth_id}")),
            AuthCredentialsStoreMode::Ephemeral,
            AuthKeyringBackendKind::default(),
        );
        Self::Chatgpt(ChatgptAuth { state, storage })
    }
}

impl ManagedChatGptAgentIdentityBinding {
    fn from_auth(auth: &CodexAuth, forced_workspace_id: Option<Vec<String>>) -> Option<Self> {
        if !auth.is_chatgpt_auth() {
            return None;
        }

        let token_data = auth.get_token_data().ok()?;
        let forced_workspace_id =
            forced_workspace_id
                .as_deref()
                .and_then(|workspace_ids| match workspace_ids {
                    [workspace_id] if !workspace_id.is_empty() => Some(workspace_id.clone()),
                    _ => None,
                });
        let account_id = forced_workspace_id
            .or(token_data
                .account_id
                .clone()
                .filter(|value| !value.is_empty()))
            .or(token_data.id_token.chatgpt_account_id.clone())?;
        let chatgpt_user_id = token_data
            .id_token
            .chatgpt_user_id
            .clone()
            .filter(|value| !value.is_empty())?;

        Some(Self {
            account_id,
            chatgpt_user_id,
            email: token_data.id_token.email.clone(),
            plan_type: auth.account_plan_type().unwrap_or(AccountPlanType::Unknown),
            chatgpt_account_is_fedramp: auth.is_fedramp_account(),
            access_token: token_data.access_token,
        })
    }
}

impl ChatgptAuth {
    fn current_auth_json(&self) -> Option<AuthDotJson> {
        #[expect(clippy::unwrap_used)]
        self.state.auth_dot_json.lock().unwrap().clone()
    }

    fn current_token_data(&self) -> Option<TokenData> {
        self.current_auth_json().and_then(|auth| auth.tokens)
    }

    fn storage(&self) -> &Arc<dyn AuthStorageBackend> {
        &self.storage
    }

    fn client(&self) -> &HttpClient {
        &self.state.client
    }

    fn persist_agent_identity_record(
        &self,
        record: AgentIdentityAuthRecord,
    ) -> std::io::Result<()> {
        persist_agent_identity_record(&self.state.auth_dot_json, &self.storage, record)
    }
}

fn persist_agent_identity_record(
    auth_dot_json: &Arc<Mutex<Option<AuthDotJson>>>,
    storage: &Arc<dyn AuthStorageBackend>,
    record: AgentIdentityAuthRecord,
) -> std::io::Result<()> {
    let mut guard = auth_dot_json
        .lock()
        .map_err(|_| std::io::Error::other("failed to lock auth state"))?;
    let mut auth = storage
        .load()?
        .or_else(|| guard.clone())
        .ok_or_else(|| std::io::Error::other("auth data is not available"))?;
    auth.agent_identity = Some(AgentIdentityStorage::Record(record));
    storage.save(&auth)?;
    *guard = Some(auth);
    Ok(())
}

pub const OPENAI_API_KEY_ENV_VAR: &str = "OPENAI_API_KEY";
pub const CODEX_API_KEY_ENV_VAR: &str = "CODEX_API_KEY";
pub const CODEX_ACCESS_TOKEN_ENV_VAR: &str = "CODEX_ACCESS_TOKEN";

/// Reads `OPENAI_API_KEY` when present. Not used for primary Codex auth.
pub fn read_openai_api_key_from_env() -> Option<String> {
    env::var(OPENAI_API_KEY_ENV_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Reads `CODEX_API_KEY` when present. Not used for primary Codex auth.
pub fn read_codex_api_key_from_env() -> Option<String> {
    read_non_empty_env_var(CODEX_API_KEY_ENV_VAR)
}

/// Reads `CODEX_ACCESS_TOKEN` when present. Not used for primary Codex auth.
pub fn read_codex_access_token_from_env() -> Option<String> {
    read_non_empty_env_var(CODEX_ACCESS_TOKEN_ENV_VAR)
}

fn read_non_empty_env_var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Delete the auth.json file inside `codex_home` if it exists. Returns `Ok(true)`
/// if a file was removed, `Ok(false)` if no auth file was present.
pub fn logout(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<bool> {
    let storage = create_auth_storage(
        codex_home.to_path_buf(),
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    storage.delete()
}

pub async fn logout_with_revoke(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    auth_route_config: Option<&AuthRouteConfig>,
) -> std::io::Result<bool> {
    let auth_dot_json = match load_auth_dot_json(
        codex_home,
        auth_credentials_store_mode,
        keyring_backend_kind,
    ) {
        Ok(auth_dot_json) => auth_dot_json,
        Err(err) => {
            tracing::warn!("failed to load stored auth during logout: {err}");
            None
        }
    };
    if let Err(err) = revoke_auth_tokens(auth_dot_json.as_ref(), auth_route_config).await {
        tracing::warn!("failed to revoke auth tokens during logout: {err}");
    }
    logout_all_stores(
        codex_home,
        auth_credentials_store_mode,
        keyring_backend_kind,
    )
}

pub(crate) async fn revoke_auth_tokens_if_superseded(
    previous_auth: Option<&AuthDotJson>,
    current_auth: &AuthDotJson,
    auth_route_config: Option<&AuthRouteConfig>,
) -> std::io::Result<()> {
    let Some(previous_auth) = previous_auth else {
        return Ok(());
    };
    if previous_auth.resolved_mode().ok() != Some(AuthMode::Chatgpt)
        || current_auth.resolved_mode().ok() != Some(AuthMode::Chatgpt)
    {
        return Ok(());
    }
    let Some(previous_tokens) = previous_auth.tokens.as_ref() else {
        return Ok(());
    };
    let Some(current_tokens) = current_auth.tokens.as_ref() else {
        return Ok(());
    };
    if previous_tokens.refresh_token == current_tokens.refresh_token
        || (previous_tokens.refresh_token.is_empty()
            && previous_tokens.access_token == current_tokens.access_token)
    {
        return Ok(());
    }
    revoke_auth_tokens(Some(previous_auth), auth_route_config).await
}

/// Writes an `auth.json` for Agent Identity JWT credentials.
pub async fn login_with_access_token(
    codex_home: &Path,
    access_token: &str,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    forced_chatgpt_workspace_id: Option<&[String]>,
    chatgpt_base_url: Option<&str>,
    keyring_backend_kind: AuthKeyringBackendKind,
    auth_route_config: Option<&AuthRouteConfig>,
) -> std::io::Result<()> {
    let _ = forced_chatgpt_workspace_id;
    let CodexAccessToken::AgentIdentityJwt(jwt) = classify_codex_access_token(access_token) else {
        return Err(std::io::Error::other(
            "personal access token auth is no longer supported. Sign in with a ChatGPT account via the account pool.",
        ));
    };
    let base_url = chatgpt_base_url
        .unwrap_or(ChatGptEnvironment::default().chatgpt_base_url())
        .trim_end_matches('/')
        .to_string();
    verified_record_from_jwt(jwt, &base_url, auth_route_config).await?;
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::AgentIdentity),
        tokens: None,
        pool_account_id: None,
        last_refresh: None,
        agent_identity: Some(AgentIdentityStorage::Jwt(jwt.to_string())),
    };
    save_auth(
        codex_home,
        &auth_dot_json,
        auth_credentials_store_mode,
        keyring_backend_kind,
    )
}

/// Persist the provided auth payload using the specified backend.
pub fn save_auth(
    codex_home: &Path,
    auth: &AuthDotJson,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<()> {
    let storage = create_auth_storage(
        codex_home.to_path_buf(),
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    storage.save(auth)
}

/// Load the raw stored auth payload without applying environment overrides.
///
/// Returns `None` when no credentials are stored. Prefer `AuthManager` for
/// ordinary production reads; this helper is for tests and write-side
/// maintenance that must inspect the exact payload in storage.
pub fn load_auth_dot_json(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<Option<AuthDotJson>> {
    let storage = create_auth_storage(
        codex_home.to_path_buf(),
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    storage.load()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub codex_home: PathBuf,
    pub auth_credentials_store_mode: AuthCredentialsStoreMode,
    pub keyring_backend_kind: AuthKeyringBackendKind,
    pub forced_login_method: Option<ForcedLoginMethod>,
    pub chatgpt_base_url: Option<String>,
    pub forced_chatgpt_workspace_id: Option<Vec<String>>,
    pub auth_route_config: Option<AuthRouteConfig>,
}

/// Enforces configured login restrictions using auth-owned HTTP settings.
pub async fn enforce_login_restrictions(config: &AuthConfig) -> std::io::Result<()> {
    let agent_identity_authapi_base_url =
        agent_identity_authapi_base_url(config.chatgpt_base_url.as_deref()).ok();
    enforce_login_restrictions_with_agent_identity_authapi_base_url(
        config,
        agent_identity_authapi_base_url.as_deref(),
    )
    .await
}

async fn enforce_login_restrictions_with_agent_identity_authapi_base_url(
    config: &AuthConfig,
    agent_identity_authapi_base_url: Option<&str>,
) -> std::io::Result<()> {
    let Some(auth) = load_auth(
        &config.codex_home,
        config.auth_credentials_store_mode,
        /*forced_chatgpt_workspace_id*/ None,
        config.chatgpt_base_url.as_deref(),
        config.keyring_backend_kind,
        agent_identity_authapi_base_url,
        config.auth_route_config.as_ref(),
    )
    .await?
    else {
        return Ok(());
    };

    if let Some(ForcedLoginMethod::Chatgpt) = config.forced_login_method
        && !matches!(
            auth.auth_mode(),
            AuthMode::Chatgpt | AuthMode::Headers | AuthMode::AgentIdentity
        )
    {
        return Err(std::io::Error::other(
            "ChatGPT login is required. Existing credentials were preserved.",
        ));
    }

    if let Some(expected_account_ids) = config.forced_chatgpt_workspace_id.as_deref() {
        let chatgpt_account_id = match &auth {
            CodexAuth::Headers(_) => {
                return Ok(());
            }
            CodexAuth::AgentIdentity(_) => auth.get_account_id(),
            CodexAuth::Chatgpt(_) => {
                let token_data = match auth.get_token_data() {
                    Ok(data) => data,
                    Err(err) => {
                        return Err(std::io::Error::other(format!(
                            "Failed to load ChatGPT credentials while enforcing workspace restrictions: {err}. Existing credentials were preserved."
                        )));
                    }
                };
                token_data.id_token.chatgpt_account_id
            }
        };

        // workspace is the external identifier for account id.
        let chatgpt_account_id = chatgpt_account_id.as_deref();
        if !chatgpt_account_id.is_some_and(|actual| {
            expected_account_ids
                .iter()
                .any(|expected| expected == actual)
        }) {
            let expected_workspaces = expected_account_ids.join(", ");
            let message = match chatgpt_account_id {
                Some(actual) => format!(
                    "Login is restricted to workspace(s) {expected_workspaces}, but current credentials belong to {actual}. Existing credentials were preserved."
                ),
                None => format!(
                    "Login is restricted to workspace(s) {expected_workspaces}, but current credentials lack a workspace identifier. Existing credentials were preserved."
                ),
            };
            return Err(std::io::Error::other(message));
        }
    }

    Ok(())
}

fn logout_all_stores(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<bool> {
    if auth_credentials_store_mode == AuthCredentialsStoreMode::Ephemeral {
        return logout(
            codex_home,
            AuthCredentialsStoreMode::Ephemeral,
            AuthKeyringBackendKind::default(),
        );
    }
    let removed_ephemeral = logout(
        codex_home,
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::default(),
    )?;
    let removed_managed = logout(
        codex_home,
        auth_credentials_store_mode,
        keyring_backend_kind,
    )?;
    Ok(removed_ephemeral || removed_managed)
}

#[allow(clippy::too_many_arguments)]
async fn load_auth(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    _forced_chatgpt_workspace_id: Option<&[String]>,
    chatgpt_base_url: Option<&str>,
    keyring_backend_kind: AuthKeyringBackendKind,
    agent_identity_authapi_base_url: Option<&str>,
    auth_route_config: Option<&AuthRouteConfig>,
) -> std::io::Result<Option<CodexAuth>> {
    // Ephemeral store first (Agent Identity / temporary credentials).
    let ephemeral_storage = create_auth_storage(
        codex_home.to_path_buf(),
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::default(),
    );
    if let Some(auth_dot_json) = ephemeral_storage.load()? {
        let auth = CodexAuth::from_auth_dot_json(
            codex_home,
            auth_dot_json,
            AuthCredentialsStoreMode::Ephemeral,
            chatgpt_base_url,
            keyring_backend_kind,
            agent_identity_authapi_base_url,
            auth_route_config,
        )
        .await?;
        return Ok(Some(auth));
    }

    if auth_credentials_store_mode == AuthCredentialsStoreMode::Ephemeral {
        return Ok(None);
    }

    let storage = create_auth_storage(
        codex_home.to_path_buf(),
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    let auth_dot_json = match storage.load()? {
        Some(auth) => auth,
        None => return Ok(None),
    };

    let auth = CodexAuth::from_auth_dot_json(
        codex_home,
        auth_dot_json,
        auth_credentials_store_mode,
        chatgpt_base_url,
        keyring_backend_kind,
        agent_identity_authapi_base_url,
        auth_route_config,
    )
    .await?;
    Ok(Some(auth))
}

// Persist refreshed tokens into auth storage and update last_refresh.
fn apply_refreshed_tokens(
    mut auth_dot_json: AuthDotJson,
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
) -> std::io::Result<AuthDotJson> {
    let tokens = auth_dot_json.tokens.get_or_insert_with(TokenData::default);
    if let Some(id_token) = id_token {
        tokens.id_token = parse_chatgpt_jwt_claims(&id_token).map_err(std::io::Error::other)?;
    }
    if let Some(access_token) = access_token {
        tokens.access_token = access_token;
    }
    if let Some(refresh_token) = refresh_token {
        tokens.refresh_token = refresh_token;
    }
    auth_dot_json.last_refresh = Some(Utc::now());
    Ok(auth_dot_json)
}

// Persist refreshed tokens into auth storage and update last_refresh.
#[cfg(test)]
fn persist_tokens(
    storage: &Arc<dyn AuthStorageBackend>,
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
) -> std::io::Result<AuthDotJson> {
    let auth_dot_json = storage
        .load()?
        .ok_or(std::io::Error::other("Token data is not available."))?;
    let auth_dot_json =
        apply_refreshed_tokens(auth_dot_json, id_token, access_token, refresh_token)?;
    storage.save(&auth_dot_json)?;
    Ok(auth_dot_json)
}

// Requests refreshed ChatGPT OAuth tokens from the auth service using a refresh token.
// The caller is responsible for persisting any returned tokens.
async fn request_chatgpt_token_refresh(
    refresh_token: String,
    client: &HttpClient,
) -> Result<RefreshResponse, RefreshTokenError> {
    let refresh_request = RefreshRequest {
        client_id: oauth_client_id(),
        grant_type: "refresh_token",
        refresh_token,
    };
    let endpoint = refresh_token_endpoint();

    // Use shared client factory to include standard headers.
    // Hard cap at 60 s — strictly less than the 90 s lock TTL — so the call
    // cannot outlast the lock and allow a second process to acquire the same
    // lock and fire a parallel OAuth call with the same refresh token (R2),
    // which would produce refresh_token_reused and permanently kill the account.
    let response = client
        .post(endpoint.as_str())
        .header("Content-Type", "application/json")
        .json(&refresh_request)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|err| RefreshTokenError::Transient(std::io::Error::other(err)))?;

    let status = response.status();
    if status.is_success() {
        let refresh_response = response
            .json::<RefreshResponse>()
            .await
            .map_err(|err| RefreshTokenError::Transient(std::io::Error::other(err)))?;
        Ok(refresh_response)
    } else {
        let body = response.text().await.unwrap_or_default();
        tracing::error!("Failed to refresh token: {status}: {body}");
        let failed = classify_refresh_token_failure(&body);
        if status == StatusCode::UNAUTHORIZED || failed.reason != RefreshTokenFailedReason::Other {
            Err(RefreshTokenError::Permanent(failed))
        } else {
            let message = try_parse_error_message(&body);
            Err(RefreshTokenError::Transient(std::io::Error::other(
                format!("Failed to refresh token: {status}: {message}"),
            )))
        }
    }
}

fn classify_refresh_token_failure(body: &str) -> RefreshTokenFailedError {
    let code = extract_refresh_token_error_code(body);

    let normalized_code = code.as_deref().map(str::to_ascii_lowercase);
    let reason = match normalized_code.as_deref() {
        Some("refresh_token_expired") => RefreshTokenFailedReason::Expired,
        Some("refresh_token_reused") => RefreshTokenFailedReason::Exhausted,
        Some("refresh_token_invalidated") => RefreshTokenFailedReason::Revoked,
        _ => RefreshTokenFailedReason::Other,
    };

    if reason == RefreshTokenFailedReason::Other {
        tracing::warn!(
            backend_code = normalized_code.as_deref(),
            backend_body = body,
            "Encountered unknown response while refreshing token"
        );
    }

    let message = match reason {
        RefreshTokenFailedReason::Expired => REFRESH_TOKEN_EXPIRED_MESSAGE.to_string(),
        RefreshTokenFailedReason::Exhausted => REFRESH_TOKEN_REUSED_MESSAGE.to_string(),
        RefreshTokenFailedReason::Revoked => REFRESH_TOKEN_INVALIDATED_MESSAGE.to_string(),
        RefreshTokenFailedReason::Other => REFRESH_TOKEN_UNKNOWN_MESSAGE.to_string(),
    };

    RefreshTokenFailedError::new(reason, message)
}

fn extract_refresh_token_error_code(body: &str) -> Option<String> {
    if body.trim().is_empty() {
        return None;
    }

    let Value::Object(map) = serde_json::from_str::<Value>(body).ok()? else {
        return None;
    };

    if let Some(error_value) = map.get("error") {
        match error_value {
            Value::Object(obj) => {
                if let Some(code) = obj.get("code").and_then(Value::as_str) {
                    return Some(code.to_string());
                }
            }
            Value::String(code) => {
                return Some(code.to_string());
            }
            _ => {}
        }
    }

    map.get("code").and_then(Value::as_str).map(str::to_string)
}

#[derive(Serialize)]
struct RefreshRequest {
    client_id: String,
    grant_type: &'static str,
    refresh_token: String,
}

#[derive(Deserialize, Clone)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

// Shared constant for token refresh (client id used for oauth token refresh flow)
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub fn oauth_client_id() -> String {
    std::env::var(CLIENT_ID_OVERRIDE_ENV_VAR)
        .ok()
        .filter(|client_id| !client_id.trim().is_empty())
        .unwrap_or_else(|| CLIENT_ID.to_string())
}

fn refresh_token_endpoint() -> String {
    std::env::var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR)
        .unwrap_or_else(|_| REFRESH_TOKEN_URL.to_string())
}

impl AuthDotJson {
    pub(super) fn resolved_mode(&self) -> std::io::Result<AuthMode> {
        match self.auth_mode {
            Some(AuthMode::Chatgpt) | None => Ok(AuthMode::Chatgpt),
            Some(AuthMode::AgentIdentity) => Ok(AuthMode::AgentIdentity),
            Some(AuthMode::Headers) => Ok(AuthMode::Headers),
        }
    }

    fn storage_mode(
        &self,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
    ) -> AuthCredentialsStoreMode {
        auth_credentials_store_mode
    }
}

async fn clear_top_level_pool_auth_copy(
    account_pool: &ChatgptAccountPool,
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) {
    let top_level_auth = match load_auth_dot_json(
        codex_home,
        auth_credentials_store_mode,
        keyring_backend_kind,
    ) {
        Ok(Some(auth)) => auth,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(
                %err,
                "preserving top-level ChatGPT auth because the source copy could not be loaded"
            );
            return;
        }
    };
    let Some(top_level_tokens) = top_level_auth.tokens.as_ref() else {
        return;
    };
    let Some(workspace_account_id) = top_level_tokens
        .account_id
        .as_deref()
        .or(top_level_tokens.id_token.chatgpt_account_id.as_deref())
    else {
        tracing::warn!(
            "preserving top-level ChatGPT auth because its pool account id could not be derived"
        );
        return;
    };
    let source_account_id = top_level_auth.pool_account_id.clone().unwrap_or_else(|| {
        derive_pool_account_id(
            workspace_account_id,
            top_level_tokens.id_token.member_identity_key().as_deref(),
        )
    });

    match account_pool.read_account_tokens(&source_account_id).await {
        Ok(Some(pool_auth))
            if pool_auth.tokens.as_ref().is_some_and(|pool_tokens| {
                !top_level_tokens.access_token.is_empty()
                    && !top_level_tokens.refresh_token.is_empty()
                    && pool_tokens.access_token == top_level_tokens.access_token
                    && pool_tokens.refresh_token == top_level_tokens.refresh_token
            }) => {}
        Ok(_) => {
            tracing::warn!(
                account_id = source_account_id,
                "preserving top-level ChatGPT auth because its exact credential is not recoverable from the pool"
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                account_id = source_account_id,
                %err,
                "preserving top-level ChatGPT auth because its pool copy could not be verified"
            );
            return;
        }
    }
    if let Err(err) = logout(
        codex_home,
        auth_credentials_store_mode,
        keyring_backend_kind,
    ) {
        tracing::warn!("failed to clear duplicated top-level ChatGPT auth copy: {err}");
    }
}

/// Internal cached auth state.
#[derive(Clone)]
struct CachedAuth {
    auth: Option<CodexAuth>,
    /// Permanent refresh failure cached for the current auth snapshot so
    /// later refresh attempts for the same credentials fail fast without network.
    permanent_refresh_failure: Option<AuthScopedRefreshFailure>,
}

#[derive(Clone)]
struct AuthScopedRefreshFailure {
    auth: CodexAuth,
    error: RefreshTokenFailedError,
}

impl Debug for CachedAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedAuth")
            .field(
                "auth_mode",
                &self.auth.as_ref().map(CodexAuth::api_auth_mode),
            )
            .field(
                "permanent_refresh_failure",
                &self
                    .permanent_refresh_failure
                    .as_ref()
                    .map(|failure| failure.error.reason),
            )
            .finish()
    }
}

enum UnauthorizedRecoveryStep {
    Reload,
    RefreshToken,
    ExternalRefresh,
    Done,
}

enum ReloadOutcome {
    /// Reload was performed and the cached auth changed
    ReloadedChanged,
    /// Reload was performed and the cached auth remained the same
    ReloadedNoChange,
    /// Reload was skipped (missing or mismatched account id)
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnauthorizedRecoveryMode {
    Managed,
    External,
}

// UnauthorizedRecovery is a state machine that handles an attempt to refresh the authentication when requests
// to API fail with 401 status code.
// The client calls next() every time it encounters a 401 error, one time per retry.
// For API key based authentication, we don't do anything and let the error bubble to the user.
//
// For ChatGPT based authentication, we:
// 1. Attempt to reload the auth data from disk. We only reload if the account id matches the one the current process is running as.
// 2. Attempt to refresh the token using OAuth token refresh flow.
// If after both steps the server still responds with 401 we let the error bubble to the user.
//
// For external auth sources, UnauthorizedRecovery retries once by asking the
// configured provider to refresh and caching the returned auth through the same
// path used by other auth sources.
pub struct UnauthorizedRecovery {
    manager: Arc<AuthManager>,
    step: UnauthorizedRecoveryStep,
    expected_account_id: Option<String>,
    mode: UnauthorizedRecoveryMode,
    stale_access_token: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnauthorizedRecoveryStepResult {
    auth_state_changed: Option<bool>,
}

impl UnauthorizedRecoveryStepResult {
    pub fn auth_state_changed(&self) -> Option<bool> {
        self.auth_state_changed
    }
}

impl UnauthorizedRecovery {
    fn new(manager: Arc<AuthManager>) -> Self {
        let cached_auth = manager.auth_cached();
        let expected_account_id = cached_auth
            .as_ref()
            .and_then(|auth| auth.get_pool_account_id().or_else(|| auth.get_account_id()));
        let mode = if manager.has_external_auth() {
            UnauthorizedRecoveryMode::External
        } else {
            UnauthorizedRecoveryMode::Managed
        };
        let step = match mode {
            UnauthorizedRecoveryMode::Managed => UnauthorizedRecoveryStep::Reload,
            UnauthorizedRecoveryMode::External => UnauthorizedRecoveryStep::ExternalRefresh,
        };
        Self {
            manager,
            step,
            expected_account_id,
            mode,
            stale_access_token: cached_auth
                .as_ref()
                .and_then(CodexAuth::get_current_token_data)
                .map(|tokens| tokens.access_token),
        }
    }

    pub fn has_next(&self) -> bool {
        if !self
            .manager
            .auth_cached()
            .as_ref()
            .is_some_and(CodexAuth::supports_unauthorized_recovery)
        {
            return false;
        }

        if self.mode == UnauthorizedRecoveryMode::External && !self.manager.has_external_auth() {
            return false;
        }

        !matches!(self.step, UnauthorizedRecoveryStep::Done)
    }

    pub fn unavailable_reason(&self) -> &'static str {
        if !self
            .manager
            .auth_cached()
            .as_ref()
            .is_some_and(CodexAuth::supports_unauthorized_recovery)
        {
            return "not_chatgpt_auth";
        }

        if self.mode == UnauthorizedRecoveryMode::External && !self.manager.has_external_auth() {
            return "no_external_auth";
        }

        if matches!(self.step, UnauthorizedRecoveryStep::Done) {
            return "recovery_exhausted";
        }

        "ready"
    }

    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            UnauthorizedRecoveryMode::Managed => "managed",
            UnauthorizedRecoveryMode::External => "external",
        }
    }

    pub fn step_name(&self) -> &'static str {
        match self.step {
            UnauthorizedRecoveryStep::Reload => "reload",
            UnauthorizedRecoveryStep::RefreshToken => "refresh_token",
            UnauthorizedRecoveryStep::ExternalRefresh => "external_refresh",
            UnauthorizedRecoveryStep::Done => "done",
        }
    }

    pub fn current_auth_is_chatgpt(&self) -> bool {
        self.manager
            .auth_cached()
            .as_ref()
            .is_some_and(CodexAuth::is_chatgpt_auth)
    }

    pub async fn next(&mut self) -> Result<UnauthorizedRecoveryStepResult, RefreshTokenError> {
        if !self.has_next() {
            return Err(RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                RefreshTokenFailedReason::Other,
                "No more recovery steps available.",
            )));
        }

        match self.step {
            UnauthorizedRecoveryStep::Reload => {
                match self
                    .manager
                    .reload_if_account_id_matches(self.expected_account_id.as_deref())
                    .await
                {
                    ReloadOutcome::ReloadedChanged => {
                        self.step = UnauthorizedRecoveryStep::RefreshToken;
                        return Ok(UnauthorizedRecoveryStepResult {
                            auth_state_changed: Some(true),
                        });
                    }
                    ReloadOutcome::ReloadedNoChange => {
                        self.step = UnauthorizedRecoveryStep::RefreshToken;
                        return Ok(UnauthorizedRecoveryStepResult {
                            auth_state_changed: Some(false),
                        });
                    }
                    ReloadOutcome::Skipped => {
                        self.step = UnauthorizedRecoveryStep::Done;
                        return Err(RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                            RefreshTokenFailedReason::Other,
                            REFRESH_TOKEN_ACCOUNT_MISMATCH_MESSAGE.to_string(),
                        )));
                    }
                }
            }
            UnauthorizedRecoveryStep::RefreshToken => {
                self.manager
                    .refresh_token_from_authority_forced(self.stale_access_token.as_deref())
                    .await?;
                self.step = UnauthorizedRecoveryStep::Done;
                return Ok(UnauthorizedRecoveryStepResult {
                    auth_state_changed: Some(true),
                });
            }
            UnauthorizedRecoveryStep::ExternalRefresh => {
                self.manager.refresh_token_from_authority().await?;
                self.step = UnauthorizedRecoveryStep::Done;
                return Ok(UnauthorizedRecoveryStepResult {
                    auth_state_changed: Some(true),
                });
            }
            UnauthorizedRecoveryStep::Done => {}
        }
        Ok(UnauthorizedRecoveryStepResult {
            auth_state_changed: None,
        })
    }
}

/// Central manager providing a single source of truth for auth.json derived
/// authentication data. It loads once (or on preference change) and then
/// hands out cloned `CodexAuth` values so the rest of the program has a
/// consistent snapshot.
///
/// External modifications to `auth.json` will NOT be observed until
/// `reload()` is called explicitly. This matches the design goal of avoiding
/// different parts of the program seeing inconsistent auth data mid‑run.
pub struct AuthManager {
    codex_home: PathBuf,
    inner: RwLock<CachedAuth>,
    auth_change_tx: watch::Sender<u64>,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    forced_chatgpt_workspace_id: RwLock<Option<Vec<String>>>,
    chatgpt_base_url: Option<String>,
    agent_identity_authapi_base_url: Option<String>,
    refresh_lock: Semaphore,
    agent_identity_lock: Semaphore,
    agent_identity_bootstrap_cooldown: Mutex<AgentIdentityBootstrapCooldown>,
    external_auth: RwLock<Option<Arc<dyn ExternalAuth>>>,
    account_pool: Option<ChatgptAccountPool>,
    auth_route_config: Option<AuthRouteConfig>,
}

/// Account-scoped rate-limit data collected after a turn.
///
#[derive(Debug, Default)]
pub struct AccountPoolPostTurnRateLimits {
    pub snapshots: Vec<RateLimitSnapshot>,
}

/// Configuration view required to construct a shared [`AuthManager`].
///
/// Implementations should return the auth-related config values for the
/// already-resolved runtime configuration. The primary implementation is
/// `codex_core::config::Config`, but this trait keeps `codex-login` independent
/// from `codex-core`.
pub trait AuthManagerConfig {
    /// Returns the Codex home directory used for auth storage.
    fn codex_home(&self) -> PathBuf;

    /// Returns the CLI auth credential storage mode for auth loading.
    fn cli_auth_credentials_store_mode(&self) -> AuthCredentialsStoreMode;

    /// Returns the backend to use when CLI auth keyring storage is selected.
    fn auth_keyring_backend_kind(&self) -> AuthKeyringBackendKind;

    /// Returns the workspace IDs that ChatGPT auth should be restricted to, if any.
    fn forced_chatgpt_workspace_id(&self) -> Option<Vec<String>>;

    /// Returns the ChatGPT backend base URL used for first-party backend authorization.
    fn chatgpt_base_url(&self) -> String;

    /// Returns route-selection settings for auth-owned clients.
    fn auth_route_config(&self) -> Option<AuthRouteConfig>;
}

impl Debug for AuthManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthManager")
            .field("codex_home", &self.codex_home)
            .field("inner", &self.inner)
            .field(
                "auth_credentials_store_mode",
                &self.auth_credentials_store_mode,
            )
            .field("keyring_backend_kind", &self.keyring_backend_kind)
            .field(
                "forced_chatgpt_workspace_id",
                &self.forced_chatgpt_workspace_id,
            )
            .field("chatgpt_base_url", &self.chatgpt_base_url)
            .field("auth_route_config", &self.auth_route_config)
            .field("has_external_auth", &self.has_external_auth())
            .field("has_account_pool", &self.account_pool.is_some())
            .finish_non_exhaustive()
    }
}

fn should_consider_account_pool_auth(auth: Option<&CodexAuth>) -> bool {
    // Consult the account pool when we hold a managed ChatGPT auth, or when we
    // hold no auth at all. The auth-free case matters because a previous turn
    // may have logged out after the pool reported no eligible accounts; without
    // it a pool account that becomes idle again mid-session (a cooldown expires
    // or a new account is registered) is never picked up until the user
    // restarts or re-authenticates by hand. This mirrors the startup selection
    // path. An explicit non-pool credential (e.g. an API key) is left untouched.
    matches!(auth, None | Some(CodexAuth::Chatgpt(_)))
}

async fn load_startup_account_pool_auth(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    chatgpt_base_url: Option<&str>,
    keyring_backend_kind: AuthKeyringBackendKind,
    auth_route_config: Option<&AuthRouteConfig>,
    managed_auth: Option<CodexAuth>,
    account_pool: Option<&ChatgptAccountPool>,
) -> Option<CodexAuth> {
    let Some(account_pool) = account_pool else {
        return managed_auth;
    };
    let managed_chatgpt_auth_loaded = managed_auth
        .as_ref()
        .is_some_and(|auth| matches!(auth, CodexAuth::Chatgpt(_)));
    let candidate_auth = managed_auth;
    // ChatGPT credentials without an explicit pool_account_id are unfinished
    // migrations: fall through to resolve_turn_selection so the migrated pool
    // row can be activated. Only skip resolution when we already hold a
    // pool-managed id that is missing from the pool DB.
    if managed_chatgpt_auth_loaded
        && let Some(current_account_id) = candidate_auth
            .as_ref()
            .and_then(CodexAuth::get_pool_account_id)
    {
        match account_pool
            .account_last_auth_refresh_at(&current_account_id)
            .await
        {
            Ok(_) => {}
            Err(ChatgptAccountPoolError::AccountNotFound(_)) => {
                return candidate_auth;
            }
            Err(err) => {
                tracing::warn!(
                    account_id = current_account_id,
                    error = %err,
                    "failed to check whether startup ChatGPT auth belongs to the account pool; keeping existing managed auth"
                );
                return candidate_auth;
            }
        }
    }
    let fallback_auth = candidate_auth.clone();
    let current_account_id = fallback_auth
        .as_ref()
        .and_then(CodexAuth::get_pool_account_id);
    let selection = match account_pool
        .resolve_turn_selection(
            current_account_id.as_deref(),
            /*current_refresh_failed_permanently*/ false,
        )
        .await
    {
        Ok(selection) => selection,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to resolve ChatGPT account-pool startup selection; \
                 keeping existing managed auth"
            );
            return candidate_auth;
        }
    };
    let resolved = match selection {
        ChatgptAccountPoolSelectionOutcome::Unchanged => candidate_auth.clone(),
        ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts => return None,
        ChatgptAccountPoolSelectionOutcome::Activated { auth, .. } => {
            let agent_identity_authapi_base_url =
                agent_identity_authapi_base_url(chatgpt_base_url).ok();
            CodexAuth::from_auth_dot_json(
                codex_home,
                auth,
                auth_credentials_store_mode,
                chatgpt_base_url,
                keyring_backend_kind,
                agent_identity_authapi_base_url.as_deref(),
                auth_route_config,
            )
            .await
            .ok()
            .or(candidate_auth.clone())
        }
    };
    if resolved
        .as_ref()
        .and_then(CodexAuth::get_pool_account_id)
        .is_some()
    {
        clear_top_level_pool_auth_copy(
            account_pool,
            codex_home,
            auth_credentials_store_mode,
            keyring_backend_kind,
        )
        .await;
    }

    // Probe /usage before handing auth to the MCP layer. A 401 here is not
    // authoritative enough to rotate or invalidate the selected account at
    // startup, so keep the current auth and let the normal runtime refresh
    // and failover paths make the final decision with fresher context.
    if let Some(auth) = &resolved
        && let Some(account_id) = auth.get_account_id()
        && matches!(auth, CodexAuth::Chatgpt(_))
        && account_pool
            .probe_token_status(chatgpt_base_url, auth)
            .await
            == Some(reqwest::StatusCode::UNAUTHORIZED)
    {
        tracing::warn!(
            account_id,
            "startup probe got 401; keeping current auth because /usage 401 is non-authoritative at startup"
        );
    }

    resolved
}

fn default_agent_identity_authapi_base_url() -> Option<String> {
    agent_identity_authapi_base_url(/*chatgpt_base_url*/ None).ok()
}

impl AuthManager {
    /// Create a new manager loading the initial auth using the provided
    /// preferred auth method. Errors loading auth are swallowed; `auth()` will
    /// simply return `None` in that case so callers can treat it as an
    /// unauthenticated state.
    pub async fn new(
        codex_home: PathBuf,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        forced_chatgpt_workspace_id: Option<Vec<String>>,
        chatgpt_base_url: Option<String>,
        keyring_backend_kind: AuthKeyringBackendKind,
        auth_route_config: Option<AuthRouteConfig>,
    ) -> Self {
        let agent_identity_authapi_base_url =
            agent_identity_authapi_base_url(chatgpt_base_url.as_deref()).ok();
        let managed_auth = load_auth(
            &codex_home,
            auth_credentials_store_mode,
            forced_chatgpt_workspace_id.as_deref(),
            chatgpt_base_url.as_deref(),
            keyring_backend_kind,
            agent_identity_authapi_base_url.as_deref(),
            auth_route_config.as_ref(),
        )
        .await
        .ok()
        .flatten();
        let account_pool = ChatgptAccountPool::open_with_auth_config(
            codex_home.clone(),
            auth_credentials_store_mode,
            chatgpt_base_url.clone(),
            keyring_backend_kind,
            auth_route_config.clone(),
        )
        .await
        .map_err(|err| {
            tracing::warn!("failed to initialize ChatGPT account pool: {err}");
            err
        })
        .ok();
        // Consult the account pool at startup when we already hold a managed
        // ChatGPT auth, or when we hold no auth at all. The auth-free case
        // matters because a previous run may have logged out after the pool
        // reported no eligible accounts (or this CLI relies entirely on the
        // pool); without it a newly idle pool account is never picked up until
        // the user re-authenticates by hand. An explicit non-pool credential
        // (e.g. Agent Identity) is left untouched.
        let consult_account_pool =
            matches!(managed_auth.as_ref(), None | Some(CodexAuth::Chatgpt(_)));
        let managed_auth = if consult_account_pool {
            load_startup_account_pool_auth(
                &codex_home,
                auth_credentials_store_mode,
                chatgpt_base_url.as_deref(),
                keyring_backend_kind,
                auth_route_config.as_ref(),
                managed_auth,
                account_pool.as_ref(),
            )
            .await
        } else {
            managed_auth
        };
        let (auth_change_tx, _auth_change_rx) = watch::channel(0);
        Self {
            codex_home,
            inner: RwLock::new(CachedAuth {
                auth: managed_auth,
                permanent_refresh_failure: None,
            }),
            auth_change_tx,
            auth_credentials_store_mode,
            keyring_backend_kind,
            forced_chatgpt_workspace_id: RwLock::new(forced_chatgpt_workspace_id),
            chatgpt_base_url,
            agent_identity_authapi_base_url,
            refresh_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_bootstrap_cooldown: Mutex::default(),
            external_auth: RwLock::new(None),
            account_pool,
            auth_route_config,
        }
    }

    /// Create an AuthManager with a specific CodexAuth, for testing only.
    pub fn from_auth_for_testing(auth: CodexAuth) -> Arc<Self> {
        let cached = CachedAuth {
            auth: Some(auth),
            permanent_refresh_failure: None,
        };
        let (auth_change_tx, _auth_change_rx) = watch::channel(0);

        Arc::new(Self {
            codex_home: PathBuf::from("non-existent"),
            inner: RwLock::new(cached),
            auth_change_tx,
            auth_credentials_store_mode: AuthCredentialsStoreMode::File,
            keyring_backend_kind: AuthKeyringBackendKind::default(),
            forced_chatgpt_workspace_id: RwLock::new(None),
            chatgpt_base_url: None,
            agent_identity_authapi_base_url: default_agent_identity_authapi_base_url(),
            refresh_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_bootstrap_cooldown: Mutex::default(),
            external_auth: RwLock::new(None),
            account_pool: None,
            auth_route_config: None,
        })
    }

    /// Create an AuthManager with a specific CodexAuth and codex home, for testing only.
    pub fn from_auth_for_testing_with_home(auth: CodexAuth, codex_home: PathBuf) -> Arc<Self> {
        let cached = CachedAuth {
            auth: Some(auth),
            permanent_refresh_failure: None,
        };
        let (auth_change_tx, _auth_change_rx) = watch::channel(0);
        Arc::new(Self {
            codex_home,
            inner: RwLock::new(cached),
            auth_change_tx,
            auth_credentials_store_mode: AuthCredentialsStoreMode::File,
            keyring_backend_kind: AuthKeyringBackendKind::default(),
            forced_chatgpt_workspace_id: RwLock::new(None),
            chatgpt_base_url: None,
            agent_identity_authapi_base_url: default_agent_identity_authapi_base_url(),
            refresh_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_bootstrap_cooldown: Mutex::default(),
            external_auth: RwLock::new(None),
            account_pool: None,
            auth_route_config: None,
        })
    }

    /// Create an AuthManager with a specific CodexAuth and Agent Identity AuthAPI base URL, for testing only.
    #[doc(hidden)]
    pub fn from_auth_for_testing_with_agent_identity_authapi_base_url(
        auth: CodexAuth,
        agent_identity_authapi_base_url: String,
    ) -> Arc<Self> {
        let cached = CachedAuth {
            auth: Some(auth),
            permanent_refresh_failure: None,
        };
        let (auth_change_tx, _auth_change_rx) = watch::channel(0);
        Arc::new(Self {
            codex_home: PathBuf::from("non-existent"),
            inner: RwLock::new(cached),
            auth_change_tx,
            auth_credentials_store_mode: AuthCredentialsStoreMode::File,
            keyring_backend_kind: AuthKeyringBackendKind::default(),
            forced_chatgpt_workspace_id: RwLock::new(None),
            chatgpt_base_url: None,
            agent_identity_authapi_base_url: Some(
                agent_identity_authapi_base_url
                    .trim_end_matches('/')
                    .to_string(),
            ),
            refresh_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_bootstrap_cooldown: Mutex::default(),
            external_auth: RwLock::new(None),
            account_pool: None,
            auth_route_config: None,
        })
    }

    pub fn external_bearer_only(config: ModelProviderAuthInfo) -> Arc<Self> {
        let (auth_change_tx, _auth_change_rx) = watch::channel(0);
        Arc::new(Self {
            codex_home: PathBuf::from("non-existent"),
            inner: RwLock::new(CachedAuth {
                auth: None,
                permanent_refresh_failure: None,
            }),
            auth_change_tx,
            auth_credentials_store_mode: AuthCredentialsStoreMode::File,
            keyring_backend_kind: AuthKeyringBackendKind::default(),
            forced_chatgpt_workspace_id: RwLock::new(None),
            chatgpt_base_url: None,
            agent_identity_authapi_base_url: default_agent_identity_authapi_base_url(),
            refresh_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_lock: Semaphore::new(/*permits*/ 1),
            agent_identity_bootstrap_cooldown: Mutex::default(),
            external_auth: RwLock::new(Some(
                Arc::new(BearerTokenRefresher::new(config)) as Arc<dyn ExternalAuth>
            )),
            account_pool: None,
            auth_route_config: None,
        })
    }

    /// Current cached auth (clone) without attempting a refresh.
    pub fn auth_cached(&self) -> Option<CodexAuth> {
        self.inner
            .read()
            .ok()
            .and_then(|cached| cached.auth.clone())
    }

    /// Subscribes to cached auth changes that can affect request recovery.
    pub fn auth_change_receiver(&self) -> watch::Receiver<u64> {
        self.auth_change_tx.subscribe()
    }

    pub fn refresh_failure_for_auth(&self, auth: &CodexAuth) -> Option<RefreshTokenFailedError> {
        self.inner.read().ok().and_then(|cached| {
            cached
                .permanent_refresh_failure
                .as_ref()
                .filter(|failure| Self::auths_equal_for_refresh(Some(auth), Some(&failure.auth)))
                .map(|failure| failure.error.clone())
        })
    }

    /// Current cached auth (clone). May be `None` if not logged in or load failed.
    /// For managed ChatGPT auth that needs a proactive refresh, first performs
    /// a guarded reload and then refreshes only if the on-disk auth is unchanged.
    #[instrument(level = "trace", skip_all)]
    pub async fn auth(&self) -> Option<CodexAuth> {
        if self.has_external_auth() {
            self.reload().await;
            return self.auth_cached();
        }

        let auth = self.auth_cached()?;
        if matches!(auth, CodexAuth::Chatgpt(_)) {
            match self.pool_managed_chatgpt_refresh_context(&auth).await {
                Ok(Some((_account_pool, _chatgpt_auth, account_id))) => {
                    // Pool-managed: always read directly from the DB on every call — no
                    // timestamp comparison, no in-memory cache. This guarantees the CLI uses
                    // whatever token codex-accounts last wrote without any conditional logic.
                    // If the token is still stale after the reload (codex-accounts hasn't
                    // refreshed yet), skip the proactive refresh_token() wait — the CLI cannot
                    // call OAuth itself, so waiting is pointless. Let the API call proceed; a
                    // server 401 hits the forced=true path which failsover immediately.
                    if let Err(err) = self.reload_active_auth_from_pool_copy(&account_id).await {
                        tracing::warn!(
                            %account_id,
                            %err,
                            "pool auth DB reload failed; proceeding with cached auth"
                        );
                    }
                }
                Ok(None) if self.account_pool.is_some() => {
                    // ChatGPT credentials must be pool-managed. Activate (or fail over)
                    // through the account pool before returning auth to callers.
                    if let Err(err) = self.prepare_chatgpt_account_pool_for_turn().await {
                        tracing::warn!(%err, "pool account pickup failed for ChatGPT auth");
                    }
                }
                Ok(None) | Err(_) => {
                    // No account pool available: fall back to disk reload using the
                    // same identity key upstream uses (`get_account_id`).
                    let last_refresh = auth
                        .get_current_auth_json()
                        .and_then(|auth_dot_json| auth_dot_json.last_refresh);
                    let recently_refreshed = last_refresh.is_some_and(|last_refresh| {
                        last_refresh > Utc::now() - chrono::Duration::minutes(5)
                    });
                    if auth.chatgpt_access_token_is_stale() && !recently_refreshed {
                        if let Err(err) = self.refresh_token().await {
                            tracing::warn!(%err, "ChatGPT auth refresh failed; proceeding with cached auth");
                        }
                    } else if last_refresh.is_none_or(|last_refresh| {
                        last_refresh <= Utc::now() - chrono::Duration::days(7)
                    }) {
                        // Upstream uses workspace `get_account_id` for this reload guard.
                        // When a pool row id is present prefer it, because
                        // `reload_if_account_id_matches` compares against
                        // `pool_account_id.or(account_id)` and pool ids are `pool_<hash>`.
                        let expected_account_id =
                            auth.get_pool_account_id().or_else(|| auth.get_account_id());
                        let _ = self
                            .reload_if_account_id_matches(expected_account_id.as_deref())
                            .await;
                    }
                }
            }
        }
        // AgentIdentity needs no refresh — return cached auth as-is.
        self.auth_cached()
    }

    pub async fn agent_identity_auth(
        &self,
        policy: AgentIdentityAuthPolicy,
        session_source: SessionSource,
    ) -> std::io::Result<Option<AgentIdentityAuth>> {
        let Some(auth) = self.auth().await else {
            return Ok(None);
        };
        if policy == AgentIdentityAuthPolicy::ChatGptAuth && matches!(auth, CodexAuth::Chatgpt(_)) {
            let _bootstrap_permit = self
                .agent_identity_lock
                .acquire()
                .await
                .map_err(std::io::Error::other)?;
            let forced_chatgpt_workspace_id = self.forced_chatgpt_workspace_id();
            let cooldown_key = ManagedChatGptAgentIdentityBinding::from_auth(
                &auth,
                forced_chatgpt_workspace_id.clone(),
            )
            .and_then(|binding| {
                self.agent_identity_authapi_base_url
                    .as_ref()
                    .map(|base_url| (binding.account_id, base_url.clone()))
            });
            if let Some((account_id, authapi_base_url)) = cooldown_key.as_ref()
                && let Ok(mut cooldown) = self.agent_identity_bootstrap_cooldown.lock()
                && let Some(error) =
                    cooldown.error_for(account_id, authapi_base_url, Instant::now())
            {
                tracing::warn!("agent identity bootstrap retry suppressed during shared cooldown");
                return Err(std::io::Error::other(error));
            }

            let result = auth
                .agent_identity_auth(
                    policy,
                    self.agent_identity_authapi_base_url.as_deref(),
                    forced_chatgpt_workspace_id,
                    self.auth_route_config.as_ref(),
                    session_source,
                )
                .await;
            if let Ok(mut cooldown) = self.agent_identity_bootstrap_cooldown.lock() {
                if let (Err(err), Some((account_id, authapi_base_url))) = (&result, cooldown_key)
                    && let Some(error) = AgentIdentityAuthError::bootstrap_unavailable(err).cloned()
                {
                    cooldown.record_failure(account_id, authapi_base_url, error, Instant::now());
                } else {
                    cooldown.clear();
                }
            }
            return result;
        }
        auth.agent_identity_auth(
            policy,
            self.agent_identity_authapi_base_url.as_deref(),
            self.forced_chatgpt_workspace_id(),
            self.auth_route_config.as_ref(),
            session_source,
        )
        .await
    }

    /// Reloads auth from the active source. Returns whether the auth value changed.
    pub async fn reload(&self) -> bool {
        tracing::info!("Reloading auth");
        let new_auth = self.load_auth().await;
        self.set_cached_auth(new_auth)
    }

    async fn reload_if_account_id_matches(
        &self,
        expected_account_id: Option<&str>,
    ) -> ReloadOutcome {
        let expected_account_id = match expected_account_id {
            Some(account_id) => account_id,
            None => {
                tracing::info!("Skipping auth reload because no account id is available.");
                return ReloadOutcome::Skipped;
            }
        };

        let new_auth = self.load_auth().await;
        let new_account_id = new_auth
            .as_ref()
            .and_then(|auth| auth.get_pool_account_id().or_else(|| auth.get_account_id()));

        if new_account_id.as_deref() != Some(expected_account_id) {
            let found_account_id = new_account_id.as_deref().unwrap_or("unknown");
            tracing::info!(
                "Skipping auth reload due to account id mismatch (expected: {expected_account_id}, found: {found_account_id})"
            );
            return ReloadOutcome::Skipped;
        }

        tracing::info!("Reloading auth for account {expected_account_id}");
        let cached_before_reload = self.auth_cached();
        let auth_changed =
            !Self::auths_equal_for_refresh(cached_before_reload.as_ref(), new_auth.as_ref());
        self.set_cached_auth(new_auth);
        if auth_changed {
            ReloadOutcome::ReloadedChanged
        } else {
            ReloadOutcome::ReloadedNoChange
        }
    }

    fn auths_equal_for_refresh(a: Option<&CodexAuth>, b: Option<&CodexAuth>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => match (a.api_auth_mode(), b.api_auth_mode()) {
                (AuthMode::Chatgpt, AuthMode::Chatgpt) => {
                    a.get_current_auth_json() == b.get_current_auth_json()
                }
                (AuthMode::Headers, AuthMode::Headers) => a == b,
                (AuthMode::AgentIdentity, AuthMode::AgentIdentity) => match (a, b) {
                    (CodexAuth::AgentIdentity(a), CodexAuth::AgentIdentity(b)) => {
                        a.record() == b.record()
                    }
                    _ => false,
                },
                _ => false,
            },
            _ => false,
        }
    }

    fn auths_equal(a: Option<&CodexAuth>, b: Option<&CodexAuth>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Records a permanent refresh failure only if the failed refresh was
    /// attempted against the auth snapshot that is still cached.
    fn record_permanent_refresh_failure_if_unchanged(
        &self,
        attempted_auth: &CodexAuth,
        error: &RefreshTokenFailedError,
    ) {
        if let Ok(mut guard) = self.inner.write() {
            let current_auth_matches =
                Self::auths_equal_for_refresh(Some(attempted_auth), guard.auth.as_ref());
            if current_auth_matches {
                guard.permanent_refresh_failure = Some(AuthScopedRefreshFailure {
                    auth: attempted_auth.clone(),
                    error: error.clone(),
                });
            }
        }
    }

    async fn load_auth(&self) -> Option<CodexAuth> {
        if let Some(external_auth) = self.external_auth() {
            return match self.resolve_external_auth(&external_auth).await {
                Ok(auth) => Some(auth),
                Err(err) => {
                    tracing::error!("Failed to resolve external auth: {err}");
                    None
                }
            };
        }

        let pool_account_id = match self.auth_cached() {
            Some(auth) => self
                .pool_managed_chatgpt_refresh_context(&auth)
                .await
                .ok()
                .flatten()
                .map(|(_, _, account_id)| account_id),
            None => None,
        };
        if let Some(account_id) = pool_account_id {
            let account_pool = self.account_pool.as_ref()?;
            let pool_auth = match account_pool.read_account_tokens(&account_id).await {
                Ok(Some(pool_auth)) => pool_auth,
                Ok(None) => return None,
                Err(err) => {
                    tracing::warn!(
                        account_id,
                        error = %err,
                        "failed to load active pool-managed auth from account-pool DB"
                    );
                    return None;
                }
            };
            return CodexAuth::from_auth_dot_json(
                &self.codex_home,
                pool_auth,
                self.auth_credentials_store_mode,
                self.chatgpt_base_url.as_deref(),
                self.keyring_backend_kind,
                self.agent_identity_authapi_base_url.as_deref(),
                self.auth_route_config.as_ref(),
            )
            .await
            .ok();
        }
        let forced_chatgpt_workspace_id = self.forced_chatgpt_workspace_id();
        load_auth(
            &self.codex_home,
            self.auth_credentials_store_mode,
            forced_chatgpt_workspace_id.as_deref(),
            self.chatgpt_base_url.as_deref(),
            self.keyring_backend_kind,
            self.agent_identity_authapi_base_url.as_deref(),
            self.auth_route_config.as_ref(),
        )
        .await
        .ok()
        .flatten()
    }

    async fn activate_pool_managed_auth(&self, auth: AuthDotJson) -> Result<bool, std::io::Error> {
        let account_pool = self
            .account_pool
            .as_ref()
            .ok_or_else(|| std::io::Error::other("ChatGPT account pool is unavailable"))?;
        if auth.pool_account_id.is_none() {
            return Err(std::io::Error::other("pool auth is missing its account id"));
        }
        let auth = CodexAuth::from_auth_dot_json(
            &self.codex_home,
            auth,
            self.auth_credentials_store_mode,
            self.chatgpt_base_url.as_deref(),
            self.keyring_backend_kind,
            self.agent_identity_authapi_base_url.as_deref(),
            self.auth_route_config.as_ref(),
        )
        .await?;
        clear_top_level_pool_auth_copy(
            account_pool,
            &self.codex_home,
            self.auth_credentials_store_mode,
            self.keyring_backend_kind,
        )
        .await;
        Ok(self.set_cached_auth(Some(auth)))
    }

    fn set_cached_auth(&self, new_auth: Option<CodexAuth>) -> bool {
        if let Ok(mut guard) = self.inner.write() {
            let previous = guard.auth.as_ref();
            let changed = !AuthManager::auths_equal(previous, new_auth.as_ref());
            let auth_changed_for_refresh =
                !Self::auths_equal_for_refresh(previous, new_auth.as_ref());
            if auth_changed_for_refresh {
                guard.permanent_refresh_failure = None;
            }
            tracing::info!("Reloaded auth, changed: {changed}");
            guard.auth = new_auth;
            if auth_changed_for_refresh {
                self.auth_change_tx.send_modify(|revision| *revision += 1);
            }
            changed
        } else {
            false
        }
    }

    pub async fn set_external_auth(
        &self,
        external_auth: Arc<dyn ExternalAuth>,
    ) -> Result<(), RefreshTokenError> {
        let auth = self.resolve_external_auth(&external_auth).await?;
        *self.external_auth.write().map_err(|_| {
            RefreshTokenError::Transient(std::io::Error::other("external auth lock is poisoned"))
        })? = Some(external_auth);
        self.commit_external_auth(auth)
    }

    pub fn clear_external_auth(&self) {
        if let Ok(mut external_auth) = self.external_auth.write()
            && external_auth.take().is_some()
        {
            self.set_cached_auth(/*new_auth*/ None);
        }
    }

    pub fn set_forced_chatgpt_workspace_id(&self, workspace_id: Option<Vec<String>>) {
        if let Ok(mut guard) = self.forced_chatgpt_workspace_id.write()
            && *guard != workspace_id
        {
            *guard = workspace_id;
        }
    }

    pub fn forced_chatgpt_workspace_id(&self) -> Option<Vec<String>> {
        self.forced_chatgpt_workspace_id
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    pub fn has_external_auth(&self) -> bool {
        self.external_auth().is_some()
    }

    pub fn is_external_chatgpt_auth_active(&self) -> bool {
        false
    }

    pub fn codex_api_key_env_enabled(&self) -> bool {
        false
    }

    /// Convenience constructor returning an `Arc` wrapper.
    pub async fn shared(
        codex_home: PathBuf,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        forced_chatgpt_workspace_id: Option<Vec<String>>,
        chatgpt_base_url: Option<String>,
        keyring_backend_kind: AuthKeyringBackendKind,
        auth_route_config: Option<AuthRouteConfig>,
    ) -> Arc<Self> {
        Arc::new(
            Self::new(
                codex_home,
                auth_credentials_store_mode,
                forced_chatgpt_workspace_id,
                chatgpt_base_url,
                keyring_backend_kind,
                auth_route_config,
            )
            .await,
        )
    }

    /// Convenience constructor returning an `Arc` wrapper from resolved config.
    pub async fn shared_from_config(config: &impl AuthManagerConfig) -> Arc<Self> {
        Self::shared(
            config.codex_home(),
            config.cli_auth_credentials_store_mode(),
            config.forced_chatgpt_workspace_id(),
            Some(config.chatgpt_base_url()),
            config.auth_keyring_backend_kind(),
            config.auth_route_config(),
        )
        .await
    }

    pub fn chatgpt_account_pool(&self) -> Option<ChatgptAccountPool> {
        self.account_pool.clone()
    }

    /// Returns ChatGPT auth for account-management requests even when every pool
    /// account is temporarily ineligible for model turns.
    pub async fn chatgpt_account_management_auth(&self) -> Option<CodexAuth> {
        if let Some(auth) = self.auth().await {
            return Some(auth);
        }
        let account_pool = self.account_pool.as_ref()?;
        let (_, auth) = match account_pool.selected_account_auth().await {
            Ok(Some(selected)) => selected,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to load selected ChatGPT account for account management"
                );
                return None;
            }
        };
        CodexAuth::from_auth_dot_json(
            &self.codex_home,
            auth,
            self.auth_credentials_store_mode,
            self.chatgpt_base_url.as_deref(),
            self.keyring_backend_kind,
            self.agent_identity_authapi_base_url.as_deref(),
            self.auth_route_config.as_ref(),
        )
        .await
        .map_err(|err| {
            tracing::warn!(
                error = %err,
                "failed to construct selected ChatGPT auth for account management"
            );
        })
        .ok()
    }

    pub async fn record_account_pool_activity(&self) {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return;
        };
        let account_id = self
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id());
        let Some(account_id) = account_id else {
            return;
        };
        account_pool.record_account_activity(&account_id).await;
    }

    /// Records activity for an explicit pool account ID rather than re-reading
    /// `auth_cached()`. Use this when the account ID was captured at turn-start
    /// and must remain stable across mid-turn failovers.
    pub async fn record_pool_account_activity_for(&self, account_id: &str) {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return;
        };
        account_pool.record_account_activity(account_id).await;
    }

    pub async fn clear_account_pool_activity(&self) {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return;
        };
        let account_id = self
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id());
        let Some(account_id) = account_id else {
            return;
        };
        account_pool.clear_account_activity(&account_id).await;
    }

    /// Clears activity for an explicit pool account ID rather than re-reading
    /// `auth_cached()`. Use this when the account ID was captured at turn-start
    /// and must remain stable across mid-turn failovers.
    pub async fn clear_pool_account_activity_for(&self, account_id: &str) {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return;
        };
        account_pool.clear_account_activity(account_id).await;
    }

    pub async fn record_account_pool_rate_limits_fetch_for(
        &self,
        account_id: &str,
        snapshots: &[RateLimitSnapshot],
    ) {
        if snapshots.is_empty() {
            return;
        }
        let Some(account_pool) = self.account_pool.as_ref() else {
            return;
        };
        if let Err(err) = account_pool
            .record_fetched_rate_limits(account_id, snapshots)
            .await
        {
            tracing::warn!(
                account_id,
                error = %err,
                "failed to persist fetched ChatGPT account-pool rate limits"
            );
        }
    }

    pub async fn record_account_pool_rate_limit_snapshot_for(
        &self,
        account_id: &str,
        snapshot: &RateLimitSnapshot,
    ) {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return;
        };
        if let Err(err) = account_pool
            .record_rate_limit_snapshot(account_id, snapshot)
            .await
        {
            tracing::warn!(
                account_id,
                error = %err,
                "failed to persist observed ChatGPT account-pool rate limits"
            );
        }
    }

    pub async fn clear_account_pool_rate_limit_cooldown_for(
        &self,
        account_id: &str,
    ) -> Result<(), ChatgptAccountPoolError> {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return Ok(());
        };
        account_pool.clear_rate_limit_cooldown(account_id).await
    }

    pub async fn prepare_chatgpt_account_pool_for_turn(
        &self,
    ) -> Result<(), ChatgptAccountPoolError> {
        if !should_consider_account_pool_auth(self.auth_cached().as_ref()) {
            return Ok(());
        }
        let Some(account_pool) = self.account_pool.as_ref() else {
            return Ok(());
        };
        // Always read the freshest token directly from the DB at the start of every
        // turn — no timestamp comparison, no in-memory cache. This ensures the CLI
        // holds whatever token codex-accounts last wrote before resolving turn
        // selection and before the first API request of the turn.
        if let Some(account_id) = self.auth_cached().and_then(|a| a.get_pool_account_id())
            && let Err(err) = self.reload_active_auth_from_pool_copy(&account_id).await
        {
            tracing::warn!(
                %account_id,
                %err,
                "pool auth DB reload at turn start failed; proceeding with cached auth"
            );
        }
        let current_auth = self.auth_cached();
        let current_account_id = current_auth
            .as_ref()
            .and_then(CodexAuth::get_pool_account_id);
        let current_refresh_failed_permanently = current_auth
            .as_ref()
            .and_then(|auth| self.refresh_failure_for_auth(auth))
            .is_some();
        match account_pool
            .resolve_turn_selection(
                current_account_id.as_deref(),
                current_refresh_failed_permanently,
            )
            .await?
        {
            ChatgptAccountPoolSelectionOutcome::Unchanged => Ok(()),
            ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts => {
                if current_auth
                    .as_ref()
                    .is_some_and(|auth| matches!(auth, CodexAuth::Chatgpt(_)))
                    && current_account_id.is_none()
                {
                    tracing::warn!(
                        "no eligible pool account is available; clearing only in-memory auth and preserving stored credentials"
                    );
                    self.set_cached_auth(None);
                }
                Err(ChatgptAccountPoolError::NoEligibleAccounts)
            }
            ChatgptAccountPoolSelectionOutcome::Activated { auth, .. } => {
                self.activate_pool_managed_auth(auth).await?;
                Ok(())
            }
        }
    }

    pub async fn register_current_chatgpt_account(&self) -> Result<(), ChatgptAccountPoolError> {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return Ok(());
        };
        let Some(auth) = self
            .auth_cached()
            .and_then(|auth| auth.get_current_auth_json())
        else {
            return Ok(());
        };
        if auth.resolved_mode()? != AuthMode::Chatgpt {
            return Ok(());
        }
        account_pool.register_account(&auth).await?;
        Ok(())
    }

    pub async fn handle_chatgpt_account_pool_usage_limit(
        &self,
        // The pool account ID of the account that produced the error, captured
        // before the failing request so a concurrent probe-driven auth swap cannot
        // cause the newly-activated (innocent) account to be marked as exhausted.
        // Falls back to auth_cached() when None.
        failing_account_id: Option<&str>,
        safe_to_retry: bool,
        snapshot: Option<&RateLimitSnapshot>,
        resets_at: Option<DateTime<Utc>>,
    ) -> Result<bool, ChatgptAccountPoolError> {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return Ok(false);
        };
        let current_auth = self.auth_cached();
        let Some(current_auth) = current_auth.as_ref() else {
            return Ok(false);
        };
        if !matches!(current_auth, CodexAuth::Chatgpt(_)) {
            return Ok(false);
        }
        let account_id = if let Some(id) = failing_account_id {
            id.to_string()
        } else {
            let Some(id) = current_auth.get_pool_account_id() else {
                return Ok(false);
            };
            id
        };
        let snapshot_contributes_cooldown =
            snapshot.is_none_or(snapshot_contributes_account_cooldown);
        let should_resolve_selection = if snapshot_contributes_cooldown {
            account_pool
                .mark_current_account_rate_limited(&account_id, snapshot, resets_at)
                .await?;
            true
        } else {
            let should_resolve_selection = match account_pool.list_rate_limits().await {
                Ok(entries) => entries
                    .into_iter()
                    .find(|entry| entry.account_id == account_id)
                    .is_some_and(|entry| {
                        entry
                            .rate_limits
                            .values()
                            .any(snapshot_indicates_account_cooldown)
                    }),
                Err(err) => {
                    tracing::warn!(
                        account_id,
                        error = %err,
                        "failed to read cached ChatGPT account-pool usage after non-codex usage limit"
                    );
                    false
                }
            };
            // Persist the observed non-codex limit after reading cached service
            // data. The CLI does not probe `/usage` to refresh rate limits;
            // codex-accounts owns polling.
            account_pool
                .mark_current_account_rate_limited(&account_id, snapshot, resets_at)
                .await?;
            should_resolve_selection
        };
        if !should_resolve_selection {
            return Ok(false);
        }
        // Resolve and activate the fallback even when the current turn is not
        // safe to retry, so the next turn starts on the fresh account.
        match account_pool
            .resolve_turn_selection(
                Some(account_id.as_str()),
                /*current_refresh_failed_permanently*/ false,
            )
            .await?
        {
            ChatgptAccountPoolSelectionOutcome::Activated { auth, failover, .. } if failover => {
                self.activate_pool_managed_auth(auth).await?;
                Ok(safe_to_retry)
            }
            _ => Ok(false),
        }
    }

    /// Loads cached rate limits for the active pool account after a turn completes.
    ///
    /// codex-accounts owns `/usage` polling. The CLI only consumes its latest
    /// persisted snapshots and never performs a network refresh here. If the cache
    /// shows the current account exhausted, it is marked for failover at the next
    /// turn boundary.
    ///
    /// Returns snapshots for the account that remains active after the read.
    /// When no pool, ChatGPT auth, or cached snapshot is available, the result is
    /// empty and errors are logged rather than propagated.
    pub async fn load_cached_rate_limits_post_turn(
        &self,
        serving_account_id: &str,
    ) -> AccountPoolPostTurnRateLimits {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return AccountPoolPostTurnRateLimits::default();
        };
        let Some(current_auth) = self.auth_cached() else {
            return AccountPoolPostTurnRateLimits::default();
        };
        if !matches!(current_auth, CodexAuth::Chatgpt(_)) {
            return AccountPoolPostTurnRateLimits::default();
        }
        let Some(account_id) = current_auth.get_pool_account_id() else {
            return AccountPoolPostTurnRateLimits::default();
        };
        if account_id != serving_account_id {
            return AccountPoolPostTurnRateLimits::default();
        }

        let entry = match account_pool.list_rate_limits().await {
            Ok(entries) => entries
                .into_iter()
                .find(|entry| entry.account_id == account_id),
            Err(err) => {
                tracing::debug!("post-turn cached rate-limit read failed: {err}");
                return AccountPoolPostTurnRateLimits::default();
            }
        };
        let Some(entry) = entry else {
            return AccountPoolPostTurnRateLimits::default();
        };

        // A detached read may finish after another turn has already switched
        // accounts. Its snapshots and failover decision belong to the account
        // that served the completed turn, not whichever account is active now.
        if self
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id())
            .as_deref()
            != Some(serving_account_id)
        {
            return AccountPoolPostTurnRateLimits::default();
        }

        let snapshots: Vec<RateLimitSnapshot> = entry.rate_limits.into_values().collect();

        // Check whether the codex-scoped account is exhausted.
        // codex-accounts treats an in-use account as "overdrive" when its primary
        // (5h) OR secondary (weekly) window is at 100% (isAccountOverdrive), and the
        // pool's own cooldown logic (exhausted_reset_at) likewise considers both
        // windows. The shared helper also catches authoritative reached-type
        // snapshots that may omit window details.
        let codex_snapshot = snapshots.iter().find(|s| {
            s.limit_id
                .as_deref()
                .is_none_or(|id| id.eq_ignore_ascii_case("codex"))
        });
        let is_exhausted = codex_snapshot.is_some_and(snapshot_indicates_account_cooldown);

        if is_exhausted
            && let Err(err) = account_pool
                .mark_current_account_rate_limited(&account_id, codex_snapshot, None)
                .await
        {
            tracing::warn!("post-turn mark rate limited failed: {err}");
        }

        AccountPoolPostTurnRateLimits { snapshots }
    }

    pub async fn handle_chatgpt_account_pool_auth_failure(
        &self,
        // The pool account ID of the account that produced the error, captured
        // before the failing request so a concurrent probe-driven auth swap cannot
        // cause the newly-activated (innocent) account to be marked as failed.
        // Falls back to auth_cached() when None.
        failing_account_id: Option<&str>,
        safe_to_retry: bool,
        error: &RefreshTokenFailedError,
    ) -> Result<bool, ChatgptAccountPoolError> {
        let Some(account_pool) = self.account_pool.as_ref() else {
            return Ok(false);
        };
        let current_auth = self.auth_cached();
        let Some(current_auth) = current_auth.as_ref() else {
            return Ok(false);
        };
        if !matches!(current_auth, CodexAuth::Chatgpt(_)) {
            return Ok(false);
        }
        let account_id = if let Some(id) = failing_account_id {
            id.to_string()
        } else {
            let Some(id) = current_auth.get_pool_account_id() else {
                return Ok(false);
            };
            id
        };
        if error.reason == RefreshTokenFailedReason::Other {
            let auth_status = account_pool.read_account_auth_status(&account_id).await?;
            if !matches!(
                auth_status,
                Some(
                    ChatgptAccountPoolAuthStatus::Invalid
                        | ChatgptAccountPoolAuthStatus::RefreshFailedPermanent
                )
            ) {
                account_pool
                    .mark_account_auth_retryable(&account_id, &error.to_string())
                    .await?;
            }
        } else {
            account_pool
                .mark_account_auth_failed(&account_id, &error.to_string())
                .await?;
            if current_auth.get_pool_account_id().as_deref() == Some(account_id.as_str()) {
                self.record_permanent_refresh_failure_if_unchanged(current_auth, error);
            }
        }
        // Resolve and activate the fallback even when the current turn is not
        // safe to retry, so the next turn starts on the fresh account.
        match account_pool
            .resolve_turn_selection(
                Some(account_id.as_str()),
                /*current_refresh_failed_permanently*/ false,
            )
            .await?
        {
            ChatgptAccountPoolSelectionOutcome::Activated { auth, failover, .. } if failover => {
                self.activate_pool_managed_auth(auth).await?;
                Ok(safe_to_retry)
            }
            _ => Ok(false),
        }
    }

    pub fn unauthorized_recovery(self: &Arc<Self>) -> UnauthorizedRecovery {
        UnauthorizedRecovery::new(Arc::clone(self))
    }

    fn external_auth(&self) -> Option<Arc<dyn ExternalAuth>> {
        self.external_auth
            .read()
            .ok()
            .and_then(|external_auth| external_auth.as_ref().map(Arc::clone))
    }

    async fn resolve_external_auth(
        &self,
        external_auth: &Arc<dyn ExternalAuth>,
    ) -> Result<CodexAuth, RefreshTokenError> {
        let auth = external_auth
            .resolve()
            .await
            .map_err(RefreshTokenError::Transient)?;
        self.validate_external_auth(&auth)?;
        Ok(auth)
    }

    /// Attempt to refresh the token by first performing a guarded reload from
    /// the active auth source. If the loaded token differs from the cached token,
    /// we can assume that the source already refreshed it. Otherwise, ask the
    /// token authority to refresh.
    pub async fn refresh_token(&self) -> Result<(), RefreshTokenError> {
        if let Some(auth) = self.auth_cached() {
            // For pool-managed accounts: check the DB timestamp before deciding
            // whether to refresh. If codex-accounts has already written a fresher
            // token to the pool DB since we last loaded it, reload from the pool
            // copy and skip the OAuth call. This ensures the in-memory cache is
            // always synced with what codex-accounts wrote before we attempt an
            // unnecessary OAuth round-trip.
            self.maybe_reload_pool_managed_auth_from_ack(&auth).await?;
        }
        let auth_before_reload = self.auth_cached();
        if auth_before_reload
            .as_ref()
            .is_some_and(|auth| !auth.is_chatgpt_auth())
        {
            return Ok(());
        }
        if let Some(auth) = auth_before_reload.as_ref()
            && self
                .pool_managed_chatgpt_refresh_context(auth)
                .await?
                .is_some()
        {
            // Pool-managed refresh only polls the DB copy written by codex-accounts;
            // it never performs an OAuth refresh itself, so holding the local single-
            // flight semaphore across the poll loop only blocks unrelated callers.
            return self
                .refresh_token_from_authority_impl(
                    /*forced=*/ false, /*stale_access_token=*/ None,
                )
                .await;
        }
        let _refresh_guard = self.refresh_lock.acquire().await.map_err(|_| {
            RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                RefreshTokenFailedReason::Other,
                REFRESH_TOKEN_UNKNOWN_MESSAGE.to_string(),
            ))
        })?;
        let expected_account_id = auth_before_reload
            .as_ref()
            .and_then(|auth| auth.get_pool_account_id().or_else(|| auth.get_account_id()));

        match self
            .reload_if_account_id_matches(expected_account_id.as_deref())
            .await
        {
            ReloadOutcome::ReloadedChanged => {
                tracing::info!("Skipping token refresh because auth changed after guarded reload.");
                Ok(())
            }
            ReloadOutcome::ReloadedNoChange => {
                self.refresh_token_from_authority_impl(
                    /*forced=*/ false, /*stale_access_token=*/ None,
                )
                .await
            }
            ReloadOutcome::Skipped => {
                Err(RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                    RefreshTokenFailedReason::Other,
                    REFRESH_TOKEN_ACCOUNT_MISMATCH_MESSAGE.to_string(),
                )))
            }
        }
    }

    /// Attempt to refresh the current auth token from the authority that issued
    /// it and update the shared cache. If the token refresh fails, returns the
    /// error to the caller.
    pub async fn refresh_token_from_authority(&self) -> Result<(), RefreshTokenError> {
        self.refresh_token_from_authority_with_forced(
            /*forced=*/ false, /*stale_access_token=*/ None,
        )
        .await
    }

    /// Like [`refresh_token_from_authority`] but bypasses the "token looks
    /// valid by expiry" short-circuit inside the pool-managed refresh path.
    /// Call this when responding to a server-side 401 — the server has already
    /// told us the token is invalid, so expiry-based skipping is wrong.
    pub(crate) async fn refresh_token_from_authority_forced(
        &self,
        stale_access_token: Option<&str>,
    ) -> Result<(), RefreshTokenError> {
        self.refresh_token_from_authority_with_forced(/*forced=*/ true, stale_access_token)
            .await
    }

    async fn refresh_token_from_authority_with_forced(
        &self,
        forced: bool,
        stale_access_token: Option<&str>,
    ) -> Result<(), RefreshTokenError> {
        if !forced
            && let Some(auth) = self.auth_cached()
            && self
                .pool_managed_chatgpt_refresh_context(&auth)
                .await?
                .is_some()
        {
            // Pool-managed non-forced refreshes only wait for the DB copy to change.
            // They do not benefit from process-local single-flight and should not hold
            // the semaphore while polling.
            return self
                .refresh_token_from_authority_impl(forced, stale_access_token)
                .await;
        }
        let _refresh_guard = self.refresh_lock.acquire().await.map_err(|_| {
            RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                RefreshTokenFailedReason::Other,
                REFRESH_TOKEN_UNKNOWN_MESSAGE.to_string(),
            ))
        })?;
        self.refresh_token_from_authority_impl(forced, stale_access_token)
            .await
    }

    async fn refresh_token_from_authority_impl(
        &self,
        forced: bool,
        stale_access_token: Option<&str>,
    ) -> Result<(), RefreshTokenError> {
        tracing::info!("Refreshing token");

        let auth = match self.auth_cached() {
            Some(auth) => {
                self.maybe_reload_pool_managed_auth_from_ack(&auth).await?;
                self.auth_cached().unwrap_or(auth)
            }
            None => return Ok(()),
        };
        if let Some(error) = self.refresh_failure_for_auth(&auth) {
            return Err(RefreshTokenError::Permanent(error));
        }

        let attempted_auth = auth.clone();
        let result = if self.has_external_auth() {
            self.refresh_external_auth(ExternalAuthRefreshReason::Unauthorized)
                .await
        } else if let Some((account_pool, chatgpt_auth, account_id)) =
            self.pool_managed_chatgpt_refresh_context(&auth).await?
        {
            self.refresh_pool_managed_chatgpt_token(
                &account_pool,
                &chatgpt_auth,
                &account_id,
                forced,
                stale_access_token,
            )
            .await
        } else if self.account_pool.is_some() && matches!(auth, CodexAuth::Chatgpt(_)) {
            // Pool-only ChatGPT: never call OAuth from the CLI. Missing pool
            // registration is a setup/migration problem for codex-accounts.
            Err(RefreshTokenError::Transient(std::io::Error::other(
                "ChatGPT auth is not registered in the account pool",
            )))
        } else {
            match auth {
                CodexAuth::Chatgpt(chatgpt_auth) => {
                    let token_data = chatgpt_auth.current_token_data().ok_or_else(|| {
                        RefreshTokenError::Transient(std::io::Error::other(
                            "Token data is not available.",
                        ))
                    })?;
                    self.refresh_and_persist_chatgpt_token(&chatgpt_auth, token_data.refresh_token)
                        .await
                }
                CodexAuth::Headers(_) | CodexAuth::AgentIdentity(_) => Ok(()),
            }
        };
        if let Err(RefreshTokenError::Permanent(error)) = &result {
            self.record_permanent_refresh_failure_if_unchanged(&attempted_auth, error);
        }
        result
    }

    // Pool-managed ChatGPT token refresh coordination.
    async fn maybe_reload_pool_managed_auth_from_ack(
        &self,
        auth: &CodexAuth,
    ) -> Result<bool, RefreshTokenError> {
        let Some((account_pool, _, account_id)) =
            self.pool_managed_chatgpt_refresh_context(auth).await?
        else {
            return Ok(false);
        };
        let last_loaded_refresh_at = auth
            .get_current_auth_json()
            .and_then(|auth_dot_json| auth_dot_json.last_refresh.map(|value| value.timestamp()));
        let last_persisted_refresh_at = account_pool
            .account_last_auth_refresh_at(&account_id)
            .await
            .map_err(|err| RefreshTokenError::Transient(std::io::Error::other(err)))?;
        // Skip the reload only when we can positively confirm that the DB copy is
        // older than what is already cached: both timestamps must be present AND the
        // DB timestamp must be strictly less than the loaded one (strict so that equal
        // wall-clock seconds — which can occur when codex-accounts writes a new token
        // within the same second as the CLI's cached last_refresh — still trigger a
        // reload rather than silently keeping the stale refresh token in memory).
        //
        // When last_persisted_refresh_at is None (the account was never OAuth-refreshed
        // by codex-accounts), we cannot conclude the cached copy is current — always
        // reload.  Using Option's Ord would evaluate None < Some(_) == true and
        // incorrectly skip the reload for newly-registered accounts.
        let db_is_strictly_older = matches!(
            (last_persisted_refresh_at, last_loaded_refresh_at),
            (Some(db), Some(loaded)) if db < loaded
        );
        if db_is_strictly_older {
            return Ok(false);
        }
        self.reload_active_auth_from_pool_copy(&account_id).await
    }

    async fn pool_managed_chatgpt_refresh_context(
        &self,
        auth: &CodexAuth,
    ) -> Result<Option<(ChatgptAccountPool, ChatgptAuth, String)>, RefreshTokenError> {
        let Some(account_pool) = self.account_pool.clone() else {
            return Ok(None);
        };
        let CodexAuth::Chatgpt(chatgpt_auth) = auth else {
            return Ok(None);
        };
        let Some(account_id) = auth.get_pool_account_id() else {
            return Ok(None);
        };
        match account_pool.account_last_auth_refresh_at(&account_id).await {
            Ok(_) => Ok(Some((account_pool, chatgpt_auth.clone(), account_id))),
            Err(ChatgptAccountPoolError::AccountNotFound(_)) => Ok(None),
            Err(err) => Err(RefreshTokenError::Transient(std::io::Error::other(err))),
        }
    }

    async fn reload_active_auth_from_pool_copy(
        &self,
        account_id: &str,
    ) -> Result<bool, RefreshTokenError> {
        let pool_auth = match self.account_pool.as_ref() {
            Some(account_pool) => match account_pool
                .read_account_tokens(account_id)
                .await
                .map_err(|err| RefreshTokenError::Transient(std::io::Error::other(err)))?
            {
                Some(pool_auth) => pool_auth,
                None => {
                    self.failover_from_missing_pool_secret(account_pool, account_id)
                        .await?;
                    return Ok(true);
                }
            },
            None => {
                return Err(RefreshTokenError::Transient(std::io::Error::other(
                    format!("pool auth secret missing for account {account_id}"),
                )));
            }
        };
        let loaded_account_id = pool_auth.pool_account_id.as_deref();
        if loaded_account_id != Some(account_id) {
            return Err(RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                RefreshTokenFailedReason::Other,
                REFRESH_TOKEN_ACCOUNT_MISMATCH_MESSAGE.to_string(),
            )));
        }
        self.activate_pool_managed_auth(pool_auth)
            .await
            .map_err(RefreshTokenError::Transient)
    }

    async fn failover_from_missing_pool_secret(
        &self,
        account_pool: &ChatgptAccountPool,
        account_id: &str,
    ) -> Result<(), RefreshTokenError> {
        account_pool
            .mark_account_missing_secret(account_id)
            .await
            .map_err(|err| RefreshTokenError::Transient(std::io::Error::other(err)))?;
        match account_pool
            .resolve_turn_selection(
                Some(account_id),
                /*current_refresh_failed_permanently*/ false,
            )
            .await
            .map_err(|err| RefreshTokenError::Transient(std::io::Error::other(err)))?
        {
            ChatgptAccountPoolSelectionOutcome::Activated { auth, failover, .. } if failover => {
                let _ = self
                    .activate_pool_managed_auth(auth)
                    .await
                    .map_err(RefreshTokenError::Transient)?;
                Ok(())
            }
            _ => Err(RefreshTokenError::Transient(std::io::Error::other(
                format!("pool auth secret missing for account {account_id}",),
            ))),
        }
    }

    async fn refresh_pool_managed_chatgpt_token(
        &self,
        account_pool: &ChatgptAccountPool,
        chatgpt_auth: &ChatgptAuth,
        account_id: &str,
        // When true, we are responding to a server 401: the current token is
        // known-bad even if its expiry still looks fresh, so we must wait for a
        // *different* token to appear rather than trusting the cached expiry.
        forced: bool,
        stale_access_token: Option<&str>,
    ) -> Result<(), RefreshTokenError> {
        // codex-accounts is the SOLE token-refresh authority. The CLI never calls the
        // OAuth refresh endpoint for pool-managed accounts and never takes the refresh
        // lock — it only reloads the freshest token that codex-accounts has written to
        // the account-pool database. Making codex-accounts the single writer of
        // the rotating refresh token eliminates the dual-writer race that previously
        // forked the token chain and produced refresh_token_reused (permanent death).
        //
        let fallback_stale_access_token = chatgpt_auth
            .current_token_data()
            .map(|tokens| tokens.access_token);
        let stale_access_token = stale_access_token.or(fallback_stale_access_token.as_deref());

        // Reload once up front — codex-accounts may already have written a fresh token.
        self.reload_active_auth_from_pool_copy(account_id).await?;
        if self.pool_managed_token_ready(account_id, forced, stale_access_token)? {
            return Ok(());
        }

        // On the forced (401) path the server has explicitly rejected the current access
        // token. Rather than waiting the full lock TTL for a replacement, decide
        // immediately based on what codex-accounts wrote to auth_status:
        //
        //   - Terminal (invalid / refresh_failed_permanent): codex-accounts already
        //     confirmed the refresh token is dead (refresh_token_reused,
        //     refresh_token_invalidated, etc.). The CLI mirrors that verdict: mark
        //     invalid in the DB with a detailed event and return Permanent so the
        //     turn fails over without retrying this account.
        //
        //   - Not terminal (valid / pending / etc.): the 401 may be a normal expiry
        //     that codex-accounts is mid-refresh for. Return Transient immediately so
        //     the turn fails over to another account quickly; the account is NOT marked
        //     invalid so it can be selected again once the token is fresh.
        if forced {
            let auth_status = account_pool
                .read_account_auth_status(account_id)
                .await
                .unwrap_or(None);
            let is_terminal = matches!(
                auth_status,
                Some(
                    ChatgptAccountPoolAuthStatus::Invalid
                        | ChatgptAccountPoolAuthStatus::RefreshFailedPermanent
                )
            );
            if is_terminal {
                let stale_fingerprint = pool_token_fingerprint(stale_access_token);
                let reason = format!(
                    "cli-initiated invalidation after server 401: \
                     codex-accounts confirmed auth_status={auth_status:?} (terminal) \
                     and no replacement token was found after one DB reload; \
                     marking account invalid for immediate permanent turn failover \
                     (account_id={account_id} stale_access_token_fingerprint={stale_fingerprint})"
                );
                if let Err(err) = account_pool
                    .mark_account_auth_failed(account_id, &reason)
                    .await
                {
                    tracing::warn!(
                        %account_id,
                        %err,
                        "failed to record cli-initiated invalidation event in pool DB; \
                         failing over anyway"
                    );
                }
                return Err(RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                    RefreshTokenFailedReason::Other,
                    reason,
                )));
            }
            // Not terminal — fail over fast without marking the account dead.
            return Err(RefreshTokenError::Transient(std::io::Error::other(
                format!(
                    "pool-managed token for account {account_id} was rejected with 401 \
                 but auth_status is not yet terminal (auth_status={auth_status:?}); \
                 failing over immediately without invalidation"
                ),
            )));
        }

        // Not forced: honor a terminal auth_status from codex-accounts immediately
        // instead of waiting the full lock TTL for a refresh that will never come.
        let auth_status = account_pool
            .read_account_auth_status(account_id)
            .await
            .unwrap_or(None);
        if matches!(
            auth_status,
            Some(
                ChatgptAccountPoolAuthStatus::Invalid
                    | ChatgptAccountPoolAuthStatus::RefreshFailedPermanent
            )
        ) {
            let reason = format!(
                "pool-managed token for account {account_id} has terminal \
                 auth_status={auth_status:?}; treating refresh as permanently failed"
            );
            return Err(RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                RefreshTokenFailedReason::Other,
                reason,
            )));
        }

        // Not forced: token expired by time. Wait briefly for codex-accounts (which polls
        // with its own backoff/retry) to refresh it, reloading the pool copy each
        // iteration. If it never becomes ready within the lock TTL, return a transient
        // error so the turn fails over to another pool account and leaves this one for
        // codex-accounts to repair.
        const POLL_INTERVAL_MS: u64 = 500;
        let max_wait_ms: u64 = ChatgptAccountPool::token_refresh_lock_ttl().as_millis() as u64;
        let mut waited_ms: u64 = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            waited_ms += POLL_INTERVAL_MS;
            self.reload_active_auth_from_pool_copy(account_id).await?;
            if self.pool_managed_token_ready(account_id, forced, stale_access_token)? {
                return Ok(());
            }
            if waited_ms >= max_wait_ms {
                return Err(RefreshTokenError::Transient(std::io::Error::other(
                    format!(
                        "pool-managed token for account {account_id} was not refreshed by \
                     codex-accounts within {max_wait_ms}ms"
                    ),
                )));
            }
        }
    }

    /// Whether the freshly reloaded in-memory auth is usable for `account_id`.
    /// - If a failover switched the active account, we now hold a different usable
    ///   account — treat that as done.
    /// - Not forced: ready when the access token no longer needs refresh (expiry).
    /// - Forced (responding to a 401): the previous token was rejected even though its
    ///   expiry may look fine, so require that codex-accounts has rotated to a
    ///   different access token before trusting it.
    fn pool_managed_token_ready(
        &self,
        account_id: &str,
        forced: bool,
        stale_access_token: Option<&str>,
    ) -> Result<bool, RefreshTokenError> {
        let Some(auth) = self.auth_cached() else {
            return Ok(false);
        };
        if auth.get_pool_account_id().as_deref() != Some(account_id) {
            return Ok(true);
        }
        let Some(auth_dot_json) = auth.get_current_auth_json() else {
            return Ok(false);
        };
        if forced {
            let current_access_token = auth_dot_json
                .tokens
                .as_ref()
                .map(|tokens| tokens.access_token.as_str());
            return Ok(current_access_token.is_some() && current_access_token != stale_access_token);
        }
        Ok(!ChatgptAccountPool::account_auth_needs_token_refresh(
            &auth_dot_json,
            Utc::now(),
        ))
    }

    /// Log out by deleting the on‑disk auth.json (if present). Returns Ok(true)
    /// if a file was removed, Ok(false) if no auth file existed. On success,
    /// reloads the in‑memory auth cache so callers immediately observe the
    /// unauthenticated state.
    pub async fn logout(&self) -> std::io::Result<bool> {
        let pool_managed_auth = match self.auth_cached() {
            Some(auth) => self
                .pool_managed_chatgpt_refresh_context(&auth)
                .await
                .ok()
                .flatten()
                .is_some(),
            None => false,
        };
        let removed = logout_all_stores(
            &self.codex_home,
            self.auth_credentials_store_mode,
            self.keyring_backend_kind,
        )?;
        self.clear_external_auth();
        if pool_managed_auth {
            self.set_cached_auth(None);
        } else {
            self.reload().await;
        }
        Ok(removed)
    }

    pub async fn logout_with_revoke(&self) -> std::io::Result<bool> {
        let auth_dot_json = self
            .auth_cached()
            .and_then(|auth| auth.get_current_auth_json());
        let pool_managed_auth = match self.auth_cached() {
            Some(auth) => self
                .pool_managed_chatgpt_refresh_context(&auth)
                .await
                .ok()
                .flatten()
                .is_some(),
            None => false,
        };
        if let Err(err) =
            revoke_auth_tokens(auth_dot_json.as_ref(), self.auth_route_config.as_ref()).await
        {
            tracing::warn!("failed to revoke auth tokens during logout: {err}");
        }
        if pool_managed_auth && let Some(account_pool) = self.account_pool.as_ref() {
            account_pool
                .disable_all_accounts_for_logout()
                .await
                .map_err(std::io::Error::other)?;
        }
        let result = logout_all_stores(
            &self.codex_home,
            self.auth_credentials_store_mode,
            self.keyring_backend_kind,
        )?;
        self.clear_external_auth();
        if pool_managed_auth {
            self.set_cached_auth(None);
        } else {
            self.reload().await;
        }
        Ok(result)
    }

    /// Returns the precise kind of credentials backing the current authentication.
    pub fn get_api_auth_mode(&self) -> Option<AuthMode> {
        self.auth_cached().as_ref().map(CodexAuth::api_auth_mode)
    }

    /// Returns the effective backend auth mode for the current authentication.
    pub fn auth_mode(&self) -> Option<AuthMode> {
        self.auth_cached().as_ref().map(CodexAuth::auth_mode)
    }

    pub fn current_auth_uses_codex_backend(&self) -> bool {
        self.get_api_auth_mode()
            .is_some_and(AuthMode::uses_codex_backend)
    }

    async fn refresh_external_auth(
        &self,
        reason: ExternalAuthRefreshReason,
    ) -> Result<(), RefreshTokenError> {
        let Some(external_auth) = self.external_auth() else {
            return Err(RefreshTokenError::Transient(std::io::Error::other(
                "external auth is not configured",
            )));
        };
        let previous_account_id = self
            .auth_cached()
            .as_ref()
            .and_then(CodexAuth::get_account_id);
        let context = ExternalAuthRefreshContext {
            reason,
            previous_account_id,
        };

        let refreshed = external_auth
            .refresh(context)
            .await
            .map_err(RefreshTokenError::Transient)?;
        self.validate_external_auth(&refreshed)?;
        self.commit_external_auth(refreshed)?;
        Ok(())
    }

    fn commit_external_auth(&self, auth: CodexAuth) -> Result<(), RefreshTokenError> {
        // External bearer auth (provider command / Headers) stays in-memory only.
        self.set_cached_auth(Some(auth));
        Ok(())
    }

    fn validate_external_auth(&self, auth: &CodexAuth) -> Result<(), RefreshTokenError> {
        if let Some(account_id) = auth.get_account_id()
            && let Some(expected_workspace_ids) = self.forced_chatgpt_workspace_id()
            && !expected_workspace_ids.contains(&account_id)
        {
            return Err(RefreshTokenError::Transient(std::io::Error::other(
                format!(
                    "external auth returned workspace {account_id:?}, expected one of {expected_workspace_ids:?}"
                ),
            )));
        }
        Ok(())
    }

    // Refreshes ChatGPT OAuth tokens, persists the updated auth state, and
    // reloads the in-memory cache so callers immediately observe new tokens.
    async fn refresh_and_persist_chatgpt_token(
        &self,
        auth: &ChatgptAuth,
        refresh_token: String,
    ) -> Result<(), RefreshTokenError> {
        let refresh_response = request_chatgpt_token_refresh(refresh_token, auth.client()).await?;
        let access_token = refresh_response
            .access_token
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                RefreshTokenError::Transient(std::io::Error::other(
                    "token refresh response omitted access_token; existing credentials were preserved",
                ))
            })?;
        let refreshed_refresh_token = refresh_response
            .refresh_token
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                RefreshTokenError::Transient(std::io::Error::other(
                    "token refresh response omitted refresh_token; refusing to persist a potentially spent credential",
                ))
            })?;
        let current_auth = match auth.storage().load() {
            Ok(Some(stored)) => stored,
            Ok(None) => auth.current_auth_json().ok_or_else(|| {
                RefreshTokenError::Transient(std::io::Error::other("Token data is not available."))
            })?,
            Err(err) => {
                tracing::warn!(
                    %err,
                    "failed to reload auth storage after token rotation; using the in-memory copy"
                );
                auth.current_auth_json()
                    .ok_or_else(|| RefreshTokenError::Transient(std::io::Error::other(err)))?
            }
        };
        let refreshed_auth = match apply_refreshed_tokens(
            current_auth.clone(),
            refresh_response.id_token,
            Some(access_token.clone()),
            Some(refreshed_refresh_token.clone()),
        ) {
            Ok(refreshed_auth) => refreshed_auth,
            Err(err) => {
                tracing::warn!(
                    %err,
                    "refreshed id_token was unusable; retaining the prior id_token while preserving the rotated access and refresh tokens"
                );
                apply_refreshed_tokens(
                    current_auth,
                    /*id_token*/ None,
                    Some(access_token),
                    Some(refreshed_refresh_token),
                )
                .map_err(RefreshTokenError::from)?
            }
        };

        let mut last_save_error = None;
        for attempt in 1..=3 {
            match auth.storage().save(&refreshed_auth) {
                Ok(()) => {
                    self.reload().await;
                    return Ok(());
                }
                Err(err) => {
                    last_save_error = Some(err);
                    if attempt < 3 {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }

        // Keep the rotated credential alive in this process even when persistent
        // storage is temporarily unavailable. This prevents another automatic
        // refresh from reusing the now-spent previous refresh token.
        let mut state = auth
            .state
            .auth_dot_json
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock refreshed auth state"))?;
        *state = Some(refreshed_auth);
        drop(state);
        self.auth_change_tx.send_modify(|revision| *revision += 1);
        let err = last_save_error
            .unwrap_or_else(|| std::io::Error::other("unknown auth storage failure"));
        Err(RefreshTokenError::Transient(std::io::Error::new(
            err.kind(),
            format!(
                "token rotated successfully but persistent auth storage failed after retries; the refreshed credential is retained in memory: {err}"
            ),
        )))
    }
}

/// Returns a short one-way fingerprint for a pool token so event log messages
/// can be correlated without retaining any part of the credential.
fn pool_token_fingerprint(token: Option<&str>) -> String {
    let Some(token) = token else {
        return "none".to_string();
    };
    let digest = format!("{:x}", Sha256::digest(token.as_bytes()));
    digest[..12].to_string()
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
