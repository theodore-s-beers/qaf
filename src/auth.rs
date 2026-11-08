use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use rand::{RngCore, rngs::OsRng};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";
const RESOURCE: &str = "https://api.openai.com/v1";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

#[derive(Clone)]
pub struct Store {
    directory: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct State {
    host_id: String,
    #[serde(default)]
    accounts: BTreeMap<String, Account>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Account {
    client_id: String,
    subject: Option<String>,
    email: Option<String>,
    credentials: Option<Credentials>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Credentials {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_at: u64,
    scope: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    refresh_token: Option<String>,
    id_token: Option<String>,
    scope: Option<String>,
}

#[derive(Clone, Deserialize)]
struct Identity {
    sub: String,
    email: Option<String>,
    nonce: Option<String>,
    azp: Option<String>,
    aud: Value,
}

impl Store {
    pub fn new(directory: Option<PathBuf>) -> Result<Self> {
        let directory = match directory {
            Some(path) => path,
            None => match std::env::var_os("XDG_CONFIG_HOME") {
                Some(path) => PathBuf::from(path).join("qaf"),
                None => PathBuf::from(
                    std::env::var_os("HOME").context("HOME is not set; use --auth-dir")?,
                )
                .join(".config/qaf"),
            },
        };
        Ok(Self { directory })
    }

    // A process-wide file lock serializes rotating refresh tokens and atomic updates.
    fn lock(&self) -> Result<File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.directory)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&self.directory)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(self.directory.join("auth.lock"))
            .context("open authentication lock")?;
        file.try_lock()
            .context("another qaf process is updating authentication; try again shortly")?;
        Ok(file)
    }

    fn read(&self) -> Result<State> {
        match std::fs::read(self.directory.join("auth.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("read qaf authentication state"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State {
                host_id: format!("urn:uuid:{}", uuid::Uuid::new_v4()),
                accounts: BTreeMap::new(),
            }),
            Err(error) => Err(error).context("read qaf authentication state"),
        }
    }

    fn save(&self, state: &State) -> Result<()> {
        // NamedTempFile is created with mode 0600; rename replaces the record atomically.
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer(file.as_file_mut(), state)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join("auth.json"))
            .context("save authentication state")?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    pub fn status(&self) -> Result<()> {
        let _lock = self.lock()?;
        let state = self.read()?;
        if state.accounts.is_empty() {
            println!("Not signed in. Run qaf login.");
        }
        for (label, account) in state.accounts {
            let status = match account.credentials {
                Some(ref c) if has_plan(&c.scope) => "ChatGPT plan enabled",
                Some(_) => "signed in; ChatGPT plan usage not granted",
                None => "signed out",
            };
            println!(
                "{label}: {} — {status}",
                account.email.as_deref().unwrap_or("account")
            );
        }
        Ok(())
    }

    pub async fn login(&self, http: &Client, label: &str, no_browser: bool) -> Result<()> {
        ensure!(!label.trim().is_empty(), "account label must not be empty");
        let _lock = self.lock()?;
        let mut state = self.read()?;
        self.save(&state)?; // Persist the host before its first authorization.
        let previous = state.accounts.get(label).cloned();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("listen for ChatGPT sign-in")?;
        let pending = Pending::new(
            listener.local_addr()?.port(),
            previous.as_ref().map(|a| a.client_id.clone()),
        );
        let url = pending.url(&state.host_id)?;
        // Deliberately omit id_token_hint so the displayed URL contains no tokens.
        println!(
            "Continue with ChatGPT:\n{url}\n\nWaiting for browser sign-in (5 minute timeout)..."
        );
        if !no_browser {
            let program = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            if !matches!(tokio::process::Command::new(program).arg(url.as_str())
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                .status().await, Ok(status) if status.success())
            {
                eprintln!("Open the link above in your browser.");
            }
        }
        let (callback, mut stream) =
            tokio::time::timeout(Duration::from_secs(300), pending.receive(&listener))
                .await
                .context("sign-in timed out; run qaf login again")??;
        let result = async {
            // Retain the issued registration even if a one-use code expires during exchange.
            let account = state.accounts.entry(label.to_owned()).or_insert_with(|| Account {
                client_id: callback.client_id.clone(), subject: None, email: None, credentials: None,
            });
            ensure!(account.client_id == callback.client_id, "callback changed the selected registration");
            self.save(&state)?;
            let token = exchange(http, TOKEN, &[
                ("grant_type", "authorization_code"), ("client_id", &callback.client_id),
                ("code", &callback.code), ("code_verifier", &pending.verifier),
                ("redirect_uri", &pending.redirect), ("resource", RESOURCE),
            ]).await?;
            let id_token = token.id_token.as_deref().context("sign-in returned no ID token")?;
            let keys: JwkSet = http.get(JWKS).send().await?.error_for_status()?.json().await?;
            let identity = validate_identity(id_token, &keys, &callback.client_id, Some(&pending.nonce))?;
            if let Some(expected) = previous.as_ref().and_then(|a| a.subject.as_deref()) {
                ensure!(identity.sub == expected, "sign-in returned a different account; use a new --account label");
            }
            let credentials = token.credentials(None)?;
            let enabled = has_plan(&credentials.scope);
            state.accounts.insert(label.to_owned(), Account {
                client_id: callback.client_id, subject: Some(identity.sub), email: identity.email,
                credentials: Some(credentials),
            });
            self.save(&state)?;
            ensure!(enabled, "signed in, but ChatGPT plan usage was not granted; run qaf login again and enable plan usage");
            Ok(())
        }.await;
        let message = if result.is_ok() {
            "Signed in to qaf. You can close this tab."
        } else {
            "Sign-in could not enable qaf. Check your terminal for details."
        };
        let _ = browser_reply(&mut stream, "200 OK", message).await;
        result
    }

    pub async fn access_token(&self, http: &Client, label: &str) -> Result<String> {
        self.access_token_at(http, label, TOKEN).await
    }

    async fn access_token_at(
        &self,
        http: &Client,
        label: &str,
        token_endpoint: &str,
    ) -> Result<String> {
        let _lock = self.lock()?;
        let mut state = self.read()?;
        let account = state
            .accounts
            .get_mut(label)
            .context("account not registered; run qaf login")?;
        let old = account
            .credentials
            .as_ref()
            .context("signed out; run qaf login")?;
        ensure!(
            has_plan(&old.scope),
            "ChatGPT plan usage is not enabled; run qaf login"
        );
        if old.expires_at > now()? + 60 {
            return Ok(old.access_token.clone());
        }
        let refresh = old
            .refresh_token
            .as_deref()
            .context("session cannot refresh; run qaf login")?;
        let token = exchange(
            http,
            token_endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &account.client_id),
                ("refresh_token", refresh),
                ("resource", RESOURCE),
            ],
        )
        .await
        .context("refresh ChatGPT session; if revoked, run qaf login")?;
        if let Some(id_token) = token.id_token.as_deref() {
            let keys: JwkSet = http
                .get(JWKS)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let identity = validate_identity(id_token, &keys, &account.client_id, None)?;
            ensure!(
                Some(identity.sub.as_str()) == account.subject.as_deref(),
                "refresh returned a different identity"
            );
        }
        let credentials = token.credentials(Some(old))?;
        let access = credentials.access_token.clone();
        let enabled = has_plan(&credentials.scope);
        account.credentials = Some(credentials);
        self.save(&state)?;
        ensure!(
            enabled,
            "ChatGPT plan permission was removed; run qaf login"
        );
        Ok(access)
    }

    pub async fn logout(&self, http: &Client, label: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut state = self.read()?;
        let account = state
            .accounts
            .get_mut(label)
            .context("account not registered")?;
        let revoked = if let Some(refresh) = account
            .credentials
            .as_ref()
            .and_then(|c| c.refresh_token.as_deref())
        {
            revoke(http, &account.client_id, refresh).await.is_ok()
        } else {
            true
        };
        account.credentials = None;
        self.save(&state)?;
        println!("Signed out locally from {label}.");
        if !revoked {
            eprintln!(
                "Remote revocation was not confirmed. Disconnect qaf in ChatGPT Settings to revoke it."
            );
        }
        Ok(())
    }
}

impl TokenResponse {
    fn credentials(self, old: Option<&Credentials>) -> Result<Credentials> {
        ensure!(
            self.token_type.eq_ignore_ascii_case("bearer"),
            "unsupported OAuth token type"
        );
        ensure!(
            !self.access_token.is_empty() && self.expires_in > 0,
            "invalid OAuth token response"
        );
        let scope = self
            .scope
            .or_else(|| old.map(|c| c.scope.clone()))
            .context("token response omitted granted scopes")?;
        Ok(Credentials {
            access_token: self.access_token,
            refresh_token: self
                .refresh_token
                .or_else(|| old.and_then(|c| c.refresh_token.clone())),
            id_token: self
                .id_token
                .or_else(|| old.and_then(|c| c.id_token.clone())),
            expires_at: now()?
                .checked_add(self.expires_in)
                .context("invalid token expiry")?,
            scope,
        })
    }
}

fn has_plan(scope: &str) -> bool {
    scope.split_whitespace().any(|s| s == PLAN_SCOPE)
}
fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

async fn exchange(http: &Client, endpoint: &str, fields: &[(&str, &str)]) -> Result<TokenResponse> {
    let response = http
        .post(endpoint)
        .form(fields)
        .send()
        .await
        .context("contact OpenAI authentication")?;
    let status = response.status();
    if !status.is_success() {
        // Never put token response bodies or authorization codes in errors/logs.
        let body: Value = response.json().await.unwrap_or(Value::Null);
        let code = body["error"]
            .as_str()
            .filter(|s| s.len() < 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or("oauth_error");
        bail!("OpenAI authentication returned {status} ({code})");
    }
    response.json().await.context("invalid OAuth response")
}

async fn revoke(http: &Client, client_id: &str, refresh: &str) -> Result<()> {
    let discovery: Value = http
        .get(format!("{ISSUER}/.well-known/openid-configuration"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let endpoint = Url::parse(
        discovery["revocation_endpoint"]
            .as_str()
            .context("no revocation endpoint")?,
    )?;
    ensure!(
        endpoint.scheme() == "https" && endpoint.host_str() == Some("auth.openai.com"),
        "untrusted revocation endpoint"
    );
    for attempt in 0..3 {
        match http
            .post(endpoint.clone())
            .form(&[
                ("token", refresh),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ])
            .send()
            .await
        {
            Ok(response) if response.status() == reqwest::StatusCode::OK => return Ok(()),
            Ok(response) if !response.status().is_server_error() => bail!("revocation rejected"),
            _ => {}
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
        }
    }
    bail!("could not confirm revocation")
}

fn validate_identity(
    token: &str,
    keys: &JwkSet,
    client_id: &str,
    nonce: Option<&str>,
) -> Result<Identity> {
    let header = decode_header(token).context("invalid ID token header")?;
    ensure!(
        header.alg == Algorithm::RS256,
        "unsupported ID token signature algorithm"
    );
    let key = keys
        .find(header.kid.as_deref().context("ID token has no key ID")?)
        .context("ID token signing key not found")?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 0;
    let claims = decode::<Identity>(token, &DecodingKey::from_jwk(key)?, &validation)
        .context("ID token validation failed")?
        .claims;
    ensure!(!claims.sub.is_empty(), "ID token has empty subject");
    if let Some(expected) = nonce {
        ensure!(
            claims.nonce.as_deref() == Some(expected),
            "ID token nonce mismatch"
        );
    }
    if claims.azp.is_some() || claims.aud.as_array().is_some_and(|a| a.len() > 1) {
        ensure!(
            claims.azp.as_deref() == Some(client_id),
            "ID token authorized party mismatch"
        );
    }
    Ok(claims)
}

struct Pending {
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
    client_id: Option<String>,
}
struct Callback {
    code: String,
    client_id: String,
}

impl Pending {
    fn new(port: u16, client_id: Option<String>) -> Self {
        Self {
            state: random(),
            nonce: random(),
            verifier: random(),
            redirect: format!("http://127.0.0.1:{port}/auth/callback"),
            client_id,
        }
    }

    fn url(&self, host: &str) -> Result<Url> {
        let mut url = Url::parse(AUTHORIZE)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        {
            let mut query = url.query_pairs_mut();
            query.extend_pairs([
                (
                    "client_id",
                    self.client_id.as_deref().unwrap_or("dynamic_agent_client"),
                ),
                ("ext_agent_host_id", host),
                ("response_type", "code"),
                ("redirect_uri", &self.redirect),
                ("scope", SCOPES),
                ("resource", RESOURCE),
                ("state", &self.state),
                ("nonce", &self.nonce),
                ("code_challenge_method", "S256"),
                ("code_challenge", &challenge),
            ]);
            if self.client_id.is_none() {
                query.append_pair("agent_name_hint", "qaf");
            }
            // Explicit login is also the recovery path after declining plan permission.
            query.append_pair("prompt", "consent");
        }
        Ok(url)
    }

    fn callback(&self, url: &Url) -> Result<Callback> {
        let mut params = BTreeMap::new();
        for (key, value) in url.query_pairs() {
            ensure!(
                params
                    .insert(key.into_owned(), value.into_owned())
                    .is_none(),
                "duplicate callback parameter"
            );
        }
        ensure!(
            params.get("state") == Some(&self.state),
            "OAuth state mismatch"
        );
        ensure!(
            !params.contains_key("error"),
            "ChatGPT sign-in was declined or failed"
        );
        let client_id = match (&self.client_id, params.get("client_id")) {
            (Some(saved), Some(returned)) => {
                ensure!(saved == returned, "callback client ID mismatch");
                saved.clone()
            }
            (Some(saved), None) => saved.clone(),
            (None, Some(issued)) => issued.clone(),
            _ => bail!("registration callback omitted client ID"),
        };
        ensure!(
            !client_id.is_empty() && client_id != "dynamic_agent_client",
            "callback did not issue a client ID"
        );
        let code = params
            .get("code")
            .filter(|s| !s.is_empty())
            .context("callback omitted authorization code")?
            .clone();
        Ok(Callback { code, client_id })
    }

    async fn receive(&self, listener: &TcpListener) -> Result<(Callback, TcpStream)> {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let Ok(Ok(url)) = tokio::time::timeout(
                Duration::from_secs(3),
                read_callback(&mut stream, &self.redirect),
            )
            .await
            else {
                let _ = browser_reply(&mut stream, "400 Bad Request", "Invalid callback.").await;
                continue;
            };
            // Unrelated requests cannot terminate an in-progress login.
            if !url
                .query_pairs()
                .any(|(key, value)| key == "state" && value == self.state)
            {
                let _ =
                    browser_reply(&mut stream, "400 Bad Request", "Invalid sign-in state.").await;
                continue;
            }
            match self.callback(&url) {
                Ok(callback) => return Ok((callback, stream)),
                Err(error) => {
                    let _ = browser_reply(
                        &mut stream,
                        "400 Bad Request",
                        "Sign-in failed. Check your terminal.",
                    )
                    .await;
                    return Err(error);
                }
            }
        }
    }
}

fn random() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

async fn read_callback(stream: &mut TcpStream, redirect: &str) -> Result<Url> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 1024];
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        let count = stream.read(&mut buffer).await?;
        ensure!(count > 0, "incomplete callback");
        bytes.extend_from_slice(&buffer[..count]);
        ensure!(bytes.len() <= 16 * 1024, "callback too large");
    }
    let text = std::str::from_utf8(&bytes)?;
    let mut request = text
        .lines()
        .next()
        .context("missing HTTP request")?
        .split_whitespace();
    ensure!(request.next() == Some("GET"), "callback must use GET");
    let target = request.next().context("missing callback target")?;
    ensure!(
        target.starts_with("/auth/callback?"),
        "incorrect callback path"
    );
    let mut url = Url::parse(redirect)?;
    let query = target
        .strip_prefix("/auth/callback?")
        .context("incorrect callback")?;
    url.set_query(Some(query));
    Ok(url)
}

async fn browser_reply(stream: &mut TcpStream, status: &str, text: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        stream.write_all(response.as_bytes()),
    )
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    fn keys() -> JwkSet {
        serde_json::from_str(include_str!("../tests/fixtures/oauth-test-jwks.json")).unwrap()
    }

    fn claims() -> Value {
        json!({"iss": ISSUER, "sub": "user-123", "aud": "oaiapp_test",
            "exp": now().unwrap() + 3600, "nonce": "nonce-123", "email": "user@example.test"})
    }

    fn sign(claims: &Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".into());
        // Generated test-only key, never used for a real account or service.
        let key = EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/oauth-test-key.pem"))
            .unwrap();
        encode(&header, claims, &key).unwrap()
    }

    #[test]
    fn verifies_signature_issuer_audience_expiry_nonce_and_authorized_party() {
        let original = claims();
        assert_eq!(
            validate_identity(&sign(&original), &keys(), "oaiapp_test", Some("nonce-123"))
                .unwrap()
                .sub,
            "user-123"
        );
        for (field, value) in [
            ("iss", json!("https://attacker.test")),
            ("aud", json!("another-client")),
            ("exp", json!(1)),
            ("nonce", json!("wrong")),
            ("sub", json!("")),
            ("azp", json!("another-client")),
        ] {
            let mut altered = original.clone();
            altered[field] = value;
            assert!(
                validate_identity(&sign(&altered), &keys(), "oaiapp_test", Some("nonce-123"))
                    .is_err(),
                "{field}"
            );
        }
        let mut token = sign(&original).into_bytes();
        let last = token.len() - 10;
        token[last] = if token[last] == b'A' { b'B' } else { b'A' };
        assert!(
            validate_identity(
                std::str::from_utf8(&token).unwrap(),
                &keys(),
                "oaiapp_test",
                Some("nonce-123")
            )
            .is_err()
        );
        let hmac = encode(
            &Header::default(),
            &original,
            &EncodingKey::from_secret(b"not-rsa"),
        )
        .unwrap();
        assert!(validate_identity(&hmac, &keys(), "oaiapp_test", Some("nonce-123")).is_err());
    }

    #[test]
    fn pkce_uses_s256_and_returning_login_reuses_registration() {
        let pending = Pending::new(12345, None);
        assert_eq!(pending.verifier.len(), 43);
        let url = pending.url("urn:uuid:host").unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["client_id"], "dynamic_agent_client");
        assert_eq!(
            query["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()))
        );
        assert_eq!(
            query["redirect_uri"],
            "http://127.0.0.1:12345/auth/callback"
        );
        assert_eq!(query["agent_name_hint"], "qaf");
        let returning = Pending::new(12346, Some("oaiapp_test".into()));
        let url = returning.url("urn:uuid:host").unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["client_id"], "oaiapp_test");
        assert!(!query.contains_key("agent_name_hint"));
        assert_ne!(pending.state, returning.state);
    }

    #[test]
    fn callback_rejects_wrong_state_denial_duplicate_parameters_and_changed_client() {
        let pending = Pending::new(12345, None);
        let valid = format!(
            "{}?state={}&code=secret-code&client_id=oaiapp_test",
            pending.redirect, pending.state
        );
        assert_eq!(
            pending
                .callback(&Url::parse(&valid).unwrap())
                .unwrap()
                .client_id,
            "oaiapp_test"
        );
        for url in [
            valid.replace(&pending.state, "wrong"),
            format!("{valid}&error=access_denied"),
            format!("{valid}&state=extra"),
            valid.replace("&client_id=oaiapp_test", ""),
            valid.replace("oaiapp_test", "dynamic_agent_client"),
        ] {
            assert!(pending.callback(&Url::parse(&url).unwrap()).is_err());
        }
        let returning = Pending {
            client_id: Some("oaiapp_other".into()),
            ..pending
        };
        assert!(returning.callback(&Url::parse(&valid).unwrap()).is_err());
        assert_eq!(
            returning
                .callback(&Url::parse(&valid.replace("&client_id=oaiapp_test", "")).unwrap())
                .unwrap()
                .client_id,
            "oaiapp_other"
        );
    }

    #[test]
    fn token_response_replaces_tokens_and_preserves_omitted_fields() {
        let old = Credentials {
            access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            id_token: Some("old-id".into()),
            expires_at: 1,
            scope: SCOPES.into(),
        };
        let response: TokenResponse = serde_json::from_value(json!({
            "access_token": "new-access", "token_type": "Bearer", "expires_in": 3600,
            "refresh_token": "new-refresh"
        }))
        .unwrap();
        let updated = response.credentials(Some(&old)).unwrap();
        assert_eq!(updated.access_token, "new-access");
        assert_eq!(updated.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(updated.id_token.as_deref(), Some("old-id"));
        assert_eq!(updated.scope, old.scope);
        assert!(updated.expires_at > now().unwrap());
    }

    #[test]
    fn plan_permission_requires_an_exact_scope() {
        assert!(has_plan(SCOPES));
        for scope in ["", "openid email", "chatgpt.tokens.use.direct.fake"] {
            assert!(!has_plan(scope), "{scope}");
        }
    }

    #[test]
    fn store_preserves_accounts_permissions_and_excludes_a_second_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Some(dir.path().join("auth"))).unwrap();
        let lock = store.lock().unwrap();
        let mut state = store.read().unwrap();
        let host = state.host_id.clone();
        store.save(&state).unwrap();
        assert!(store.lock().is_err());
        state.accounts.insert(
            "work".into(),
            Account {
                client_id: "work-client".into(),
                subject: Some("same-subject".into()),
                email: Some("same@example.test".into()),
                credentials: None,
            },
        );
        state.accounts.insert(
            "personal".into(),
            Account {
                client_id: "personal-client".into(),
                subject: Some("same-subject".into()),
                email: Some("same@example.test".into()),
                credentials: None,
            },
        );
        store.save(&state).unwrap();
        let loaded = store.read().unwrap();
        assert_eq!(loaded.host_id, host);
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts["work"].client_id, "work-client");
        assert_eq!(loaded.accounts["personal"].client_id, "personal-client");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(store.directory.join("auth.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        drop(lock);
        assert!(store.lock().is_ok());
    }

    #[tokio::test]
    async fn callback_listener_ignores_unrelated_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pending = Pending::new(listener.local_addr().unwrap().port(), None);
        let client = async {
            let mut wrong = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            wrong
                .write_all(b"GET /auth/callback?state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            wrong.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 400"));
            let mut right = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            right.write_all(format!("GET /auth/callback?state={}&code=code&client_id=oaiapp_test HTTP/1.1\r\n\r\n", pending.state).as_bytes()).await.unwrap();
        };
        let (received, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(pending.receive(&listener), client)
        })
        .await
        .expect("callback test timed out");
        assert_eq!(received.unwrap().0.client_id, "oaiapp_test");
    }

    #[tokio::test]
    async fn token_exchange_is_form_encoded_and_errors_do_not_leak_tokens() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let form = read_form(&mut socket).await;
            assert_eq!(form["code"], "one+two");
            assert_eq!(form["client_id"], "oaiapp_test");
            let body = r#"{"error":"invalid_grant","access_token":"DO_NOT_LOG","error_description":"DO_NOT_LOG"}"#;
            socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        };
        let http = crate::model::http_client().unwrap();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                exchange(
                    &http,
                    &endpoint,
                    &[("client_id", "oaiapp_test"), ("code", "one+two")]
                ),
                server
            )
        })
        .await
        .expect("token exchange test timed out");
        let error = result.err().unwrap().to_string();
        assert!(error.contains("invalid_grant"));
        assert!(!error.contains("DO_NOT_LOG"));
    }

    // Read the complete request body rather than waiting for a particular field.
    async fn read_form(socket: &mut TcpStream) -> BTreeMap<String, String> {
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "request ended before its body");
            bytes.extend_from_slice(&buffer[..count]);
            assert!(bytes.len() <= 16 * 1024, "unexpectedly large test request");
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&bytes[..end])
                    .unwrap()
                    .to_ascii_lowercase();
                assert!(headers.starts_with("post "));
                assert!(headers.contains("content-type: application/x-www-form-urlencoded"));
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .expect("missing request length")
                    .parse()
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    let body = std::str::from_utf8(&bytes[end + 4..end + 4 + length]).unwrap();
                    return Url::parse(&format!("http://localhost/?{body}"))
                        .unwrap()
                        .query_pairs()
                        .map(|(key, value)| (key.into_owned(), value.into_owned()))
                        .collect();
                }
            }
        }
    }

    #[tokio::test]
    async fn refresh_persists_rotated_credentials_and_reuses_them_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Some(dir.path().to_owned())).unwrap();
        {
            let _lock = store.lock().unwrap();
            let mut state = store.read().unwrap();
            state.accounts.insert(
                "test".into(),
                Account {
                    client_id: "oaiapp_test".into(),
                    subject: Some("user-123".into()),
                    email: None,
                    credentials: Some(Credentials {
                        access_token: "old-access".into(),
                        refresh_token: Some("old-refresh".into()),
                        id_token: None,
                        expires_at: 1,
                        scope: SCOPES.into(),
                    }),
                },
            );
            store.save(&state).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let form = read_form(&mut socket).await;
            assert_eq!(
                form,
                BTreeMap::from([
                    ("grant_type".into(), "refresh_token".into()),
                    ("client_id".into(), "oaiapp_test".into()),
                    ("refresh_token".into(), "old-refresh".into()),
                    ("resource".into(), RESOURCE.into()),
                ])
            );
            let body = r#"{"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","expires_in":3600}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        };
        let http = crate::model::http_client().unwrap();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(store.access_token_at(&http, "test", &endpoint), server)
        })
        .await
        .expect("refresh test timed out");
        assert_eq!(result.unwrap(), "new-access");
        drop(listener);
        let reloaded = Store::new(Some(dir.path().to_owned())).unwrap();
        let state = reloaded.read().unwrap();
        let saved = state.accounts["test"].credentials.as_ref().unwrap();
        assert_eq!(saved.access_token, "new-access");
        assert_eq!(saved.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(saved.scope, SCOPES);
        assert!(saved.expires_at > now().unwrap() + 60);
        // No listener remains: this must use the saved unexpired access token.
        assert_eq!(
            reloaded
                .access_token_at(&http, "test", &endpoint)
                .await
                .unwrap(),
            "new-access"
        );
    }
}
