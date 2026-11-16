//! Operator sign-in: an OAuth engine that turns a browser consent into a credential a
//! backend can send, with each provider described as data.
//!
//! # What this is for
//!
//! Some providers let a person spend a subscription or mint a key through a browser
//! sign-in instead of pasting an API key. kaibo supports two today, both as a
//! [`Profile`]:
//!
//! - [`AuthProfile::Chatgpt`] — OpenAI's "Sign in with ChatGPT" plan usage (SIWC). An
//!   OpenID Connect authorization-code flow with PKCE and *dynamic* client
//!   registration: the first authorize request names `dynamic_agent_client`, and the
//!   callback hands back an issued `client_id` that later sign-ins and refreshes reuse.
//!   It yields a one-hour access token plus a refresh token that **rotates on every
//!   refresh** — see [`grant`] for why that shapes the refresh lock.
//! - [`AuthProfile::OpenRouter`] — OpenRouter's PKCE key mint. No client id, no OIDC:
//!   the callback carries a code, and exchanging it yields a long-lived API key.
//!
//! The engine is the part they share: PKCE, `state`, the authorize URL, the callback
//! check, and the code exchange. What differs is [`Flow`], matched in a handful of
//! places, so a third provider is a new `Profile` plus whatever new [`Flow`] arm its
//! exchange needs — never a second engine.
//!
//! # Where this sits in the trust model
//!
//! Signing in is an **operator** act: only `kaibo login` starts it (see `cli.rs`), and
//! the MCP server never does. A model call can *use* a credential and, for a refreshing
//! grant, *trigger* a refresh — the same shape as `api_key_cmd`, which a model can
//! trigger and never control. Every endpoint a refresh dials comes from the stored
//! grant, which only `kaibo login` writes; no model input reaches it.
//!
//! The callback listener ([`callback`]) is the one socket kaibo binds: loopback only,
//! one connection served at a time, closed after the first request that carries the
//! right `state`, and bounded by [`callback::CALLBACK_TIMEOUT`]. `tests/no_bind.rs`
//! pins it as the only bind site in `src/`.
//!
//! kaibo never opens a browser for you: that would run an external command, which the
//! invariants reserve for `api_key_cmd`. `kaibo login` prints the URL instead.
//!
//! # ID token checks
//!
//! For the OIDC flow the engine checks the ID token's `iss`, `aud`, `nonce`, and `exp`
//! claims, and does not verify its signature against a JWKS. That is the allowance in
//! OpenID Connect Core §3.1.3.7 (6): an ID token received directly from the token
//! endpoint over TLS, in the code flow, may rely on the TLS server check instead of the
//! signature. kaibo's token request is exactly that — a direct rustls connection to
//! the provider's token endpoint — and the access token is what the provider itself
//! validates on every request. Verifying a JWKS signature would need a JWT crate, and
//! the common ones pull aws-lc, which the TLS invariant forbids.

pub mod callback;
pub mod grant;

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::credentials::ProviderKind;

pub use grant::{Grant, GrantAuthHttp, TokenSource};

/// The sign-in a backend declares with `auth = "<name>"` in config.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthProfile {
    /// OpenAI "Sign in with ChatGPT" plan usage: requests bill the operator's ChatGPT
    /// plan. `openai`-kind backends only.
    Chatgpt,
    /// OpenRouter's PKCE key mint: the sign-in creates an API key on the operator's
    /// OpenRouter account. `openrouter`-kind backends only.
    OpenRouter,
}

impl AuthProfile {
    /// Every profile, in the order help text lists them.
    pub const ALL: [AuthProfile; 2] = [AuthProfile::Chatgpt, AuthProfile::OpenRouter];

    /// The config spelling (`auth = "chatgpt"`).
    pub fn as_str(self) -> &'static str {
        match self {
            AuthProfile::Chatgpt => "chatgpt",
            AuthProfile::OpenRouter => "openrouter",
        }
    }

    /// The backend kind this sign-in produces a credential for.
    pub fn kind(self) -> ProviderKind {
        match self {
            AuthProfile::Chatgpt => ProviderKind::Openai,
            AuthProfile::OpenRouter => ProviderKind::OpenRouter,
        }
    }

    /// The provider description the engine runs.
    pub fn profile(self) -> &'static Profile {
        match self {
            AuthProfile::Chatgpt => &CHATGPT,
            AuthProfile::OpenRouter => &OPENROUTER,
        }
    }
}

impl fmt::Display for AuthProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AuthProfile {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let wanted = s.trim().to_ascii_lowercase();
        AuthProfile::ALL
            .into_iter()
            .find(|p| p.as_str() == wanted)
            .ok_or_else(|| {
                anyhow!(
                    "auth = {s:?} is not a sign-in kaibo knows — use \"chatgpt\" (an \
                     `openai` backend billed to a ChatGPT plan) or \"openrouter\" (an \
                     `openrouter` backend with a key minted by signing in)"
                )
            })
    }
}

/// One provider's sign-in, as data.
#[derive(Debug)]
pub struct Profile {
    /// Which [`AuthProfile`] this is.
    pub auth: AuthProfile,
    /// The provider name a person knows, for prompts ("ChatGPT", "OpenRouter").
    pub title: &'static str,
    /// The page the operator opens to consent.
    pub authorize_url: &'static str,
    /// Where the code is exchanged (and, for a refreshing grant, refreshed).
    pub token_url: &'static str,
    /// The host the redirect URI names. SIWC requires the literal `127.0.0.1`.
    pub redirect_host: &'static str,
    /// The redirect URI's path.
    pub callback_path: &'static str,
    /// The callback port tried first; a busy port falls back to one the OS picks.
    pub preferred_port: u16,
    /// The protocol differences.
    pub flow: Flow,
}

/// What differs between providers' protocols.
#[derive(Debug)]
pub enum Flow {
    /// OpenID Connect authorization code with PKCE and dynamic client registration —
    /// OpenAI SIWC. Yields a refreshing access token.
    OidcDynamic {
        /// The `client_id` the first sign-in sends to be issued its own.
        bootstrap_client_id: &'static str,
        /// The `agent_name_hint` shown on the consent page for a new registration.
        agent_name: &'static str,
        /// Space-separated scopes.
        scopes: &'static str,
        /// The `resource` the tokens are for, sent on authorize, exchange, and refresh.
        resource: &'static str,
        /// The `iss` an ID token must carry.
        issuer: &'static str,
    },
    /// A PKCE code exchanged for a long-lived API key — OpenRouter.
    KeyMint,
}

/// OpenAI "Sign in with ChatGPT", per `developers.openai.com/siwc` (read 2026-11-15).
pub static CHATGPT: Profile = Profile {
    auth: AuthProfile::Chatgpt,
    title: "ChatGPT",
    authorize_url: "https://auth.openai.com/api/accounts/authorize",
    token_url: "https://auth.openai.com/api/accounts/oauth/token",
    redirect_host: "127.0.0.1",
    callback_path: "/auth/callback",
    preferred_port: 1455,
    flow: Flow::OidcDynamic {
        bootstrap_client_id: "dynamic_agent_client",
        agent_name: "kaibo",
        scopes: "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
        resource: crate::credentials::HOSTED_OPENAI_BASE_URL,
        issuer: "https://auth.openai.com",
    },
};

/// OpenRouter's PKCE key mint, per `openrouter.ai/docs` "OAuth PKCE".
pub static OPENROUTER: Profile = Profile {
    auth: AuthProfile::OpenRouter,
    title: "OpenRouter",
    authorize_url: "https://openrouter.ai/auth",
    token_url: "https://openrouter.ai/api/v1/auth/keys",
    redirect_host: "localhost",
    callback_path: "/auth/callback",
    preferred_port: 3000,
    flow: Flow::KeyMint,
};

/// How long a token request may take before the sign-in or refresh gives up.
pub const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One sign-in attempt's secrets: the PKCE verifier and the values the callback must
/// echo. Fresh per attempt, never stored.
#[derive(Clone)]
pub struct Attempt {
    /// The PKCE `code_verifier` (43 base64url characters from 32 random bytes).
    pub verifier: String,
    /// `state`, checked on the callback.
    pub state: String,
    /// `nonce`, checked in the ID token (OIDC flow).
    pub nonce: String,
    /// The exact redirect URI sent on authorize, repeated on the exchange.
    pub redirect_uri: String,
}

impl fmt::Debug for Attempt {
    // The verifier is a secret for the life of the attempt; keep it out of any log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attempt")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// 32 random bytes, base64url without padding.
fn random_token() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The S256 PKCE challenge for `verifier`.
pub fn code_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A fresh UUIDv4 as `urn:uuid:...`: the SIWC `ext_agent_host_id` for this machine.
pub fn new_host_id() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

impl Profile {
    /// The redirect URI for a callback listening on `port`.
    pub fn redirect_uri(&self, port: u16) -> String {
        format!("http://{}:{port}{}", self.redirect_host, self.callback_path)
    }

    /// Whether this provider's `state` rides in the redirect path (`.../callback/<state>`)
    /// instead of a query parameter. OpenRouter takes no `state` parameter and appends
    /// its `code` to the callback URL it was given; a path segment survives whatever it
    /// does with that URL's query, where a `?state=` kaibo put there might not.
    pub fn state_in_path(&self) -> bool {
        matches!(self.flow, Flow::KeyMint)
    }

    /// A fresh attempt whose callback listens on `port`.
    pub fn new_attempt(&self, port: u16) -> Attempt {
        let state = random_token();
        let redirect_uri = if self.state_in_path() {
            format!("{}/{state}", self.redirect_uri(port))
        } else {
            self.redirect_uri(port)
        };
        Attempt {
            verifier: random_token(),
            state,
            nonce: random_token(),
            redirect_uri,
        }
    }

    /// The URL the operator opens to consent.
    ///
    /// `client_id` is the id an earlier sign-in was issued (`None` registers a new
    /// one); `host_id` is this machine's `ext_agent_host_id`. Both matter only to the
    /// OIDC flow. OpenRouter takes no `state` parameter, so `state` rides inside the
    /// callback path instead (see [`Profile::state_in_path`]).
    pub fn authorize_url(
        &self,
        attempt: &Attempt,
        client_id: Option<&str>,
        host_id: &str,
    ) -> String {
        let challenge = code_challenge(&attempt.verifier);
        let mut url = url::Url::parse(self.authorize_url).expect("profile authorize URLs parse");
        {
            let mut q = url.query_pairs_mut();
            match &self.flow {
                Flow::OidcDynamic {
                    bootstrap_client_id,
                    agent_name,
                    scopes,
                    resource,
                    ..
                } => {
                    match client_id {
                        Some(id) => {
                            q.append_pair("client_id", id);
                        }
                        None => {
                            q.append_pair("client_id", bootstrap_client_id);
                            q.append_pair("agent_name_hint", agent_name);
                        }
                    }
                    q.append_pair("ext_agent_host_id", host_id);
                    q.append_pair("response_type", "code");
                    q.append_pair("redirect_uri", &attempt.redirect_uri);
                    q.append_pair("scope", scopes);
                    q.append_pair("resource", resource);
                    q.append_pair("state", &attempt.state);
                    q.append_pair("nonce", &attempt.nonce);
                    q.append_pair("code_challenge_method", "S256");
                    q.append_pair("code_challenge", &challenge);
                }
                Flow::KeyMint => {
                    // `state` is already in the path (see `state_in_path`).
                    q.append_pair("callback_url", &attempt.redirect_uri);
                    q.append_pair("code_challenge", &challenge);
                    q.append_pair("code_challenge_method", "S256");
                }
            }
        }
        url.into()
    }

    /// Check a callback's query string against `attempt`, returning the code to
    /// exchange and, for a new OIDC registration, the issued client id.
    ///
    /// `known_client_id` is the id this backend already holds, if any: a callback that
    /// names a different one is refused, per the SIWC guide.
    pub fn check_callback(
        &self,
        attempt: &Attempt,
        query: &str,
        known_client_id: Option<&str>,
    ) -> Result<CallbackCode> {
        let mut code = None;
        let mut state = None;
        let mut client_id = None;
        let mut error = None;
        let mut error_description = None;
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "client_id" => client_id = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                "error_description" => error_description = Some(v.into_owned()),
                _ => {}
            }
        }
        // State first, on every outcome: an unmatched callback is not ours to act on,
        // not even to report a denial from.
        if state.as_deref() != Some(attempt.state.as_str()) {
            bail!(CallbackError::StateMismatch);
        }
        if let Some(error) = error {
            bail!(CallbackError::Denied {
                error,
                description: error_description,
            });
        }
        let code = code.ok_or(CallbackError::NoCode)?;
        if let (Some(issued), Some(known)) = (client_id.as_deref(), known_client_id) {
            if issued != known {
                bail!(CallbackError::ClientMismatch);
            }
        }
        Ok(CallbackCode { code, client_id })
    }

    /// Exchange a callback's code for a grant on `backend`.
    ///
    /// `token_url` is normally [`Profile::token_url`]; tests aim it at a local server.
    /// `known_client_id` is the id from an earlier sign-in, used when the callback
    /// carried none.
    pub async fn exchange(
        &self,
        http: &reqwest::Client,
        token_url: &str,
        backend: &str,
        attempt: &Attempt,
        callback: CallbackCode,
        known_client_id: Option<&str>,
    ) -> Result<Grant> {
        match &self.flow {
            Flow::OidcDynamic {
                resource, issuer, ..
            } => {
                let client_id = callback
                    .client_id
                    .or_else(|| known_client_id.map(str::to_string))
                    .ok_or_else(|| {
                        anyhow!(
                            "the {} sign-in returned no client_id, and this backend has none \
                             from an earlier sign-in — run `kaibo login {backend}` again",
                            self.title
                        )
                    })?;
                let form = form_body(&[
                    ("grant_type", "authorization_code"),
                    ("client_id", &client_id),
                    ("code", &callback.code),
                    ("code_verifier", &attempt.verifier),
                    ("redirect_uri", &attempt.redirect_uri),
                    ("resource", resource),
                ]);
                let tokens: TokenResponse = post_form(http, token_url, form, self.title).await?;
                let id_token = tokens.id_token.as_deref().ok_or_else(|| {
                    anyhow!("the {} token response carried no id_token", self.title)
                })?;
                let claims = check_id_token(id_token, issuer, &client_id, &attempt.nonce)?;
                Ok(Grant {
                    backend: backend.to_string(),
                    auth: self.auth,
                    client_id: Some(client_id),
                    token_url: token_url.to_string(),
                    access_token: tokens.access_token,
                    refresh_token: tokens.refresh_token,
                    expires_at: tokens.expires_in.map(|s| grant::now() + s as i64),
                    account: claims.email.or(claims.sub),
                    scopes: tokens.scope,
                })
            }
            Flow::KeyMint => {
                let body = serde_json::json!({
                    "code": callback.code,
                    "code_verifier": attempt.verifier,
                    "code_challenge_method": "S256",
                });
                let response = http
                    .post(token_url)
                    .timeout(TOKEN_REQUEST_TIMEOUT)
                    .json(&body)
                    .send()
                    .await
                    .with_context(|| format!("reaching the {} key endpoint", self.title))?;
                let minted: KeyMintResponse = read_json(response, self.title).await?;
                Ok(Grant {
                    backend: backend.to_string(),
                    auth: self.auth,
                    client_id: None,
                    token_url: token_url.to_string(),
                    access_token: minted.key,
                    refresh_token: None,
                    expires_at: None,
                    account: minted.user_id,
                    scopes: None,
                })
            }
        }
    }
}

/// Run one sign-in for `backend` end to end: bind the callback, hand the operator the
/// URL to open (`show_url` gets it and the callback port), wait for the browser,
/// exchange the code at `token_url`, and store the grant. Returns the stored grant.
///
/// A backend signed in before keeps its issued client id, so a new sign-in
/// re-authorizes the same OpenAI client instead of registering another.
pub async fn sign_in(
    profile: &Profile,
    backend: &str,
    store: &crate::store::CredentialStore,
    http: &reqwest::Client,
    token_url: &str,
    timeout: Duration,
    show_url: impl FnOnce(&str, u16),
) -> Result<Grant> {
    let known_client = store
        .get_grant(backend)
        .await?
        .filter(|g| g.auth == profile.auth)
        .and_then(|g| g.client_id);
    let host_id = store.host_id().await?;
    let listener = callback::bind_loopback(profile.preferred_port).await?;
    let attempt = profile.new_attempt(listener.port());
    show_url(
        &profile.authorize_url(&attempt, known_client.as_deref(), &host_id),
        listener.port(),
    );
    let code = listener
        .receive(profile, &attempt, known_client.as_deref(), timeout)
        .await?;
    let grant = profile
        .exchange(
            http,
            token_url,
            backend,
            &attempt,
            code,
            known_client.as_deref(),
        )
        .await?;
    store.put_grant(&grant).await?;
    Ok(grant)
}

/// What a checked callback carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackCode {
    /// The authorization code to exchange.
    pub code: String,
    /// The issued client id, on a new OIDC registration.
    pub client_id: Option<String>,
}

/// Why a callback was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallbackError {
    /// `state` was missing or not this attempt's.
    #[error(
        "the sign-in callback carried the wrong state, so kaibo ignored it — it did not come \
         from this login attempt. Run `kaibo login` again and finish in the browser it prints"
    )]
    StateMismatch,
    /// The provider reported an error, such as the operator declining.
    #[error("the provider refused the sign-in: {error}{}", description.as_deref().map(|d| format!(" ({d})")).unwrap_or_default())]
    Denied {
        /// The `error` parameter, e.g. `access_denied`.
        error: String,
        /// The optional `error_description`.
        description: Option<String>,
    },
    /// No `code` parameter.
    #[error("the sign-in callback carried no code — run `kaibo login` again")]
    NoCode,
    /// A different client id than the one this backend holds.
    #[error(
        "the sign-in returned a different client_id than this backend's earlier sign-in, so \
         kaibo refused it. Run `kaibo logout <backend>`, then `kaibo login <backend>`"
    )]
    ClientMismatch,
}

/// The OAuth token endpoint's JSON reply (exchange and refresh).
#[derive(Debug, Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// OpenRouter's key-mint reply.
#[derive(Debug, Deserialize)]
struct KeyMintResponse {
    key: String,
    #[serde(default)]
    user_id: Option<String>,
}

/// The ID token claims kaibo reads.
#[derive(Debug, Deserialize)]
struct IdClaims {
    iss: String,
    aud: Audience,
    #[serde(default)]
    nonce: Option<String>,
    exp: i64,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, id: &str) -> bool {
        match self {
            Audience::One(a) => a == id,
            Audience::Many(v) => v.iter().any(|a| a == id),
        }
    }
}

/// Check an ID token's claims (see the module doc for why not its signature).
fn check_id_token(token: &str, issuer: &str, client_id: &str, nonce: &str) -> Result<IdClaims> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("the ID token is not a JWT"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("the ID token payload is not base64url")?;
    let claims: IdClaims =
        serde_json::from_slice(&bytes).context("the ID token payload is not the expected JSON")?;
    if claims.iss.trim_end_matches('/') != issuer.trim_end_matches('/') {
        bail!("the ID token names issuer {:?}, not {issuer:?}", claims.iss);
    }
    if !claims.aud.contains(client_id) {
        bail!("the ID token was not issued to this client");
    }
    if claims.nonce.as_deref() != Some(nonce) {
        bail!("the ID token's nonce does not match this sign-in attempt");
    }
    // A minute of skew allowance: the token is seconds old by construction.
    if claims.exp + 60 < grant::now() {
        bail!("the ID token has already expired — check this machine's clock");
    }
    Ok(claims)
}

/// `application/x-www-form-urlencoded` body for `pairs`.
pub(crate) fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// POST a form to a token endpoint and read its JSON reply.
pub(crate) async fn post_form<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    form: String,
    title: &str,
) -> Result<T> {
    let response = http
        .post(url)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(http::header::ACCEPT, "application/json")
        .body(form)
        .send()
        .await
        .with_context(|| format!("reaching the {title} token endpoint"))?;
    read_json(response, title).await
}

/// A token endpoint's failure, with the OAuth `error` code when it sent one.
#[derive(Debug, Clone, thiserror::Error)]
#[error("the {title} token endpoint answered HTTP {status}{}", code.as_deref().map(|c| format!(" `{c}`")).unwrap_or_default())]
pub struct TokenEndpointError {
    /// Provider name.
    pub title: String,
    /// HTTP status.
    pub status: u16,
    /// The OAuth `error` field (`invalid_grant`, ...), when present.
    pub code: Option<String>,
}

impl TokenEndpointError {
    /// The grant itself is dead (revoked, expired, or the app was disconnected): only
    /// a new sign-in fixes it.
    pub fn grant_is_dead(&self) -> bool {
        matches!(
            self.code.as_deref(),
            Some("invalid_grant" | "invalid_client" | "unauthorized_client")
        )
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    title: &str,
) -> Result<T> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("reading the {title} token endpoint's reply"))?;
    if !status.is_success() {
        // The body may echo request details; keep only the OAuth error code.
        let code = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.get("error").and_then(|e| {
                    e.as_str()
                        .map(str::to_string)
                        .or_else(|| e.get("code").and_then(|c| c.as_str()).map(str::to_string))
                })
            });
        return Err(TokenEndpointError {
            title: title.to_string(),
            status: status.as_u16(),
            code,
        }
        .into());
    }
    serde_json::from_slice(&bytes)
        .with_context(|| format!("the {title} token endpoint's reply is not the expected JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: serde_json::Value) -> String {
        let enc = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.sig",
            enc(br#"{"alg":"RS256"}"#),
            enc(claims.to_string().as_bytes())
        )
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_appendix_b() {
        // RFC 7636 Appendix B's worked example.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn attempts_are_fresh_and_verifiers_are_rfc_length() {
        let a = CHATGPT.new_attempt(1455);
        let b = CHATGPT.new_attempt(1455);
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
        assert_eq!(
            a.verifier.len(),
            43,
            "32 bytes base64url, within RFC 7636's 43..=128"
        );
        assert_eq!(a.redirect_uri, "http://127.0.0.1:1455/auth/callback");
    }

    #[test]
    fn host_ids_are_uuid_v4_urns() {
        let id = new_host_id();
        assert!(id.starts_with("urn:uuid:"), "{id}");
        let uuid = &id["urn:uuid:".len()..];
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4", "version nibble: {uuid}");
        assert!("89ab".contains(&uuid[19..20]), "variant nibble: {uuid}");
        assert_ne!(new_host_id(), id);
    }

    #[test]
    fn chatgpt_first_sign_in_registers_with_the_bootstrap_client() {
        let a = CHATGPT.new_attempt(1455);
        let url = url::Url::parse(&CHATGPT.authorize_url(&a, None, "urn:uuid:x")).unwrap();
        assert_eq!(url.host_str(), Some("auth.openai.com"));
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "dynamic_agent_client");
        assert_eq!(q["agent_name_hint"], "kaibo");
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:x");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
        assert_eq!(q["resource"], "https://api.openai.com/v1");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], code_challenge(&a.verifier));
        assert!(q["scope"].contains("chatgpt.tokens.use.direct"));
        assert!(q["scope"].contains("offline_access"));
        assert_eq!(q["state"], a.state);
        assert_eq!(q["nonce"], a.nonce);
        assert!(
            !url.as_str().contains(&a.verifier),
            "the verifier never leaves the machine"
        );
    }

    #[test]
    fn chatgpt_reauthorization_reuses_the_issued_client() {
        let a = CHATGPT.new_attempt(1455);
        let url = url::Url::parse(&CHATGPT.authorize_url(&a, Some("oaiapp_1"), "h")).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "oaiapp_1");
        assert!(!q.contains_key("agent_name_hint"));
    }

    #[test]
    fn openrouter_carries_state_inside_the_callback_url() {
        let a = OPENROUTER.new_attempt(3000);
        let url = url::Url::parse(&OPENROUTER.authorize_url(&a, None, "unused")).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            q["callback_url"],
            format!("http://localhost:3000/auth/callback/{}", a.state),
            "state rides in the path, so OpenRouter's `?code=` cannot disturb it"
        );
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(!q.contains_key("client_id"));
    }

    #[test]
    fn callback_checks_state_before_anything_else() {
        let a = CHATGPT.new_attempt(1455);
        let wrong = CHATGPT
            .check_callback(&a, "code=c&state=nope", None)
            .unwrap_err();
        assert_eq!(wrong.downcast_ref(), Some(&CallbackError::StateMismatch));
        // A denial with the wrong state is ignored the same way, not reported.
        let spoofed = CHATGPT
            .check_callback(&a, "error=access_denied&state=nope", None)
            .unwrap_err();
        assert_eq!(spoofed.downcast_ref(), Some(&CallbackError::StateMismatch));
        let denied = CHATGPT
            .check_callback(&a, &format!("error=access_denied&state={}", a.state), None)
            .unwrap_err();
        assert!(matches!(
            denied.downcast_ref(),
            Some(CallbackError::Denied { .. })
        ));
    }

    #[test]
    fn callback_returns_code_and_issued_client() {
        let a = CHATGPT.new_attempt(1455);
        let q = format!(
            "code=abc&scope=openid+email&state={}&client_id=oaiapp_9",
            a.state
        );
        let got = CHATGPT.check_callback(&a, &q, None).unwrap();
        assert_eq!(
            got,
            CallbackCode {
                code: "abc".into(),
                client_id: Some("oaiapp_9".into())
            }
        );
        let mismatch = CHATGPT
            .check_callback(&a, &q, Some("oaiapp_1"))
            .unwrap_err();
        assert_eq!(
            mismatch.downcast_ref(),
            Some(&CallbackError::ClientMismatch)
        );
    }

    #[test]
    fn id_token_claims_are_checked() {
        let good = serde_json::json!({
            "iss": "https://auth.openai.com", "aud": "oaiapp_1", "nonce": "n",
            "exp": grant::now() + 600, "sub": "user-1", "email": "a@example.com"
        });
        let c = check_id_token(
            &jwt(good.clone()),
            "https://auth.openai.com",
            "oaiapp_1",
            "n",
        )
        .unwrap();
        assert_eq!(c.email.as_deref(), Some("a@example.com"));

        let mut bad = good.clone();
        bad["nonce"] = "other".into();
        assert!(check_id_token(&jwt(bad), "https://auth.openai.com", "oaiapp_1", "n").is_err());
        let mut bad = good.clone();
        bad["aud"] = serde_json::json!(["someone-else"]);
        assert!(check_id_token(&jwt(bad), "https://auth.openai.com", "oaiapp_1", "n").is_err());
        let mut bad = good.clone();
        bad["iss"] = "https://evil.example".into();
        assert!(check_id_token(&jwt(bad), "https://auth.openai.com", "oaiapp_1", "n").is_err());
        let mut bad = good;
        bad["exp"] = (grant::now() - 3600).into();
        assert!(check_id_token(&jwt(bad), "https://auth.openai.com", "oaiapp_1", "n").is_err());
    }

    #[test]
    fn auth_profile_parses_and_refuses_unknowns() {
        assert_eq!(
            "chatgpt".parse::<AuthProfile>().unwrap(),
            AuthProfile::Chatgpt
        );
        assert_eq!(
            " OpenRouter ".parse::<AuthProfile>().unwrap(),
            AuthProfile::OpenRouter
        );
        let err = "codex".parse::<AuthProfile>().unwrap_err().to_string();
        assert!(
            err.contains("\"chatgpt\"") && err.contains("\"openrouter\""),
            "{err}"
        );
    }

    #[test]
    fn dead_grant_codes() {
        let e = |c: &str| TokenEndpointError {
            title: "x".into(),
            status: 400,
            code: Some(c.into()),
        };
        assert!(e("invalid_grant").grant_is_dead());
        assert!(!e("temporarily_unavailable").grant_is_dead());
    }
}
