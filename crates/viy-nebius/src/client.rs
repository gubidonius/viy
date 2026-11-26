//! Authenticated JSON requests against Nebius' published OpenAPI contract.
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::Mutex;
use tokio::sync::Semaphore;
use viy_config::vault::Tok;
use viy_http::{HttpClient, Method, Request, user_agent};
use viy_provider::{ProviderError, Result};

pub(crate) fn error(message: impl ToString) -> ProviderError {
    ProviderError {
        message: message.to_string(),
        status: None,
        transient: false,
        capacity: false,
        retry_after: None,
    }
}

pub(crate) fn field<'a>(v: &'a Value, path: &str) -> &'a str {
    v.pointer(path).and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn number(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceKey {
    pub vault: String,
    pub account_id: String,
    pub public_key_id: String,
}

pub(crate) struct Client {
    pub http: Arc<dyn HttpClient>,
    pub profile: String,
    pub service_key: Option<ServiceKey>,
    pub terminal_login: bool,
    token: Mutex<Option<(Instant, String)>>,
    refresh: Semaphore,
}

impl Client {
    pub fn new(
        http: Arc<dyn HttpClient>,
        profile: String,
        service_key: Option<ServiceKey>,
    ) -> Self {
        Self {
            http,
            profile,
            service_key,
            terminal_login: false,
            token: Mutex::default(),
            refresh: Semaphore::new(1),
        }
    }

    pub async fn token(&self, rejected: Option<&str>) -> Result<String> {
        // Share one refresh (and any browser prompt) among simultaneous API requests.
        let _permit = self.refresh.acquire().await.map_err(error)?;
        if let Some((until, token)) = self
            .token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            && *until > Instant::now()
            && rejected != Some(token.as_str())
        {
            return Ok(token.clone());
        }
        let (token, ttl) = if let Some(key) = self.service_key.clone() {
            let jwt = tokio::task::spawn_blocking(move || assertion(&key))
                .await
                .map_err(error)??;
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair(
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:token-exchange",
                )
                .append_pair(
                    "requested_token_type",
                    "urn:ietf:params:oauth:token-type:access_token",
                )
                .append_pair("subject_token_type", "urn:ietf:params:oauth:token-type:jwt")
                .append_pair("subject_token", &jwt)
                .finish();
            let request = Request::new(
                Method::POST,
                "https://auth.eu.nebius.com/oauth2/token/exchange",
            )
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", user_agent::PROVIDER_API)
            .body(body);
            let data = self.send(request).await?;
            (
                field(&data, "/access_token").to_owned(),
                number(&data["expires_in"]).unwrap_or(300),
            )
        } else {
            // The CLI writes the browser link to stderr and the bearer token to stdout.
            let limit = if self.terminal_login { 300 } else { 20 };
            let mut cmd = tokio::process::Command::new("nebius");
            cmd.args([
                "--profile",
                &self.profile,
                "--no-browser",
                "--no-check-update",
                "--auth-timeout",
                &format!("{limit}s"),
                "iam",
                "get-access-token",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(if self.terminal_login {
                Stdio::inherit()
            } else {
                Stdio::piped()
            })
            .kill_on_drop(true);
            let child = cmd
                .spawn()
                .map_err(|e| error(format!("Could not run the Nebius CLI: {e}")))?;
            let out = tokio::time::timeout(Duration::from_secs(limit + 5), child.wait_with_output())
                .await
                .map_err(|_| error(if self.terminal_login {
                    "Nebius sign-in timed out. Retry this command for a new sign-in link."
                } else {
                    "Nebius authentication timed out. Run viy provider nebius in a terminal and open the sign-in link it prints."
                }))?
                .map_err(|e| error(format!("Could not run the Nebius CLI: {e}")))?;
            if !out.status.success() {
                if !self.terminal_login
                    && String::from_utf8_lossy(&out.stderr)
                        .contains("open the following link in your browser")
                {
                    return Err(error(
                        "Nebius requires browser sign-in. Run viy provider nebius in a terminal and open the link it prints.",
                    ));
                }
                return Err(error(format!(
                    "Nebius authentication failed for CLI profile '{}'",
                    self.profile
                )));
            }
            (
                String::from_utf8(out.stdout)
                    .map_err(|_| error("invalid Nebius access token"))?
                    .trim()
                    .to_string(),
                300,
            )
        };
        if token.is_empty() || token.bytes().any(|c| c.is_ascii_whitespace()) {
            return Err(error("invalid Nebius access token"));
        }
        *self
            .token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((
            Instant::now() + Duration::from_secs(ttl.saturating_sub(30).min(3600)),
            token.clone(),
        ));
        Ok(token)
    }

    async fn send(&self, req: Request) -> Result<Value> {
        let res = viy_http::send(&*self.http, req)
            .await
            .map_err(|e| ProviderError {
                transient: true,
                ..error(e)
            })?;
        let status = res.status;
        let retry_after = res
            .header("Retry-After")
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs);
        let bytes = res.bytes().await.map_err(error)?;
        let data: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let mut e = api_error(&data, status);
            e.retry_after = retry_after;
            return Err(e);
        }
        if data.is_null() {
            return Err(error("Nebius returned an empty or invalid JSON response"));
        }
        Ok(data)
    }

    pub async fn request(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        self.request_key(method, path, body, None).await
    }

    pub async fn request_key(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        key: Option<&str>,
    ) -> Result<Value> {
        let mut token = self.token(None).await?;
        for attempt in 0..2 {
            let mut req = Request::new(method.clone(), format!("https://api.nebius.cloud/{path}"))
                .header("User-Agent", user_agent::PROVIDER_API)
                .header("Authorization", format!("Bearer {token}"));
            if let Some(key) = key {
                req = req.header("X-Idempotency-Key", key);
            }
            if let Some(body) = body {
                req = req
                    .header("Content-Type", "application/json")
                    .body(body.to_string());
            }
            match self.send(req).await {
                Err(e) if e.status == Some(401) && attempt == 0 => {
                    token = self.token(Some(&token)).await?
                }
                Err(mut e) if matches!(e.status, Some(401 | 403)) => {
                    let identity = self
                        .service_key
                        .as_ref()
                        .map_or(self.profile.as_str(), |s| s.account_id.as_str());
                    let scope = body
                        .and_then(|v| v.pointer("/metadata/parentId"))
                        .and_then(Value::as_str)
                        .map_or_else(String::new, |p| format!(", parent {p}"));
                    e.message =
                        format!("{} (identity {identity}, request {path}{scope})", e.message);
                    return Err(e);
                }
                result => return result,
            }
        }
        Err(error("Nebius rejected refreshed credentials"))
    }

    pub async fn list(&self, resource: &str, parent: Option<&str>) -> Result<Vec<Value>> {
        self.paged(resource, "parentId", parent).await
    }

    pub async fn list_member_of(&self, account: &str) -> Result<Vec<Value>> {
        self.paged(
            "iam/v1/group-memberships/list-member-of",
            "subjectId",
            Some(account),
        )
        .await
    }

    async fn paged(&self, resource: &str, key: &str, parent: Option<&str>) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut pages = HashSet::new();
        let mut next = String::new();
        loop {
            let query = {
                let mut q = url::form_urlencoded::Serializer::new(String::new());
                if let Some(parent) = parent {
                    q.append_pair(key, parent);
                }
                if !next.is_empty() {
                    q.append_pair("pageToken", &next);
                }
                q.finish()
            };
            let data = self
                .request(Method::GET, &format!("{resource}?{query}"), None)
                .await?;
            let collection = if resource == "iam/v1/group-memberships/list-members" {
                "memberships"
            } else {
                "items"
            };
            if let Some(rows) = data.get(collection) {
                items.extend(
                    rows.as_array()
                        .ok_or_else(|| error("invalid Nebius list response"))?
                        .iter()
                        .cloned(),
                );
            }
            next = field(&data, "/nextPageToken").to_string();
            if next.is_empty() {
                return Ok(items);
            }
            if !pages.insert(next.clone()) {
                return Err(error("Nebius repeated a pagination token"));
            }
        }
    }
}

pub(crate) fn api_error(data: &Value, status: u16) -> ProviderError {
    let message = field(data, "/message");
    let lower = message.to_lowercase();
    let details = data["details"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let quota = details.iter().any(|d| d.get("quotaFailure").is_some());
    let exhausted = details
        .iter()
        .any(|d| d.get("notEnoughResources").is_some());
    let capacity = !quota
        && !lower.contains("quota")
        && (exhausted
            || [
                "insufficient capacity",
                "not enough resources",
                "no available capacity",
                "insufficient gpu",
            ]
            .iter()
            .any(|s| lower.contains(s)));
    ProviderError {
        status: Some(status),
        capacity,
        transient: status == 429 || status >= 500,
        retry_after: None,
        message: format!(
            "Nebius HTTP {status}: {}",
            if message.is_empty() {
                "request failed"
            } else {
                message
            }
        ),
    }
}

fn assertion(key: &ServiceKey) -> Result<String> {
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
    let pem = Tok::from_env().open_token(&key.vault).map_err(error)?;
    let der = STANDARD
        .decode(
            pem.lines()
                .filter(|s| !s.starts_with("-----"))
                .collect::<String>(),
        )
        .map_err(|_| error("invalid RSA PEM in vault"))?;
    let pair = RsaKeyPair::from_pkcs8(&der)
        .or_else(|_| RsaKeyPair::from_der(&der))
        .map_err(|_| error("unsupported RSA key in vault"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(error)?
        .as_secs();
    let encode = |v: Value| URL_SAFE_NO_PAD.encode(v.to_string());
    let signed = format!(
        "{}.{}",
        encode(json!({"alg":"RS256","typ":"JWT","kid":key.public_key_id})),
        encode(json!({"iat":now,"exp":now+300,"iss":key.account_id,"sub":key.account_id}))
    );
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signed.as_bytes(),
        &mut signature,
    )
    .map_err(|_| error("Nebius JWT signing failed"))?;
    Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

#[cfg(test)]
impl Client {
    pub(crate) fn synthetic_token(&self) {
        *self.token.lock().unwrap() =
            Some((Instant::now() + Duration::from_secs(60), "fixture".into()));
    }
}
