//! The stored credential from a sign-in, and how a backend's requests carry it.
//!
//! # Per request, not per arm
//!
//! A key from `api_key_env`/`_file`/`_cmd` is resolved once, synchronously, when an arm
//! is built (`Backend::resolve_key`). A signed-in credential cannot work that way: a
//! ChatGPT access token lives one hour and a consult can run longer, and refreshing
//! is network I/O. So an `auth` backend's arm wraps its transport in [`GrantAuthHttp`],
//! which asks the [`TokenSource`] for a bearer token on **every** request, and on a 401
//! refreshes once and sends again. `Backend::resolve_key` refuses an `auth` backend
//! outright, so a call site that skipped this wrapper fails loudly instead of sending
//! a placeholder upstream.
//!
//! # Why refresh takes a lease
//!
//! A ChatGPT refresh token **rotates**: each refresh returns a new one and spends the
//! old. Two refreshes with the same token — two consult tasks, or the MCP server and a
//! CLI run sharing the state db — would have the loser present a spent token, and a
//! provider that detects reuse revokes the whole grant. So refreshes are serialized:
//!
//! 1. In one process, a per-backend `tokio` mutex lets one task refresh while the
//!    others wait and then read its result.
//! 2. Across processes, a lease row in the state db: a short write claims it, the
//!    winner calls the token endpoint **with no db connection open** (the store's busy
//!    timeout is 5 s, and an HTTPS round trip held inside a write transaction would
//!    make every other store writer fail `Busy`), and a second short write stores the
//!    rotated pair guarded by the lease holder's id. A waiter polls the row until the
//!    token changes or the lease lapses ([`LEASE_SECS`], so a crashed winner frees it).
//!
//! # What a model can reach
//!
//! Nothing here takes model input. The endpoint a refresh dials is the one stored with
//! the grant, which only `kaibo login` writes; the backend name comes from config.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode};
use rig_core::http_client::{self, HttpClientExt, LazyBody};

use super::{form_body, post_form, AuthProfile, Flow, TokenEndpointError, TokenResponse};
use crate::store::SessionStore;

/// Refresh when the access token has fewer than this many seconds left, so a request
/// never leaves with a token that expires in flight.
pub const REFRESH_MARGIN_SECS: i64 = 300;

/// How long a refresh lease holds before another process may take it over.
pub const LEASE_SECS: i64 = 45;

// A refresh request must end before its lease can lapse, or a second process could
// take the lease over and present the same refresh token while the first is still in
// flight. The token request's own timeout bounds it.
const _: () = assert!((super::TOKEN_REQUEST_TIMEOUT.as_secs() as i64) < LEASE_SECS);

/// How many times a refresh tries to store the rotated pair before giving it up. A
/// write that fails `Busy` (another kaibo writing the store) usually succeeds on the
/// next try, and losing a rotated pair costs the operator a sign-in.
const STORE_ROTATED_ATTEMPTS: u32 = 2;
const STORE_ROTATED_PAUSE: Duration = Duration::from_millis(500);

// The whole refresh — token request plus every store attempt — must also end inside
// the lease, so a takeover never presents the refresh token this one just spent.
const _: () = assert!(
    (super::TOKEN_REQUEST_TIMEOUT.as_secs()
        + STORE_ROTATED_ATTEMPTS as u64 * (crate::store::BUSY_TIMEOUT.as_secs() + 1))
        < LEASE_SECS as u64
);

/// How long a waiter polls for another process's refresh before giving up.
const WAIT_FOR_REFRESH: Duration = Duration::from_secs(60);

// A waiter must outlast a lease, so a crashed winner's lapsed lease is taken over
// within one call rather than failing it.
const _: () = assert!(WAIT_FOR_REFRESH.as_secs() as i64 > LEASE_SECS);

/// Unix seconds now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One backend's stored sign-in.
#[derive(Clone, PartialEq, Eq)]
pub struct Grant {
    /// The backend it belongs to (`[backends.<name>]`).
    pub backend: String,
    /// The sign-in that produced it.
    pub auth: AuthProfile,
    /// The issued OAuth client id (OIDC flow), reused on refresh and re-sign-in.
    pub client_id: Option<String>,
    /// The token endpoint this grant refreshes against: the one it was minted at.
    pub token_url: String,
    /// The bearer credential: an access token, or a minted API key.
    pub access_token: String,
    /// The refresh token, for a refreshing grant.
    pub refresh_token: Option<String>,
    /// When `access_token` expires, Unix seconds; `None` for a key that does not.
    pub expires_at: Option<i64>,
    /// Who signed in (email or subject), for `kaibo login --status`. Never sent anywhere.
    pub account: Option<String>,
    /// The granted scopes, space-separated, as the provider returned them.
    pub scopes: Option<String>,
}

impl fmt::Debug for Grant {
    // Tokens stay out of every log and panic message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant")
            .field("backend", &self.backend)
            .field("auth", &self.auth)
            .field("client_id", &self.client_id)
            .field("token_url", &self.token_url)
            .field("expires_at", &self.expires_at)
            .field("account", &self.account)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .finish_non_exhaustive()
    }
}

impl Grant {
    /// Whether the access token needs a refresh before use: it is the one a provider
    /// just refused (`stale`), or it is inside [`REFRESH_MARGIN_SECS`] of expiring.
    fn needs_refresh(&self, stale: Option<&str>, now: i64) -> bool {
        stale == Some(self.access_token.as_str())
            || self
                .expires_at
                .is_some_and(|e| e - now < REFRESH_MARGIN_SECS)
    }
}

/// The refusal for a backend with no stored sign-in, or one the provider revoked.
pub fn not_signed_in(backend: &str, auth: AuthProfile) -> anyhow::Error {
    anyhow!(
        "backend {backend:?} uses `auth = \"{auth}\"` and has no {} sign-in. A person must \
         run `kaibo login {backend}` in a terminal on this machine and finish in the \
         browser; then call again. A model cannot sign in for them",
        auth.profile().title
    )
}

/// How to open the state db when a signed-in backend needs it and this process has
/// not opened it yet. Recorded by `Resolver::from_config`.
#[derive(Debug, Clone)]
pub struct StoreSpec {
    /// The state db path (`[persistence] path`).
    pub path: std::path::PathBuf,
    /// Session capacity, as the other open sites pass it.
    pub capacity: std::num::NonZeroUsize,
    /// The canonical allowed set, for the store's containment check.
    pub allowed: Vec<std::path::PathBuf>,
}

static STORE_SPEC: OnceLock<StoreSpec> = OnceLock::new();

/// Record how to open the state db for signed-in backends (first caller wins).
pub fn register_store_spec(spec: StoreSpec) {
    let _ = STORE_SPEC.set(spec);
}

/// This process's state db: the one it already opened, else opened now from the
/// registered [`StoreSpec`]. `None` when persistence is off.
async fn process_store() -> Result<Option<SessionStore>> {
    let Some(spec) = STORE_SPEC.get() else {
        // No spec recorded: whatever this process opened, if anything.
        return Ok(crate::store::process_store());
    };
    // Opened once per process from the spec. `SessionStore::open` shares an already
    // open handle for the same path, so this never builds a second `Database`, and
    // a process that opened some other db first still reads the configured one.
    static OPENED: tokio::sync::OnceCell<SessionStore> = tokio::sync::OnceCell::const_new();
    let store = OPENED
        .get_or_try_init(|| async {
            let allowed: Vec<&std::path::Path> = spec.allowed.iter().map(|p| p.as_path()).collect();
            SessionStore::open(&spec.path, spec.capacity, &allowed).await
        })
        .await?;
    Ok(Some(store.clone()))
}

/// Per-backend in-process refresh locks.
fn local_lock(backend: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(Default::default);
    let mut map = map.lock().unwrap_or_else(|p| p.into_inner());
    map.entry(backend.to_string()).or_default().clone()
}

/// Supplies a backend's bearer credential, refreshing it when due.
#[derive(Clone)]
pub struct TokenSource {
    backend: String,
    auth: AuthProfile,
    /// Set for an explicit store; `None` reads this process's store at first use.
    store: Option<SessionStore>,
    http: reqwest::Client,
}

impl fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSource")
            .field("backend", &self.backend)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl TokenSource {
    /// A source over an explicit store (tests, and `kaibo login --status`).
    pub fn new(
        backend: &str,
        auth: AuthProfile,
        store: SessionStore,
        http: reqwest::Client,
    ) -> Self {
        Self {
            backend: backend.to_string(),
            auth,
            store: Some(store),
            http,
        }
    }

    /// A source over this process's state db. Built at arm construction, which never
    /// fails for want of a store: the store is found (or opened) at the first request,
    /// and a process with persistence off refuses there, naming why.
    pub fn for_process(backend: &str, auth: AuthProfile) -> Result<Self> {
        Ok(Self {
            backend: backend.to_string(),
            auth,
            store: None,
            http: crate::tls::https_client(super::TOKEN_REQUEST_TIMEOUT)?,
        })
    }

    async fn store(&self) -> Result<SessionStore> {
        if let Some(store) = &self.store {
            return Ok(store.clone());
        }
        process_store().await?.ok_or_else(|| {
            anyhow!(
                "backend {:?} uses `auth = \"{}\"`, which keeps its sign-in in kaibo's state \
                 db, and this kaibo has no state db open — persistence is off \
                 (`--no-persistence`, `KAIBO_NO_PERSISTENCE`, or `[persistence] enabled = \
                 false`) or another kaibo holds it on a single-process platform. Turn \
                 persistence on, or use a backend with an API key",
                self.backend,
                self.auth
            )
        })
    }

    /// The credential to send now.
    pub async fn bearer(&self) -> Result<String> {
        self.fresh(None).await
    }

    /// The credential to send after the provider refused `stale`: a refreshed one, or
    /// the one another task already refreshed to.
    pub async fn after_refusal(&self, stale: &str) -> Result<String> {
        self.fresh(Some(stale)).await
    }

    async fn load(&self) -> Result<Grant> {
        self.store()
            .await?
            .get_grant(&self.backend)
            .await?
            // A grant from another sign-in (the backend's `auth` changed since) is not
            // this backend's credential: sending it would hand one provider's secret
            // to another.
            .filter(|g| g.auth == self.auth)
            .ok_or_else(|| not_signed_in(&self.backend, self.auth))
    }

    async fn fresh(&self, stale: Option<&str>) -> Result<String> {
        let grant = self.load().await?;
        if !grant.needs_refresh(stale, now()) {
            return Ok(grant.access_token);
        }
        if grant.refresh_token.is_none() {
            // A minted key or a grant with no refresh token: nothing to refresh to.
            return match stale {
                Some(_) => Err(anyhow!(
                    "the provider refused backend {:?}'s signed-in credential, and it cannot \
                     be refreshed. Run `kaibo logout {0}` and `kaibo login {0}` in a terminal",
                    self.backend
                )),
                None => Ok(grant.access_token),
            };
        }
        let lock = local_lock(&self.backend);
        let _held = lock.lock().await;
        let store = self.store().await?;
        let store = &store;
        let deadline = tokio::time::Instant::now() + WAIT_FOR_REFRESH;
        let holder = super::new_host_id();
        loop {
            // Re-read under the lock: the task or process before us may have refreshed.
            let grant = self.load().await?;
            if !grant.needs_refresh(stale, now()) {
                return Ok(grant.access_token);
            }
            if store
                .claim_refresh_lease(&self.backend, &holder, now(), LEASE_SECS)
                .await?
            {
                // Re-read while holding the lease: another process may have finished
                // its refresh between our read above and our claim, which would leave
                // `grant` carrying a refresh token that rotation already spent.
                let grant = self.load().await?;
                if !grant.needs_refresh(stale, now()) {
                    let _ = store.release_refresh_lease(&self.backend, &holder).await;
                    return Ok(grant.access_token);
                }
                return self.refresh_holding_lease(store, &holder, grant).await;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "another kaibo process has been refreshing backend {:?}'s sign-in for over \
                     {} seconds without finishing. Call again; if this repeats, run `kaibo \
                     login {0}` in a terminal",
                    self.backend,
                    WAIT_FOR_REFRESH.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn refresh_holding_lease(
        &self,
        store: &SessionStore,
        holder: &str,
        grant: Grant,
    ) -> Result<String> {
        match refresh(&self.http, &grant).await {
            Ok(fresh) => {
                let finished = match self.store_rotated(store, holder, &fresh).await {
                    Ok(finished) => finished,
                    Err(e) => {
                        // The provider has rotated the pair and the store still holds
                        // the spent refresh token. Releasing the lease would let the
                        // next caller present it, and a reused refresh token can get
                        // the whole grant revoked. Remove the grant instead, so the
                        // next call asks for a sign-in rather than replaying it.
                        let removed = store.delete_grant_held(&self.backend, holder).await;
                        tracing::warn!(
                            backend = %self.backend,
                            removed = ?removed.as_ref().ok(),
                            "a refreshed sign-in could not be stored"
                        );
                        return Err(anyhow::Error::from(e).context(format!(
                            "the provider refreshed backend {:?}'s sign-in, but kaibo could \
                             not store the new credential in its state db, so the stored one \
                             is spent. Run `kaibo login {0}` in a terminal to sign in again",
                            self.backend
                        )));
                    }
                };
                if !finished {
                    // Our lease lapsed and someone else took over mid-refresh; their write
                    // won. Use what is stored rather than a token the store does not hold.
                    tracing::warn!(backend = %self.backend, "sign-in refresh finished after its lease lapsed");
                    return Ok(self.load().await?.access_token);
                }
                tracing::info!(backend = %self.backend, "refreshed the signed-in credential");
                Ok(fresh.access_token)
            }
            Err(e) => {
                let dead = e
                    .downcast_ref::<TokenEndpointError>()
                    .is_some_and(TokenEndpointError::grant_is_dead);
                if dead {
                    // Revoked, expired, or the app was disconnected: the grant can never
                    // work again, so drop it and say how to sign in again. Only while we
                    // still hold the lease: a `kaibo login` that finished during this
                    // refresh cleared it, and its new grant is the one to use.
                    if !store.delete_grant_held(&self.backend, holder).await? {
                        if let Ok(current) = self.load().await {
                            if !current.needs_refresh(None, now()) {
                                return Ok(current.access_token);
                            }
                        }
                    }
                    return Err(anyhow!(
                        "the provider no longer accepts backend {:?}'s sign-in ({e}) — it was \
                         revoked, expired after 30 days unused, or the app was disconnected. \
                         kaibo removed it. Run `kaibo login {0}` in a terminal to sign in again",
                        self.backend
                    ));
                }
                let _ = store.release_refresh_lease(&self.backend, holder).await;
                Err(e.context(format!("refreshing backend {:?}'s sign-in", self.backend)))
            }
        }
    }
}

impl TokenSource {
    /// Store a rotated pair, trying again once if the write fails (`Busy` from another
    /// writer is the usual cause). Ok(false) means the lease lapsed and another holder
    /// took over, the same as [`SessionStore::finish_refresh`].
    async fn store_rotated(
        &self,
        store: &SessionStore,
        holder: &str,
        fresh: &Grant,
    ) -> crate::store::Result<bool> {
        let mut attempt = 1;
        loop {
            match store.finish_refresh(holder, fresh).await {
                Ok(finished) => return Ok(finished),
                Err(e) if attempt >= STORE_ROTATED_ATTEMPTS => return Err(e),
                Err(e) => {
                    tracing::warn!(backend = %self.backend, attempt, "storing a refreshed sign-in failed: {e}");
                    attempt += 1;
                    tokio::time::sleep(STORE_ROTATED_PAUSE).await;
                }
            }
        }
    }
}

/// One refresh call against the grant's token endpoint.
async fn refresh(http: &reqwest::Client, grant: &Grant) -> Result<Grant> {
    let profile = grant.auth.profile();
    let refresh_token = grant
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("this sign-in has no refresh token"))?;
    let client_id = grant
        .client_id
        .as_deref()
        .ok_or_else(|| anyhow!("this sign-in has no client id to refresh with"))?;
    let Flow::OidcDynamic { resource, .. } = &profile.flow else {
        return Err(anyhow!("a {} sign-in does not refresh", profile.title));
    };
    // `scope` omitted, per SIWC: the refresh keeps the granted scopes.
    let form = form_body(&[
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
        ("resource", resource),
    ]);
    let tokens: TokenResponse = post_form(http, &grant.token_url, form, profile.title).await?;
    Ok(Grant {
        access_token: tokens.access_token,
        // Rotation returns a new refresh token; a provider that keeps the old one
        // simply omits it.
        refresh_token: tokens.refresh_token.or_else(|| grant.refresh_token.clone()),
        expires_at: tokens.expires_in.map(|s| now() + s as i64),
        scopes: tokens.scope.or_else(|| grant.scopes.clone()),
        ..grant.clone()
    })
}

/// A transport that sends each request with the backend's signed-in credential.
///
/// Sits outside [`crate::completion_retry::RetryingHttp`], so a 429/5xx retry inside
/// reuses a header that is still valid (the refresh margin is five minutes), and a 401
/// comes out here, where one refresh and one resend can fix it.
#[derive(Clone, Debug)]
pub struct GrantAuthHttp<H> {
    inner: H,
    source: TokenSource,
}

impl<H> GrantAuthHttp<H> {
    /// Wrap `inner` so every request carries `source`'s credential.
    pub fn new(inner: H, source: TokenSource) -> Self {
        Self { inner, source }
    }
}

fn to_http_error(e: anyhow::Error) -> http_client::Error {
    http_client::Error::Instance(e.into())
}

fn with_bearer(
    parts: &http::request::Parts,
    token: &str,
) -> http_client::Result<http::request::Parts> {
    let mut parts = parts.clone();
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
    value.set_sensitive(true);
    parts.headers.insert(http::header::AUTHORIZATION, value);
    Ok(parts)
}

fn is_unauthorized(e: &http_client::Error) -> bool {
    e.non_success_status() == Some(StatusCode::UNAUTHORIZED)
}

/// The refusal for a spent ChatGPT plan, in place of the provider's raw 429 body.
fn plan_limit_refusal(backend: &str) -> http_client::Error {
    to_http_error(anyhow!(
        "the ChatGPT plan behind backend {backend:?} has used its included usage \
         ({}). Sending again fails until the plan's window resets; ChatGPT shows when, \
         under Settings > Usage. Use a cast on another backend until then",
        crate::completion_retry::PLAN_USAGE_LIMIT_CODE
    ))
}

/// Map a spent-plan 429 to [`plan_limit_refusal`]; pass every other result through.
fn name_plan_limit<T>(backend: &str, result: http_client::Result<T>) -> http_client::Result<T> {
    match result {
        Err(e)
            if e.non_success_status() == Some(StatusCode::TOO_MANY_REQUESTS)
                && crate::completion_retry::is_plan_usage_limit(e.non_success_body()) =>
        {
            Err(plan_limit_refusal(backend))
        }
        other => other,
    }
}

impl<H: HttpClientExt + Clone + 'static> HttpClientExt for GrantAuthHttp<H> {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl std::future::Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let inner = self.inner.clone();
        let source = self.source.clone();
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        async move {
            let token = source.bearer().await.map_err(to_http_error)?;
            let first = Request::from_parts(with_bearer(&parts, &token)?, body.clone());
            let result = match inner.send::<Bytes, U>(first).await {
                Err(e) if is_unauthorized(&e) => {
                    let token = source.after_refusal(&token).await.map_err(to_http_error)?;
                    let again = Request::from_parts(with_bearer(&parts, &token)?, body);
                    inner.send::<Bytes, U>(again).await
                }
                other => other,
            };
            name_plan_limit(&source.backend, result)
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<http_client::MultipartForm>,
    ) -> impl std::future::Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        // Not resent on a 401: a multipart form cannot be cloned. kaibo posts none on a
        // completion; this only keeps the wrapper whole.
        let inner = self.inner.clone();
        let source = self.source.clone();
        let (parts, body) = req.into_parts();
        async move {
            let token = source.bearer().await.map_err(to_http_error)?;
            let req = Request::from_parts(with_bearer(&parts, &token)?, body);
            inner.send_multipart::<U>(req).await
        }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl std::future::Future<Output = http_client::Result<http_client::StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        // kaibo's completions read the plan route's event stream as one body through
        // `send` (rig folds it), so this path serves a caller that streams itself. A 401
        // arrives before the stream starts (rig-reqwest turns a non-success head into an
        // error), so it is resent the same way.
        let inner = self.inner.clone();
        let source = self.source.clone();
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        async move {
            let token = source.bearer().await.map_err(to_http_error)?;
            let first = Request::from_parts(with_bearer(&parts, &token)?, body.clone());
            let result = match inner.send_streaming(first).await {
                Err(e) if is_unauthorized(&e) => {
                    let token = source.after_refusal(&token).await.map_err(to_http_error)?;
                    let again = Request::from_parts(with_bearer(&parts, &token)?, body);
                    inner.send_streaming(again).await
                }
                other => other,
            };
            name_plan_limit(&source.backend, result)
        }
    }
}
