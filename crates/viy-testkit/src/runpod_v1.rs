//! RunPod's retired REST v1 in front of viy's v2 client, for tests. rpctl's
//! fixtures, and the tests ported from rpctl, script RunPod in v1's URLs and
//! shapes (`rest.runpod.io/v1`), which RunPod retired on 2026-08-02; viy
//! now speaks v2 and reads v2's answers as v1's (viy-pod's `wire`). [`V1`]
//! wraps such a script: each v2 request reaches it as the v1 request rpctl
//! sent for the same thing, and its v1 answer comes back in v2's shapes.
//! [`V1::sent`] lists the requests as v1 ones, for comparing with rpctl's.
//!
//! One v1 create asked for every GPU type at once; v2 asks for one type
//! per request. The requests of one create (the same body, each with a
//! type not asked for yet, one right after another) are one v1 request:
//! the script answers the first, and a refusal of it stands for the
//! others. Its v1 request lists every type asked for, or, given the
//! [`V1::asking_together`] groups, the whole group of the first type, as
//! rpctl asked for it; a type of another group starts another request.
//!
//! Requests elsewhere (GraphQL, the v2 log stream, vLLM) pass through as
//! they are.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_http::Body;
use viy_http::BoxFuture;
use viy_http::HttpClient;
use viy_http::HttpError;
use viy_http::Method;
use viy_http::Request;
use viy_http::Response;

/// viy's REST base.
pub const V2_BASE: &str = "https://api.runpod.io/v2";
/// rpctl's, as its fixtures have it.
pub const V1_BASE: &str = "https://rest.runpod.io/v1";

/// An [`HttpClient`] that plays a v1 script to a v2 client (module docs).
pub struct V1 {
    inner: Arc<dyn HttpClient>,
    /// GPU types one v1 create asked for together.
    groups: Vec<Vec<Value>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Every request, as v1.
    sent: Vec<Request>,
    /// The create the last request belonged to, if it was one.
    create: Option<Create>,
}

/// One v1 create, while its v2 requests come.
struct Create {
    /// The v2 body without its type.
    key: Value,
    types: Vec<Value>,
    /// Where its v1 request is in `sent`.
    at: usize,
    /// The script's refusal, which stands for every type.
    refusal: Option<Answer>,
}

#[derive(Clone)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// What a v2 request asks for, in v1's terms.
enum Call {
    ListPods,
    CreatePod(Value),
    /// GET or DELETE of one pod.
    Pod(String),
    /// `start` or `stop`.
    Action(String, String),
    ListVolumes,
    CreateVolume(Value),
    /// GET or DELETE of one volume.
    Volume(String),
}

impl V1 {
    pub fn new(inner: Arc<dyn HttpClient>) -> Self {
        Self {
            inner,
            groups: Vec::new(),
            state: Mutex::default(),
        }
    }

    /// The GPU types rpctl asked for in one create, group by group (all of
    /// GPU_TYPE_IDS, or GPU_PREFERRED's and the rest).
    pub fn asking_together(mut self, groups: impl IntoIterator<Item = Vec<String>>) -> Self {
        self.groups = groups
            .into_iter()
            .map(|group| group.into_iter().map(Value::from).collect())
            .collect();
        self
    }

    fn group_of(&self, gpu: &Value) -> Option<&Vec<Value>> {
        self.groups.iter().find(|group| group.contains(gpu))
    }

    /// Every request so far, the RunPod REST ones as v1 requests.
    pub fn sent(&self) -> Vec<Request> {
        self.lock().sent.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn play(&self, request: Request) -> Result<Response, HttpError> {
        let Some(call) = Call::of(&request) else {
            {
                let mut state = self.lock();
                state.create = None;
                state.sent.push(request.clone());
            }
            return self.inner.request(request).await;
        };
        if let Call::CreatePod(body) = &call
            && let Some(answer) = self.joins_create(body)
        {
            return Ok(answer.response());
        }
        let mut v1 = call.v1_request(&request);
        if let Call::CreatePod(body) = &call
            && let Some(group) = gpu_type(body).and_then(|gpu| self.group_of(&gpu))
        {
            v1 = with_gpu_types(v1, group);
        }
        {
            let mut state = self.lock();
            state.create = match &call {
                Call::CreatePod(body) => Some(Create {
                    key: without_type(body),
                    types: gpu_type(body).into_iter().collect(),
                    at: state.sent.len(),
                    refusal: None,
                }),
                _ => None,
            };
            state.sent.push(v1.clone());
        }
        let response = self.inner.request(v1).await?;
        let translate = response.status < 400 && call.answers_with_json();
        if !translate && !matches!(call, Call::CreatePod(_)) {
            return Ok(response);
        }
        let status = response.status;
        let headers = response.headers().to_vec();
        let body = response.bytes().await?;
        let answer = Answer {
            status,
            headers,
            body: if translate {
                call.v2_answer(body)
            } else {
                body
            },
        };
        if status >= 400
            && let Some(create) = self.lock().create.as_mut()
        {
            create.refusal = Some(answer.clone());
        }
        Ok(answer.response())
    }

    /// The script's refusal again, when `body` is the next type of the
    /// create refused just before.
    fn joins_create(&self, body: &Value) -> Option<Answer> {
        let mut state = self.lock();
        let State { sent, create } = &mut *state;
        let create = create.as_mut()?;
        let gpu = gpu_type(body)?;
        let first = create.types.first()?;
        if create.key != without_type(body)
            || create.types.contains(&gpu)
            || self.group_of(&gpu) != self.group_of(first)
        {
            return None;
        }
        let refusal = create.refusal.clone()?;
        create.types.push(gpu);
        if self.groups.is_empty() {
            let request = &mut sent[create.at];
            *request = with_gpu_types(request.clone(), &create.types);
        }
        Some(refusal)
    }
}

impl HttpClient for V1 {
    fn request(&self, request: Request) -> BoxFuture<'_, Result<Response, HttpError>> {
        Box::pin(self.play(request))
    }
}

impl std::fmt::Debug for V1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V1").finish_non_exhaustive()
    }
}

impl Answer {
    fn response(self) -> Response {
        Response::new(self.status, Once(Some(self.body))).with_headers(self.headers)
    }
}

impl Call {
    fn of(request: &Request) -> Option<Self> {
        let path = request.url.strip_prefix(V2_BASE)?;
        let path = path.split_once('?').map_or(path, |(path, _)| path);
        let body = || -> Value {
            request
                .body
                .as_deref()
                .and_then(|b| serde_json::from_slice(b).ok())
                .unwrap_or(Value::Null)
        };
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let method = &request.method;
        Some(match parts.as_slice() {
            ["pods"] if method == Method::GET => Self::ListPods,
            ["pods"] if method == Method::POST => Self::CreatePod(body()),
            ["pods", id] => Self::Pod((*id).to_string()),
            ["pods", id, "action"] => {
                let action = body()["action"].as_str().unwrap_or_default().to_string();
                Self::Action((*id).to_string(), action)
            }
            ["network-volumes"] if method == Method::GET => Self::ListVolumes,
            ["network-volumes"] if method == Method::POST => Self::CreateVolume(body()),
            ["network-volumes", id] => Self::Volume((*id).to_string()),
            _ => return None,
        })
    }

    /// The v1 request rpctl sent for this: its URL, body and length.
    fn v1_request(&self, v2: &Request) -> Request {
        let (path, body) = match self {
            Self::ListPods => ("/pods".to_string(), None),
            Self::CreatePod(body) => ("/pods".to_string(), Some(pod_body(body))),
            Self::Pod(id) => (format!("/pods/{id}"), None),
            Self::Action(id, action) => (format!("/pods/{id}/{action}"), None),
            Self::ListVolumes => ("/networkvolumes".to_string(), None),
            Self::CreateVolume(body) => ("/networkvolumes".to_string(), Some(volume_body(body))),
            Self::Volume(id) => (format!("/networkvolumes/{id}"), None),
        };
        let mut request = v2.clone();
        request.url = format!("{V1_BASE}{path}");
        with_body(request, body.map(|b| b.to_string().into_bytes()))
    }

    fn answers_with_json(&self) -> bool {
        !matches!(self, Self::Action(..))
    }

    /// The script's v1 answer in v2's shape; a body that is not one as
    /// it is.
    fn v2_answer(&self, body: Vec<u8>) -> Vec<u8> {
        let Ok(v1) = serde_json::from_slice::<Value>(&body) else {
            return body;
        };
        let v2 = match (self, v1) {
            (Self::ListPods, rows) => match list(rows) {
                Some(rows) => page("pods", rows.into_iter().map(v2_pod).collect()),
                None => return body,
            },
            (Self::ListVolumes, rows) => match list(rows) {
                Some(rows) => page("networkVolumes", rows.into_iter().map(v2_volume).collect()),
                None => return body,
            },
            (Self::CreatePod(_) | Self::Pod(_), pod @ Value::Object(_)) => v2_pod(pod),
            (Self::CreateVolume(_) | Self::Volume(_), volume @ Value::Object(_)) => {
                v2_volume(volume)
            }
            _ => return body,
        };
        v2.to_string().into_bytes()
    }
}

/// `request` with this body, and the length rp's `requests` declares: the
/// body's, zero for a bodiless call other than GET and HEAD.
fn with_body(mut request: Request, body: Option<Vec<u8>>) -> Request {
    request
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("Content-Length"));
    let length = match &body {
        Some(body) => Some(body.len()),
        None if request.method == Method::GET || request.method == Method::HEAD => None,
        None => Some(0),
    };
    if let Some(length) = length {
        request
            .headers
            .push(("Content-Length".to_string(), length.to_string()));
    }
    request.body = body;
    request
}

/// A v1 create request asking for `types`.
fn with_gpu_types(request: Request, types: &[Value]) -> Request {
    let Some(mut v1) = request
        .body
        .as_deref()
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
    else {
        return request;
    };
    v1["gpuTypeIds"] = json!(types);
    with_body(request, Some(v1.to_string().into_bytes()))
}

fn gpu_type(body: &Value) -> Option<Value> {
    body.get("gpu")?.get("id").cloned()
}

fn without_type(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(gpu) = body.get_mut("gpu").and_then(Value::as_object_mut) {
        gpu.shift_remove("id");
    }
    body
}

/// A v2 create body as rpctl's v1 one, keys in rpctl's order; v1 fields
/// v2 has nothing for (`computeType`, `gpuTypePriority`) are not there.
fn pod_body(v2: &Value) -> Value {
    let gpu = v2.get("gpu");
    let network = v2.get("mounts").and_then(|m| m.pointer("/network/0"));
    let mut v1 = Map::new();
    let mut put = |key: &str, value: Option<&Value>| {
        if let Some(value) = value {
            v1.insert(key.to_string(), value.clone());
        }
    };
    put("name", v2.get("name"));
    put("imageName", v2.get("image"));
    put("cloudType", v2.get("cloud"));
    put("gpuCount", gpu.and_then(|g| g.get("count")));
    put(
        "gpuTypeIds",
        gpu.and_then(|g| g.get("id")).map(|id| json!([id])).as_ref(),
    );
    put("dataCenterIds", v2.get("dataCenterIds"));
    put("networkVolumeId", network.and_then(|n| n.get("volumeId")));
    put("volumeMountPath", network.and_then(|n| n.get("path")));
    put("containerDiskInGb", v2.get("disk"));
    put("ports", v2.get("ports"));
    put("env", v2.get("env"));
    put("dockerEntrypoint", v2.get("entrypoint"));
    put("dockerStartCmd", v2.get("cmd"));
    put(
        "allowedCudaVersions",
        gpu.and_then(|g| g.get("allowedCudaVersions")),
    );
    Value::Object(v1)
}

/// A v2 volume body as v1's: `dataCenterId` for `dataCenter`. v1 took no
/// storage tier; a `type` is kept, for tests of it.
fn volume_body(v2: &Value) -> Value {
    rename(v2.clone(), "dataCenter", "dataCenterId")
}

/// A v1 list, bare or as `{"data": [...]}`.
fn list(value: Value) -> Option<Vec<Value>> {
    match value {
        Value::Array(rows) => Some(rows),
        Value::Object(mut wrapper) => match wrapper.remove("data") {
            Some(Value::Array(rows)) => Some(rows),
            _ => None,
        },
        _ => None,
    }
}

fn page(key: &str, rows: Vec<Value>) -> Value {
    json!({ key: rows, "pagination": { "nextCursor": null, "hasNextPage": false } })
}

/// A v1 pod as v2 has it: only what v2 still carries.
pub fn v2_pod(v1: Value) -> Value {
    let Value::Object(v1) = v1 else {
        return v1;
    };
    let mut v2 = Map::new();
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value {
            v2.insert(key.to_string(), value);
        }
    };
    put("id", v1.get("id").cloned());
    put("name", v1.get("name").cloned());
    put("status", v1.get("desiredStatus").cloned());
    put("cost", v1.get("costPerHr").cloned());
    let gpu_id = v1.get("gpu").and_then(|g| g.get("id")).cloned();
    let count = v1.get("gpuCount").cloned();
    if gpu_id.is_some() || count.is_some() {
        let mut gpu = Map::new();
        gpu.extend(gpu_id.map(|id| ("id".to_string(), id)));
        gpu.extend(count.map(|count| ("count".to_string(), count)));
        put("gpu", Some(Value::Object(gpu)));
    }
    put(
        "dataCenterId",
        v1.get("machine")
            .and_then(|m| m.get("dataCenterId"))
            .cloned(),
    );
    let port = v1
        .get("portMappings")
        .and_then(|p| p.get("22"))
        .and_then(|p| match p {
            Value::String(s) => s.parse::<u64>().ok().map(Value::from),
            other => Some(other.clone()),
        });
    if let (Some(host), Some(port)) = (v1.get("publicIp").filter(|ip| ip != &""), port) {
        put(
            "ssh",
            Some(json!({ "proxy": null, "direct": { "host": host, "port": port } })),
        );
    }
    put("startedAt", v1.get("lastStartedAt").cloned());
    put("createdAt", v1.get("createdAt").cloned());
    let volume = v1
        .get("networkVolumeId")
        .filter(|id| !id.is_null() && id != &"")
        .or_else(|| v1.get("networkVolume").and_then(|v| v.get("id")))
        .cloned();
    put(
        "mounts",
        volume.map(|id| json!({ "network": [{ "volumeId": id, "path": "/workspace" }] })),
    );
    Value::Object(v2)
}

/// A v1 volume as v2 has it.
pub fn v2_volume(v1: Value) -> Value {
    rename(v1, "dataCenterId", "dataCenter")
}

fn rename(value: Value, from: &str, to: &str) -> Value {
    let Value::Object(map) = value else {
        return value;
    };
    Value::Object(
        map.into_iter()
            .map(|(key, value)| {
                if key == from {
                    (to.to_string(), value)
                } else {
                    (key, value)
                }
            })
            .collect(),
    )
}

/// rpctl's error text with viy's v2 paths: `/networkvolumes` is
/// `/network-volumes`, a start or stop is `/action`.
pub fn v2_paths(text: &str) -> String {
    text.replace("/networkvolumes", "/network-volumes")
        .replace("/stop ->", "/action ->")
        .replace("/start ->", "/action ->")
}

/// A body that comes in one piece.
struct Once(Option<Vec<u8>>);

impl Body for Once {
    fn chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, HttpError>> {
        let next = self.0.take().filter(|b| !b.is_empty());
        Box::pin(async move { Ok(next) })
    }
}
