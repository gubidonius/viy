//! RunPod's APIs: REST v2 (`api.runpod.io/v2`) for pods and network
//! volumes, GraphQL for the GPU stock, and the v2 platform log stream,
//! readable before ssh is. rpctl's `api.py`, which spoke REST v1
//! (`rest.runpod.io/v1`) until RunPod retired it on 2026-08-02 (kb f150):
//! viy speaks v2 on the wire and hands its callers v1's shapes, translated
//! in [`crate::wire`] (kb d151). The pod/api fixture pins the rest: the
//! errors, their text and what counts as gone.
//!
//! The safety invariant (rpctl docs/architecture.md 4): never confuse "API
//! unreachable" with "pod gone". Every failure is a [`RunPodError`]:
//!
//! | What happened | `status_code` | `transient()` | How callers read it |
//! |---|---|---|---|
//! | No answer: refused, reset, DNS, timeout | `None` | yes | unknown: keep what we had |
//! | HTTP 429, 500, 502, 503, 504 | the status | yes | unknown: keep what we had |
//! | Not sent: a 429 holds the client | 429 | yes | unknown: keep what we had |
//! | HTTP 404 | 404 | no | gone; [`RunPod::get_pod`] returns `Ok(None)` |
//! | Any other HTTP status from 400 | the status | no | refused (key, quota, request) |
//! | GraphQL answer with `errors` or without `data` | the status, often 200 | by the status, as above | refused |
//! | Log stream answers other than 200 | the status | by the status, as above | no platform log |
//!
//! The text is rpctl's, `"<METHOD> <path> -> HTTP <status>\n<detail>"`
//! (the detail is the JSON body pretty-printed, or the first 2000
//! characters of the text), except where `requests` named an exception
//! class: viy names its own error kind (flags.md F22), as in
//! `"GET /pods -> connection failed: ..."`.
//!
//! RunPod's quota is per user, in minute, hour and day windows, and a
//! request it refuses with 429 may count against it too
//! (https://docs.runpod.io/api-reference-v2/rate-limits). So a 429 holds
//! the client and every clone of it: until RunPod's `Retry-After` (or the
//! reset of the window its `RateLimit` header shows spent, or
//! [`RATE_LIMIT_FALLBACK`]) has passed, calls fail at once without being
//! sent, and the error says how long is left (kb d140). rpctl 573dbaa
//! polled on through a 429.

use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;
use viy_http::HttpClient;
use viy_http::HttpError;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;

use crate::regions::is_capacity_error;
use crate::wire;

pub const BASE: &str = "https://api.runpod.io/v2";
pub const GRAPHQL: &str = "https://api.runpod.io/graphql";
/// Pages a list may take before viy stops following its cursor: RunPod
/// pages 1000 rows at a time.
const MAX_PAGES: usize = 100;

/// How long [`RunPod::pod_logs`] reads when its caller has no budget of its
/// own: the stream never closes.
pub const LOG_BUDGET: Duration = Duration::from_secs(8);
const LOG_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A quiet gap this long ends the log stream.
const LOG_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// The deadline of a budget too long to add to a clock reading.
const FAR_FUTURE: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

/// How much of a non-JSON error body the message keeps.
const DETAIL_CHARS: usize = 2000;

/// How long a 429 holds the client when RunPod says nothing of when to
/// come back.
pub const RATE_LIMIT_FALLBACK: Duration = Duration::from_secs(60);

/// A failed RunPod call; see the module docs for the kinds.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct RunPodError {
    message: String,
    status_code: Option<u16>,
    /// After a 429: how long until RunPod takes requests again.
    retry_after: Option<Duration>,
}

impl RunPodError {
    pub fn new(message: impl Into<String>, status_code: Option<u16>) -> Self {
        Self {
            message: message.into(),
            status_code,
            retry_after: None,
        }
    }

    /// A 429, RunPod's or the client's hold: `message`, then when RunPod
    /// takes requests again.
    fn rate_limited(message: impl Into<String>, wait: Duration) -> Self {
        Self {
            message: format!(
                "{}\nRunPod's rate limit: it takes requests again in {}.",
                message.into(),
                spoken(wait)
            ),
            status_code: Some(429),
            retry_after: Some(wait),
        }
    }

    /// After a 429: how long until RunPod takes requests again.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// The HTTP status when RunPod answered, `None` when it did not.
    pub fn status_code(&self) -> Option<u16> {
        self.status_code
    }

    /// The answer may differ if asked again: no answer, or 429 or a 5xx
    /// from 500 to 504.
    pub fn transient(&self) -> bool {
        matches!(self.status_code, None | Some(429 | 500 | 502 | 503 | 504))
    }
}

/// One line of the platform log. Entries without a string `line` are
/// skipped; a `ts` or `source` that is not a string counts as absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogEntry {
    pub ts: Option<String>,
    pub source: Option<String>,
    pub line: String,
}

impl From<viy_provider::LogLine> for LogEntry {
    fn from(line: viy_provider::LogLine) -> Self {
        Self {
            ts: line.time,
            source: line.source,
            line: line.text,
        }
    }
}

impl LogEntry {
    /// When the line was written. System lines carry whole seconds
    /// ("...:52Z"), container lines nanoseconds ("...:52.771976788Z"):
    /// compared as text, "52Z" would come after "52.7Z".
    pub fn time(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let ts = self.ts.as_deref()?;
        chrono::DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|t| t.to_utc())
    }

    /// The order rpctl sorts by (`api.log_time(entry) or entry["ts"]`): the
    /// time to the nanosecond as text, else the timestamp as written.
    fn sort_key(&self) -> String {
        match self.time() {
            Some(time) => time.format("%Y-%m-%dT%H:%M:%S%.9f").to_string(),
            None => self.ts.clone().unwrap_or_default(),
        }
    }
}

/// The stock RunPod reports for one GPU in one data center: on a host whose
/// driver supports at least the minimum CUDA (`fit`), and on one whose
/// driver is exactly one of the older versions asked about (`old`). `None`
/// when none is free, or when no older versions were asked about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CudaStock {
    pub fit: Option<String>,
    pub old: Option<String>,
}

/// The client. Cheap to clone; every clone shares the HTTP connections.
#[derive(Clone)]
pub struct RunPod {
    http: Arc<dyn HttpClient>,
    api_key: String,
    user_agent: &'static str,
    /// Until when a 429 holds every call (module docs).
    held_until: Arc<Mutex<Option<Instant>>>,
}

/// Why a call has no response.
enum Unsent {
    /// The client's hold has this long left.
    Held(Duration),
    Http(HttpError),
}

/// The key stays out.
impl fmt::Debug for RunPod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunPod").finish_non_exhaustive()
    }
}

impl RunPod {
    pub fn new(api_key: impl Into<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            http,
            api_key: api_key.into(),
            user_agent: user_agent::PROVIDER_API,
            held_until: Arc::default(),
        }
    }

    /// How long the hold a 429 left still lasts, if it does.
    pub fn held_for(&self) -> Option<Duration> {
        let until = (*self
            .held_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner))?;
        let left = until.saturating_duration_since(Instant::now());
        (!left.is_zero()).then_some(left)
    }

    /// Sends `request` unless a hold is on; a 429 starts or extends one.
    async fn send(&self, request: Request) -> Result<viy_http::Response, Unsent> {
        if let Some(left) = self.held_for() {
            return Err(Unsent::Held(left));
        }
        let response = viy_http::send(&*self.http, request)
            .await
            .map_err(Unsent::Http)?;
        if response.status == 429 {
            let until = Instant::now() + retry_after(&response).unwrap_or(RATE_LIMIT_FALLBACK);
            let mut held = self
                .held_until
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *held = (*held).max(Some(until));
        }
        Ok(response)
    }

    /// The hold's time left for a 429 just received.
    fn wait_after_429(&self) -> Duration {
        self.held_for().unwrap_or(RATE_LIMIT_FALLBACK)
    }

    /// As [`RunPod::new`], with CLIENT_TAG off sending rp's User-Agent.
    pub fn configured(
        api_key: impl Into<String>,
        http: Arc<dyn HttpClient>,
        cfg: &viy_config::Config,
    ) -> Self {
        let mut this = Self::new(api_key, http);
        if !viy_config::config::client_tag(cfg) {
            this.user_agent = user_agent::RP_REQUESTS;
        }
        this
    }

    /// Every call's headers, rp's `requests.Session` ones in its order,
    /// without Accept-Encoding and Connection (kb d046), the User-Agent
    /// the provider marker unless CLIENT_TAG is off (kb d137); `requests`'
    /// default timeouts, [`viy_http::DEFAULT_TIMEOUT`].
    fn request(&self, method: Method, url: String) -> Request {
        Request::new(method, url)
            .header("User-Agent", self.user_agent)
            .header("Accept", "*/*")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
    }

    /// One REST call: the JSON it answers, `None` for an empty or non-JSON
    /// body (or JSON `null`).
    async fn rest(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Option<Value>, RunPodError> {
        let mut request = self.request(method.clone(), format!("{BASE}{path}"));
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        let request = with_content_length(request);
        let (status, bytes) = self.fetch(request).await.map_err(|unsent| match unsent {
            Unsent::Held(left) => {
                RunPodError::rate_limited(format!("{method} {path} -> not sent"), left)
            }
            Unsent::Http(error) => RunPodError::new(format!("{method} {path} -> {error}"), None),
        })?;
        if status >= 400 {
            let detail = match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
                Err(_) => text(&bytes),
            };
            let message = format!("{method} {path} -> HTTP {status}\n{detail}");
            if status == 429 {
                return Err(RunPodError::rate_limited(message, self.wait_after_429()));
            }
            return Err(RunPodError::new(message, Some(status)));
        }
        Ok(serde_json::from_slice(&bytes)
            .ok()
            .filter(|value: &Value| !value.is_null()))
    }

    /// Every row of the v2 list at `path`, following its pages; each row
    /// as `shape` makes it.
    async fn list(
        &self,
        path: &str,
        key: &str,
        shape: fn(Value) -> Value,
    ) -> Result<Vec<Value>, RunPodError> {
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let page_path = match &cursor {
                None => path.to_string(),
                Some(cursor) => format!("{path}?cursor={}", wire::query_value(cursor)),
            };
            let (page, next) = wire::page(self.rest(Method::GET, &page_path, None).await?, key);
            rows.extend(page.into_iter().map(shape));
            if next.is_none() || next == cursor {
                break;
            }
            cursor = next;
        }
        Ok(rows)
    }

    /// Asks RunPod to `action` the pod (`start`, `stop`). RunPod refuses an
    /// action the pod's state does not allow with 409; v1 took a start or
    /// stop the pod was already through, so a pod found as `done` (v1's
    /// `desiredStatus`) counts as acted on.
    async fn act(&self, pod_id: &str, action: &str, done: &str) -> Result<(), RunPodError> {
        let path = format!("/pods/{pod_id}/action");
        match self
            .rest(Method::POST, &path, Some(json!({ "action": action })))
            .await
        {
            Err(error) if error.status_code == Some(409) => match self.get_pod(pod_id).await {
                Ok(Some(pod)) if pod.get("desiredStatus").and_then(Value::as_str) == Some(done) => {
                    Ok(())
                }
                _ => Err(error),
            },
            result => result.map(drop),
        }
    }

    async fn fetch(&self, request: Request) -> Result<(u16, Vec<u8>), Unsent> {
        let response = self.send(request).await?;
        let status = response.status;
        Ok((status, response.bytes().await.map_err(Unsent::Http)?))
    }

    /// One GraphQL query: its `data`, an empty map when that is `null`.
    async fn graphql(&self, query: &str) -> Result<Map<String, Value>, RunPodError> {
        let request = with_content_length(
            self.request(Method::POST, GRAPHQL.to_string())
                .body(json!({ "query": query }).to_string()),
        );
        let (status, bytes) = self.fetch(request).await.map_err(|unsent| match unsent {
            Unsent::Held(left) => RunPodError::rate_limited("GraphQL -> not sent", left),
            Unsent::Http(error) => RunPodError::new(format!("GraphQL -> {error}"), None),
        })?;
        let mut body = match serde_json::from_slice(&bytes) {
            Ok(Value::Object(body)) => body,
            _ => Map::new(),
        };
        let errors = body.get("errors").filter(|errors| truthy(errors));
        if status >= 400 || errors.is_some() || !body.contains_key("data") {
            let detail = errors.map_or_else(|| text(&bytes), Value::to_string);
            let message = format!("GraphQL -> HTTP {status}\n{detail}");
            if status == 429 {
                return Err(RunPodError::rate_limited(message, self.wait_after_429()));
            }
            return Err(RunPodError::new(message, Some(status)));
        }
        Ok(match body.remove("data") {
            Some(Value::Object(data)) => data,
            _ => Map::new(),
        })
    }

    // ---- stock ----------------------------------------------------------

    /// Every data center with the GPU stock RunPod reports right now. Only
    /// the GraphQL API has it, and `available` flickers: sample it.
    pub async fn data_centers(&self) -> Result<Vec<Value>, RunPodError> {
        let mut data = self
            .graphql(
                "query { dataCenters { id location listed storageSupport \
                 gpuAvailability { gpuTypeId stockStatus available } } }",
            )
            .await?;
        Ok(rows(data.remove("dataCenters")))
    }

    /// The stock of each GPU type in each data center ([`CudaStock`]),
    /// keyed `(data center, GPU type)` in the order asked. One GraphQL
    /// request: `gpuAvailability` above has no CUDA filter, and REST's
    /// `allowedCudaVersions` stops at 13.0 and matches exactly.
    pub async fn cuda_stock(
        &self,
        data_centers: &[String],
        gpu_types: &[String],
        min_cuda: &str,
        older: &[String],
        secure: bool,
    ) -> Result<IndexMap<(String, String), CudaStock>, RunPodError> {
        let price = |gpu: &str, dc: &str, cond: &str| {
            format!(
                "gpuTypes(input: {{id: {}}}) {{ lowestPrice(input: {{gpuCount: 1, \
                 secureCloud: {secure}, dataCenterId: {}, {cond}}}) {{ stockStatus }} }}",
                quote(gpu),
                quote(dc),
            )
        };
        let fit = format!("minCudaVersion: {}", quote(min_cuda));
        let old = format!(
            "allowedCudaVersions: [{}]",
            older
                .iter()
                .map(|v| quote(v))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut keys = Vec::new();
        let mut parts = Vec::new();
        for (i, dc) in data_centers.iter().enumerate() {
            for (j, gpu) in gpu_types.iter().enumerate() {
                let alias = format!("s{i}_{j}");
                parts.push(format!("{alias}f: {}", price(gpu, dc, &fit)));
                if !older.is_empty() {
                    parts.push(format!("{alias}o: {}", price(gpu, dc, &old)));
                }
                keys.push((dc, gpu, alias));
            }
        }
        let data = self
            .graphql(&format!("query {{ {} }}", parts.join(" ")))
            .await?;
        let status = |alias: String| {
            data.get(&alias)
                .and_then(Value::as_array)
                .and_then(|rows| rows.first())
                .and_then(|row| row.get("lowestPrice"))
                .and_then(|price| price.get("stockStatus"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Ok(keys
            .into_iter()
            .map(|(dc, gpu, alias)| {
                let stock = CudaStock {
                    fit: status(format!("{alias}f")),
                    old: if older.is_empty() {
                        None
                    } else {
                        status(format!("{alias}o"))
                    },
                };
                ((dc.clone(), gpu.clone()), stock)
            })
            .collect())
    }

    // ---- network volumes ------------------------------------------------

    /// Every network volume, as v1 showed them (`dataCenterId`).
    pub async fn list_volumes(&self) -> Result<Vec<Value>, RunPodError> {
        self.list("/network-volumes", "networkVolumes", wire::volume)
            .await
    }

    pub async fn get_volume(&self, volume_id: &str) -> Result<Option<Value>, RunPodError> {
        let volume = self
            .rest(Method::GET, &format!("/network-volumes/{volume_id}"), None)
            .await?;
        Ok(volume.map(wire::volume))
    }

    /// A new volume. With no `tier` RunPod takes the data center's default
    /// storage tier; `STANDARD` or `HIGH_PERFORMANCE` asks for that one.
    pub async fn create_volume(
        &self,
        name: &str,
        size_gb: u64,
        data_center_id: &str,
        tier: Option<&str>,
    ) -> Result<Option<Value>, RunPodError> {
        let mut body = json!({ "name": name, "size": size_gb, "dataCenter": data_center_id });
        if let Some(tier) = tier {
            body["type"] = json!(tier);
        }
        let volume = self
            .rest(Method::POST, "/network-volumes", Some(body))
            .await?;
        Ok(volume.map(wire::volume))
    }

    pub async fn delete_volume(&self, volume_id: &str) -> Result<(), RunPodError> {
        self.rest(
            Method::DELETE,
            &format!("/network-volumes/{volume_id}"),
            None,
        )
        .await
        .map(drop)
    }

    // ---- pods -----------------------------------------------------------

    /// Every pod, as v1 showed them ([`crate::wire::pod`]). A listed pod
    /// carries no SSH mapping; [`RunPod::get_pod`]'s does.
    pub async fn list_pods(&self) -> Result<Vec<Value>, RunPodError> {
        self.list("/pods", "pods", wire::pod).await
    }

    /// The pod as v1 showed it, or `None` when RunPod says 404 (or answers
    /// with no body).
    pub async fn get_pod(&self, pod_id: &str) -> Result<Option<Value>, RunPodError> {
        match self
            .rest(Method::GET, &format!("/pods/{pod_id}"), None)
            .await
        {
            Err(error) if error.status_code == Some(404) => Ok(None),
            result => result.map(|pod| pod.map(wire::pod)),
        }
    }

    /// A pod from rpctl's v1 create body (`viy_vllm::build_pod_body`):
    /// one v2 request per GPU type of its `gpuTypeIds`, in order, until one
    /// is placed. A type refused for capacity (400) or not open to this
    /// account (403) passes to the next, as RunPod's v2 guide has it; any
    /// other refusal ends the search. With every type refused, the error is
    /// the last capacity refusal, so that failover reads it as v1's one
    /// "no instances" answer, else the last refusal.
    pub async fn create_pod(&self, body: Value) -> Result<Option<Value>, RunPodError> {
        let mut refusal: Option<RunPodError> = None;
        // Never empty: a body without types is still one request.
        for request in wire::pod_requests(&body) {
            let error = match self.rest(Method::POST, "/pods", Some(request)).await {
                Ok(pod) => return Ok(pod.map(wire::pod)),
                Err(error) => error,
            };
            let full = is_capacity_error(&error);
            if !full && error.status_code != Some(403) {
                return Err(error);
            }
            if full || !refusal.as_ref().is_some_and(is_capacity_error) {
                refusal = Some(error);
            }
        }
        refusal.map_or(Ok(None), Err)
    }

    pub async fn stop_pod(&self, pod_id: &str) -> Result<(), RunPodError> {
        self.act(pod_id, "stop", "EXITED").await
    }

    pub async fn start_pod(&self, pod_id: &str) -> Result<(), RunPodError> {
        self.act(pod_id, "start", "RUNNING").await
    }

    pub async fn delete_pod(&self, pod_id: &str) -> Result<(), RunPodError> {
        self.rest(Method::DELETE, &format!("/pods/{pod_id}"), None)
            .await
            .map(drop)
    }

    /// The last `tail` lines of the pod's platform log, oldest first: a
    /// bounded read of the v2 SSE stream, which never closes. Only log
    /// payloads count, not keepalives. It stops after a quiet gap of 2 s,
    /// and fails if no entry had come by then; and it stops at the end of
    /// `budget`, once anything at all arrives after it (rpctl looks at the
    /// budget when a line comes), or at once if entries are in hand.
    pub async fn pod_logs(
        &self,
        pod_id: &str,
        tail: u32,
        budget: Duration,
    ) -> Result<Vec<LogEntry>, RunPodError> {
        let start = Instant::now();
        let deadline = start.checked_add(budget).unwrap_or(start + FAR_FUTURE);
        let request = self
            .request(
                Method::GET,
                format!("{BASE}/pods/{pod_id}/logs?tail={tail}"),
            )
            .timeouts(LOG_CONNECT_TIMEOUT, LOG_READ_TIMEOUT);
        let unavailable = |error: HttpError| {
            RunPodError::new(format!("Platform logs unavailable ({error})"), None)
        };
        let mut response = self.send(request).await.map_err(|unsent| match unsent {
            Unsent::Held(left) => RunPodError::rate_limited("Platform logs -> not sent", left),
            Unsent::Http(error) => unavailable(error),
        })?;
        if response.status != 200 {
            let status = response.status;
            let message = format!("Platform logs -> HTTP {status}");
            if status == 429 {
                return Err(RunPodError::rate_limited(message, self.wait_after_429()));
            }
            return Err(RunPodError::new(message, Some(status)));
        }
        let mut entries = Vec::new();
        let mut pending = Vec::new();
        'read: loop {
            let mut next = std::pin::pin!(response.chunk());
            let read = match tokio::time::timeout_at(deadline, &mut next).await {
                Ok(read) => read,
                // Nothing in hand: rpctl still waits for the next line or
                // the quiet gap.
                Err(_budget_spent) if entries.is_empty() => match next.await {
                    Err(error) => return Err(unavailable(error)),
                    Ok(_) => break,
                },
                Err(_budget_spent) => break,
            };
            let chunk = match read {
                Ok(Some(chunk)) => chunk,
                Ok(None) => {
                    // The last line may have no line break.
                    if Instant::now() < deadline {
                        entries.extend(log_entry(&pending));
                    }
                    break;
                }
                Err(error) if entries.is_empty() => return Err(unavailable(error)),
                Err(_) => break,
            };
            // Lines end at \n, \r or \r\n, as `bytes.splitlines` has them; the
            // empty line inside a \r\n is no `data:` line.
            let mut line_start = 0;
            let mut scan = pending.len();
            pending.extend(chunk);
            while let Some(offset) = pending[scan..]
                .iter()
                .position(|&b| b == b'\n' || b == b'\r')
            {
                let end = scan + offset;
                if Instant::now() >= deadline {
                    break 'read;
                }
                entries.extend(log_entry(&pending[line_start..end]));
                line_start = end + 1;
                scan = line_start;
            }
            pending.drain(..line_start);
        }
        entries.sort_by_cached_key(LogEntry::sort_key);
        Ok(entries)
    }
}

/// RunPod fronts every exposed http port at this hostname.
pub fn proxy_url(pod_id: &str, port: impl fmt::Display) -> String {
    format!("https://{pod_id}-{port}.proxy.runpod.net")
}

/// A list GraphQL answers, bare or as `{"data": [...]}`; anything else is
/// no rows.
fn rows(value: Option<Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(rows)) => rows,
        Some(Value::Object(mut wrapper)) => match wrapper.remove("data") {
            Some(Value::Array(rows)) => rows,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// One SSE line as a log entry, if it is a `data:` line holding one.
fn log_entry(line: &[u8]) -> Option<LogEntry> {
    let data = line.strip_prefix(b"data:")?;
    let Ok(Value::Object(entry)) = serde_json::from_slice(data) else {
        return None;
    };
    let field = |key| entry.get(key).and_then(Value::as_str).map(str::to_string);
    Some(LogEntry {
        line: field("line")?,
        ts: field("ts"),
        source: field("source"),
    })
}

/// rp's `requests` declares a length on every request with a body and a
/// zero one on a bodiless request other than GET and HEAD; hyper leaves the
/// header off when there is no body.
/// When RunPod takes requests again after this 429: its `Retry-After` in
/// seconds, else the largest reset among the windows its `RateLimit` header
/// shows spent (`"hour";r=0;t=1812, ...`).
fn retry_after(response: &viy_http::Response) -> Option<Duration> {
    if let Some(seconds) = response
        .header("retry-after")
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_secs(seconds));
    }
    response
        .header("ratelimit")?
        .split(',')
        .filter_map(|window| {
            let mut remaining = None;
            let mut reset = None;
            for param in window.split(';').skip(1) {
                match param.trim().split_once('=') {
                    Some(("r", value)) => remaining = value.trim().parse::<u64>().ok(),
                    Some(("t", value)) => reset = value.trim().parse::<u64>().ok(),
                    _ => {}
                }
            }
            (remaining == Some(0)).then_some(reset).flatten()
        })
        .max()
        .map(Duration::from_secs)
}

/// A wait as people say it: "45s", "12m 5s", "1h 2m".
pub fn spoken(wait: Duration) -> String {
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    match (secs / 3600, secs % 3600 / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

fn with_content_length(request: Request) -> Request {
    let length = match &request.body {
        Some(body) => body.len(),
        None if request.method == Method::GET || request.method == Method::HEAD => {
            return request;
        }
        None => 0,
    };
    request.header("Content-Length", length.to_string())
}

/// A body as text, for an error message.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(DETAIL_CHARS)
        .collect()
}

/// A string as a GraphQL (and JSON) string literal.
fn quote(s: &str) -> String {
    Value::from(s).to_string()
}

/// A JSON value as Python's `str()` shows it in rpctl's messages and
/// listings: `None`, `True`, strings bare.
pub(crate) fn shown(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(other) => other.to_string(),
    }
}

/// Python's truth of a JSON value, as rp tests one: not null, false, 0,
/// or empty. GraphQL's `errors`, and pod.json's `tunnel_only` and
/// `tunnel_pid`.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
