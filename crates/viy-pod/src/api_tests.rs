use std::collections::BTreeSet;
use std::sync::Arc;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_testkit::fixtures;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Piece;
use viy_testkit::http::Reply;
use viy_testkit::runpod_v1;
use viy_testkit::runpod_v1::V1;

use super::*;

/// The fixture's synthetic key (tools/fixtures.py API_KEY).
const API_KEY: &str = "rpa_SYNTHETICKEY0000";

/// Headers rp's `requests` sends that viy drops (kb d046): no
/// decompression, and HTTP/1.1's default keep-alive.
const NOT_SENT_BY_VIY: [&str; 2] = ["Accept-Encoding", "Connection"];

/// The fixture's reply as a script. A read timeout is the stream going
/// quiet, so the real per-read timeout ends it.
fn reply(spec: &Value) -> Reply {
    if spec["fail"] == "connect" {
        return Reply::Fail(HttpError::Connect("synthetic: connection refused".into()));
    }
    let chunks: Vec<String> = match spec.get("chunks") {
        Some(chunks) => serde_json::from_value(chunks.clone()).unwrap(),
        None => vec![spec["body"].as_str().unwrap_or("").to_string()],
    };
    let mut body: Vec<Piece> = chunks.into_iter().map(|c| Piece::Data(c.into())).collect();
    if spec["then"] == "read_timeout" {
        body.push(Piece::Hang);
    }
    Reply::Answer {
        status: spec["status"].as_u64().map_or(200, |s| s as u16),
        body,
    }
}

fn strings(value: &Value) -> Vec<String> {
    serde_json::from_value(value.clone()).unwrap()
}

/// Runs the fixture's call; the outcome in the fixture's JSON shape.
async fn call(rp: &RunPod, call: &[Value]) -> Result<Value, RunPodError> {
    let arg = |i: usize| call[i].as_str().unwrap();
    let unit = |()| Value::Null;
    let opt = |v: Option<Value>| v.unwrap_or(Value::Null);
    Ok(match arg(0) {
        "list_pods" => json!(rp.list_pods().await?),
        "get_pod" => opt(rp.get_pod(arg(1)).await?),
        "create_pod" => opt(rp.create_pod(call[1].clone()).await?),
        "stop_pod" => unit(rp.stop_pod(arg(1)).await?),
        "start_pod" => unit(rp.start_pod(arg(1)).await?),
        "delete_pod" => unit(rp.delete_pod(arg(1)).await?),
        "list_volumes" => json!(rp.list_volumes().await?),
        "get_volume" => opt(rp.get_volume(arg(1)).await?),
        "create_volume" => opt(rp
            .create_volume(arg(1), call[2].as_u64().unwrap(), arg(3), None)
            .await?),
        "delete_volume" => unit(rp.delete_volume(arg(1)).await?),
        "data_centers" => json!(rp.data_centers().await?),
        "cuda_stock" => {
            let stock = rp
                .cuda_stock(
                    &strings(&call[1]),
                    &strings(&call[2]),
                    arg(3),
                    &strings(&call[4]),
                    call[5].as_bool().unwrap(),
                )
                .await?;
            json!(
                stock
                    .into_iter()
                    .map(|((dc, gpu), s)| json!([dc, gpu, s.fit, s.old]))
                    .collect::<Vec<_>>()
            )
        }
        "pod_logs" => {
            let entries = rp
                .pod_logs(
                    arg(1),
                    call[2].as_u64().unwrap() as u32,
                    Duration::from_secs(call[3].as_u64().unwrap()),
                )
                .await?;
            json!(entries.into_iter().map(as_rpctl_entry).collect::<Vec<_>>())
        }
        other => panic!("no such call: {other}"),
    })
}

/// A log entry as rpctl's dict: only the keys the line had.
fn as_rpctl_entry(entry: LogEntry) -> Value {
    let mut map = Map::new();
    if let Some(ts) = entry.ts {
        map.insert("ts".into(), ts.into());
    }
    if let Some(source) = entry.source {
        map.insert("source".into(), source.into());
    }
    map.insert("line".into(), entry.line.into());
    Value::Object(map)
}

/// Calls whose result rpctl returns and viy drops: nothing reads it.
fn viy_discards_the_result(call: &str) -> bool {
    matches!(call, "start_pod")
}

/// JSON text in a canonical form that keeps key order, for comparing bodies
/// and details whose spacing differs (serde's compact and pretty output
/// against Python's `json.dumps`, kb d030).
fn canonical(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()
        .map(|v| v.to_string())
}

fn check_request(name: &str, sent: &Request, rp_sent: &Value) {
    assert_eq!(sent.method.as_str(), rp_sent["method"], "{name}");
    assert_eq!(sent.url, rp_sent["url"], "{name}");
    let rp_headers = rp_sent["headers"].as_object().unwrap();
    let names: Vec<&str> = sent.headers.iter().map(|(n, _)| n.as_str()).collect();
    let rp_names: Vec<&str> = rp_headers
        .keys()
        .map(String::as_str)
        .filter(|n| !NOT_SENT_BY_VIY.contains(n))
        .collect();
    assert_eq!(names, rp_names, "{name}: header names and order");
    for (header, value) in &sent.headers {
        if header == "Content-Length" && sent.body.is_some() {
            // The bodies' bytes differ (json.dumps' spacing); the content
            // is compared below.
            let length = sent.body.as_ref().map(Vec::len).unwrap_or_default();
            assert_eq!(value, &length.to_string(), "{name}");
            continue;
        }
        if header == "User-Agent" {
            // rp's is python-requests'; viy's is the provider marker (kb d137).
            assert_eq!(value, viy_http::user_agent::PROVIDER_API, "{name}");
            continue;
        }
        assert_eq!(rp_headers[header], value.as_str(), "{name}: {header}");
        assert!(!value.to_lowercase().contains("viy"), "{name}: {header}");
    }
    let body = sent
        .body
        .as_deref()
        .map(|b| canonical(std::str::from_utf8(b).unwrap()).unwrap());
    let rp_body = rp_sent["body"].as_str().map(|b| canonical(b).unwrap());
    // A pod's v2 body has other fields than these synthetic v1 ones;
    // wire_tests pin what viy sends.
    if !(sent.method == Method::POST && sent.url.ends_with("/v1/pods")) {
        assert_eq!(body, rp_body, "{name}: body");
    }
    let (connect, read) = match &rp_sent["timeout"] {
        Value::Array(pair) => (pair[0].as_u64().unwrap(), pair[1].as_u64().unwrap()),
        one => (one.as_u64().unwrap(), one.as_u64().unwrap()),
    };
    assert_eq!(
        (sent.connect_timeout, sent.read_timeout),
        (Duration::from_secs(connect), Duration::from_secs(read)),
        "{name}: timeouts"
    );
}

/// rp's message where `requests` named an exception class, viy's where it
/// names its own error kind (module docs).
fn viy_message(name: &str, rp_message: &str) -> String {
    let rp_message = &runpod_v1::v2_paths(rp_message);
    match name {
        "transport_failure_is_transient" => {
            "GET /pods/pod111 -> connection failed: synthetic: connection refused".into()
        }
        "read_timeout_is_transient" => "GET /network-volumes -> timeout".into(),
        "graphql_transport_failure" => {
            "GraphQL -> connection failed: synthetic: connection refused".into()
        }
        "pod_logs_stall_before_entries_is_an_error" => "Platform logs unavailable (timeout)".into(),
        "pod_logs_unreachable" => {
            "Platform logs unavailable (connection failed: synthetic: connection refused)".into()
        }
        // A 429 says when RunPod takes requests again (kb d140); without
        // headers, the fallback.
        "get_volume_http_429" => format!(
            "{}\nRunPod's rate limit: it takes requests again in 1m 0s.",
            pretty_detail(rp_message)
        ),
        "graphql_http_429_json" => {
            format!("{rp_message}\nRunPod's rate limit: it takes requests again in 1m 0s.")
        }
        _ => rp_message.clone(),
    }
}

/// What rpctl returned, as far as RunPod's v2 API still says it: the
/// script's answer as v2 gives it, read back as viy reads v2.
fn as_v2_tells_it(call: &str, rp_ok: &Value) -> Value {
    let pod = |p: &Value| crate::wire::pod(runpod_v1::v2_pod(p.clone()));
    let volume = |v: &Value| crate::wire::volume(runpod_v1::v2_volume(v.clone()));
    let each = |rows: &Value, one: &dyn Fn(&Value) -> Value| {
        json!(rows.as_array().unwrap().iter().map(one).collect::<Vec<_>>())
    };
    match call {
        "list_pods" => each(rp_ok, &pod),
        "get_pod" | "create_pod" if !rp_ok.is_null() => pod(rp_ok),
        "list_volumes" => each(rp_ok, &volume),
        "get_volume" | "create_volume" if !rp_ok.is_null() => volume(rp_ok),
        _ => rp_ok.clone(),
    }
}

/// `rp_message` with its JSON detail as viy pretty-prints it.
fn pretty_detail(rp_message: &str) -> String {
    let (head, detail) = rp_message.split_once('\n').unwrap_or((rp_message, ""));
    let detail: Value = serde_json::from_str(detail).unwrap();
    format!("{head}\n{}", serde_json::to_string_pretty(&detail).unwrap())
}

fn check_error(name: &str, error: &RunPodError, case: &Value) {
    assert_eq!(
        error.status_code().map(u64::from),
        case["status_code"].as_u64(),
        "{name}"
    );
    assert_eq!(error.transient(), case["transient"], "{name}");
    let expected = viy_message(name, case["error"].as_str().unwrap());
    let message = error.to_string();
    let (head, detail) = message.split_once('\n').unwrap_or((&message, ""));
    let (rp_head, rp_detail) = expected.split_once('\n').unwrap_or((&expected, ""));
    assert_eq!(head, rp_head, "{name}");
    match canonical(rp_detail) {
        Some(rp_json) => assert_eq!(canonical(detail), Some(rp_json), "{name}: {detail}"),
        None => assert_eq!(detail, rp_detail, "{name}"),
    }
}

#[tokio::test(start_paused = true)]
async fn each_call_sends_and_reads_what_rpctls_does() {
    let fixture = fixtures::load("pod/api").unwrap();
    let mut seen = BTreeSet::new();
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let call_spec = case["call"].as_array().unwrap();
        seen.insert(call_spec[0].as_str().unwrap().to_string());
        if name == "proxy_url" {
            assert_eq!(
                proxy_url(call_spec[1].as_str().unwrap(), "8000"),
                case["ok"]
            );
            continue;
        }
        let http = Arc::new(V1::new(Arc::new(FakeHttp::new([reply(&case["reply"])]))));
        let rp = RunPod::new(API_KEY, http.clone());
        let outcome = call(&rp, call_spec).await;

        let sent = http.sent();
        let rp_sent = case["sent"].as_array().unwrap();
        assert_eq!(sent.len(), rp_sent.len(), "{name}");
        for (sent, rp_sent) in sent.iter().zip(rp_sent) {
            check_request(name, sent, rp_sent);
        }
        match outcome {
            Ok(value) => {
                assert!(
                    case.get("error").is_none(),
                    "{name}: rp failed, viy did not"
                );
                let called = call_spec[0].as_str().unwrap();
                if !viy_discards_the_result(called) {
                    assert_eq!(value, as_v2_tells_it(called, &case["ok"]), "{name}");
                }
            }
            Err(error) => check_error(name, &error, case),
        }
    }
    // Every public call of rpctl's RunPod and proxy_url.
    assert_eq!(seen.len(), 14, "{seen:?}");
}

#[test]
fn transient_is_no_answer_429_or_a_5xx_to_504() {
    let transient: Vec<Option<u16>> = [None, Some(200), Some(400), Some(401), Some(403)]
        .into_iter()
        .chain((404..=599).map(Some))
        .filter(|&code| RunPodError::new("x", code).transient())
        .collect();
    assert_eq!(
        transient,
        [None, Some(429), Some(500), Some(502), Some(503), Some(504)]
    );
}

fn log_line(ts: &str, line: &str) -> Vec<u8> {
    format!("data: {}\n", json!({"ts": ts, "line": line})).into_bytes()
}

fn logs_rp(body: Vec<Piece>) -> RunPod {
    let http = FakeHttp::new([Reply::Answer { status: 200, body }]);
    RunPod::new(API_KEY, Arc::new(http))
}

/// rpctl checks its budget only when a line arrives, so a stream that keeps
/// sending keepalives ends there; viy ends at the budget itself.
#[tokio::test(start_paused = true)]
async fn the_log_read_ends_at_the_budget_while_lines_keep_coming() {
    let mut body = vec![Piece::Data(log_line("1", "in time"))];
    for i in 0..20 {
        body.push(Piece::Late(
            Duration::from_secs(1),
            b": keepalive\n".to_vec(),
        ));
        if i == 9 {
            body.push(Piece::Data(log_line("2", "too late")));
        }
    }
    let rp = logs_rp(body);
    let start = Instant::now();
    let entries = rp.pod_logs("pod111", 100, LOG_BUDGET).await.unwrap();
    assert_eq!(start.elapsed(), LOG_BUDGET);
    assert_eq!(
        entries,
        [LogEntry {
            ts: Some("1".into()),
            source: None,
            line: "in time".into(),
        }]
    );
}

#[tokio::test(start_paused = true)]
async fn a_budget_spent_on_keepalives_is_no_entries_not_an_error() {
    let body = (0..20)
        .map(|_| Piece::Late(Duration::from_secs(1), b": keepalive\n".to_vec()))
        .collect();
    let entries = logs_rp(body)
        .pod_logs("pod111", 100, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(entries, []);
}

#[tokio::test(start_paused = true)]
async fn a_quiet_log_stream_ends_after_two_seconds() {
    let rp = logs_rp(vec![Piece::Data(log_line("1", "a")), Piece::Hang]);
    let start = Instant::now();
    let entries = rp.pod_logs("pod111", 100, LOG_BUDGET).await.unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    assert_eq!(entries.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_log_stream_that_never_answers_gives_up_after_seven_seconds() {
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([Reply::Hang])));
    let start = Instant::now();
    let error = rp.pod_logs("pod111", 100, LOG_BUDGET).await.unwrap_err();
    assert_eq!(start.elapsed(), Duration::from_secs(7));
    assert_eq!(error.to_string(), "Platform logs unavailable (timeout)");
    assert!(error.transient());
}

#[tokio::test(start_paused = true)]
async fn a_rest_call_waits_two_minutes_for_an_answer_at_most() {
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([Reply::Hang])));
    let start = Instant::now();
    let error = rp.get_pod("pod111").await.unwrap_err();
    assert_eq!(start.elapsed(), Duration::from_secs(120));
    assert_eq!(
        (error.to_string().as_str(), error.status_code()),
        ("GET /pods/pod111 -> timeout", None)
    );
}

#[test]
fn the_key_is_not_in_debug_output() {
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::default()));
    let shown = format!("{rp:?}");
    assert!(!shown.contains(API_KEY), "{shown}");
}

/// rpctl's `_req` turns JSON `null` into None, as it does an empty body.
#[tokio::test]
async fn a_null_answer_is_no_value() {
    let http = FakeHttp::new([Reply::answer(200, "null"), Reply::answer(200, "")]);
    let rp = RunPod::new(API_KEY, Arc::new(http));
    assert_eq!(rp.get_volume("vol222").await, Ok(None));
    assert_eq!(rp.get_pod("pod111").await, Ok(None));
}

/// The budget ends while the stream is quiet and nothing has come: rpctl
/// waits out the 2 s gap and fails, so the boot report says "unavailable".
#[tokio::test(start_paused = true)]
async fn a_budget_that_ends_in_a_quiet_gap_with_nothing_read_is_an_error() {
    let rp = logs_rp(vec![Piece::Data(b": keepalive\n".to_vec()), Piece::Hang]);
    let start = Instant::now();
    let error = rp
        .pod_logs("pod111", 100, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    assert_eq!(error.to_string(), "Platform logs unavailable (timeout)");
}

#[tokio::test(start_paused = true)]
async fn lines_may_end_in_a_bare_carriage_return() {
    let body = vec![Piece::Data(
        b"data: {\"ts\": \"2\", \"line\": \"b\"}\rdata: {\"ts\": \"1\", \"line\": \"a\"}\r"
            .to_vec(),
    )];
    let lines: Vec<String> = logs_rp(body)
        .pod_logs("pod111", 100, LOG_BUDGET)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.line)
        .collect();
    assert_eq!(lines, ["a", "b"]);
}

#[tokio::test(start_paused = true)]
async fn a_budget_with_no_end_reads_until_the_gap() {
    let rp = logs_rp(vec![Piece::Data(log_line("1", "a")), Piece::Hang]);
    let entries = rp.pod_logs("pod111", 100, Duration::MAX).await.unwrap();
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn bodiless_calls_other_than_get_declare_a_zero_length() {
    let http = Arc::new(FakeHttp::new([
        Reply::answer(200, ""),
        Reply::answer(200, ""),
        Reply::answer(200, "[]"),
    ]));
    let rp = RunPod::new(API_KEY, http.clone());
    rp.delete_pod("pod111").await.unwrap();
    rp.delete_volume("vol222").await.unwrap();
    rp.list_pods().await.unwrap();
    let sent = http.sent();
    let lengths: Vec<Option<&str>> = sent
        .iter()
        .map(|r| r.header_value("Content-Length"))
        .collect();
    assert_eq!(lengths, [Some("0"), Some("0"), None]);
}

/// The same text rpctl's `api.log_time` sorts by.
#[test]
fn log_entries_sort_by_rpctls_key() {
    let entry = |ts: &str| LogEntry {
        ts: Some(ts.to_string()),
        source: None,
        line: String::new(),
    };
    assert_eq!(
        entry("2026-07-27T02:27:52Z").sort_key(),
        "2026-07-27T02:27:52.000000000"
    );
    assert_eq!(
        entry("2026-07-27T02:27:52.771976788Z").sort_key(),
        "2026-07-27T02:27:52.771976788"
    );
    assert_eq!(entry("1").sort_key(), "1");
}

#[tokio::test]
async fn client_tag_off_sends_rps_user_agent() {
    for (value, want) in [
        (None, user_agent::PROVIDER_API),
        (Some("1"), user_agent::PROVIDER_API),
        (Some("0"), user_agent::RP_REQUESTS),
        (Some("no"), user_agent::RP_REQUESTS),
    ] {
        let mut cfg = viy_config::Config::defaults();
        if let Some(value) = value {
            cfg.set("CLIENT_TAG", value);
        }
        let http = Arc::new(FakeHttp::new([Reply::answer(200, "null")]));
        let rp = RunPod::configured(API_KEY, http.clone(), &cfg);
        assert_eq!(rp.get_pod("pod111").await, Ok(None));
        let sent = http.sent();
        assert_eq!(sent[0].header_value("User-Agent"), Some(want), "{value:?}");
    }
}

/// RunPod's answers of 2026-07-30 (kb d139).
#[tokio::test]
async fn a_volume_with_a_tier_asks_for_it_and_reads_as_v1s() {
    let made = r#"{"dataCenter":"EU-RO-1","id":"vol9","name":"w-backup","size":100,"tags":[],"type":"STANDARD"}"#;
    let refused = r#"{"detail":"Standard storage is not available in data center \"AP-IN-1\".","status":400,"title":"Bad Request"}"#;
    let http = Arc::new(FakeHttp::new([
        Reply::answer(201, made),
        Reply::answer(400, refused),
    ]));
    let rp = RunPod::new(API_KEY, http.clone());
    let volume = rp
        .create_volume("w-backup", 100, "EU-RO-1", Some("STANDARD"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(volume["id"], "vol9");
    assert_eq!(volume["dataCenterId"], "EU-RO-1");
    let sent = http.sent();
    assert_eq!(sent[0].url, "https://api.runpod.io/v2/network-volumes");
    let body: Value = serde_json::from_slice(sent[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"name": "w-backup", "size": 100, "dataCenter": "EU-RO-1", "type": "STANDARD"})
    );
    let error = rp
        .create_volume("w-backup", 100, "AP-IN-1", Some("STANDARD"))
        .await
        .unwrap_err();
    assert!(
        crate::regions::is_tier_unavailable(&crate::provider::error_of(&error)),
        "{error}"
    );
}

// ---- RunPod's REST v2 on the wire (kb d151) ------------------------------

/// What the requests sent were, as `METHOD url` and their JSON bodies.
fn requests(http: &FakeHttp) -> Vec<(String, Value)> {
    http.sent()
        .iter()
        .map(|r| {
            let body = r
                .body
                .as_deref()
                .map_or(Value::Null, |b| serde_json::from_slice(b).unwrap());
            (format!("{} {}", r.method, r.url), body)
        })
        .collect()
}

fn create_body(types: &[&str]) -> Value {
    json!({"name": "qwen-pod", "imageName": "img", "gpuCount": 1, "gpuTypeIds": types,
           "dataCenterIds": ["EUR-IS-1"], "networkVolumeId": "vol222",
           "volumeMountPath": "/workspace"})
}

/// RunPod's v2 capacity refusal, as reported on 2026-07-07 (kb f151).
const NO_CAPACITY: &str = r#"{"detail": "There are no longer any instances available with the requested specifications. Please refresh and try again.", "status": 400, "title": "Bad Request"}"#;
const NO_ACCESS: &str = r#"{"title": "Forbidden", "status": 403, "detail": "access denied"}"#;

#[tokio::test]
async fn a_pod_is_asked_for_one_gpu_type_at_a_time_until_one_is_placed() {
    let http = Arc::new(FakeHttp::new([
        Reply::answer(400, NO_CAPACITY),
        Reply::answer(403, NO_ACCESS),
        Reply::answer(
            201,
            r#"{"id": "pod111", "status": "PROVISIONING", "cost": 1.19}"#,
        ),
    ]));
    let rp = RunPod::new(API_KEY, http.clone());
    let pod = rp.create_pod(create_body(&["A", "B", "C", "D"])).await;
    assert_eq!(
        pod,
        Ok(Some(
            json!({"id": "pod111", "desiredStatus": "RUNNING", "costPerHr": 1.19})
        ))
    );
    let sent = requests(&http);
    let asked: Vec<(&str, &Value)> = sent
        .iter()
        .map(|(call, body)| (call.as_str(), &body["gpu"]["id"]))
        .collect();
    let post = "POST https://api.runpod.io/v2/pods";
    assert_eq!(
        asked,
        [
            (post, &json!("A")),
            (post, &json!("B")),
            (post, &json!("C"))
        ]
    );
    assert_eq!(
        sent[0].1,
        json!({"name": "qwen-pod", "image": "img", "gpu": {"id": "A", "count": 1},
               "dataCenterIds": ["EUR-IS-1"],
               "mounts": {"network": [{"volumeId": "vol222", "path": "/workspace"}]}})
    );
}

#[tokio::test]
async fn every_type_refused_answers_with_the_capacity_refusal_for_failover() {
    let http = Arc::new(FakeHttp::new([
        Reply::answer(400, NO_CAPACITY),
        Reply::answer(403, NO_ACCESS),
    ]));
    let rp = RunPod::new(API_KEY, http.clone());
    let error = rp.create_pod(create_body(&["A", "B"])).await.unwrap_err();
    assert_eq!(error.status_code(), Some(400));
    assert!(crate::regions::is_capacity_error(&error), "{error}");
    assert!(
        error.to_string().starts_with("POST /pods -> HTTP 400\n"),
        "{error}"
    );

    let http = Arc::new(FakeHttp::new([Reply::answer(403, NO_ACCESS)]));
    let rp = RunPod::new(API_KEY, http.clone());
    let error = rp.create_pod(create_body(&["A"])).await.unwrap_err();
    assert_eq!(error.status_code(), Some(403));
}

#[tokio::test]
async fn a_refusal_not_about_capacity_ends_the_search() {
    for status in [400, 402, 422, 500] {
        let refused = r#"{"title": "Refused", "status": 0, "detail": "synthetic"}"#;
        let http = Arc::new(FakeHttp::new([Reply::answer(status, refused)]));
        let rp = RunPod::new(API_KEY, http.clone());
        let error = rp.create_pod(create_body(&["A", "B"])).await.unwrap_err();
        assert_eq!(error.status_code(), Some(status));
        assert_eq!(http.sent().len(), 1, "{status}");
    }
}

/// rpctl's own create body, built from viy's defaults, fits under v2's
/// request limit of 102400 bytes (api.runpod.io/v2/openapi.json, 413).
#[test]
fn the_real_create_requests_fit_v2s_size_limit() {
    let mut cfg = viy_config::Config::defaults();
    // A real token's length, dashed so the secret scanner passes it.
    cfg.set("HF_TOKEN", "hf_SYNTHETIC-TOKEN-00000000000000000");
    cfg.set(
        "RUNPOD_WATCHDOG_API_KEY",
        "rpa_SYNTHETICWATCHDOG000000000000",
    );
    let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAISYNTHETICSYNTHETICSYNTHETICSYNTHETIC x";
    let body = viy_vllm::build_pod_body(&cfg, "sk-SYNTHETIC", "vol222", Some(key)).unwrap();
    for request in crate::wire::pod_requests(&body) {
        let size = request.to_string().len();
        assert!(size < 102_400, "{size} bytes");
    }
}

#[tokio::test]
async fn stop_and_start_are_actions_and_one_already_done_counts() {
    let http = Arc::new(FakeHttp::new([
        Reply::answer(200, r#"{"id": "pod111", "status": "EXITED"}"#),
        Reply::answer(
            409,
            r#"{"title": "Conflict", "status": 409, "detail": "not allowed"}"#,
        ),
        Reply::answer(200, r#"{"id": "pod111", "status": "EXITED"}"#),
        Reply::answer(
            409,
            r#"{"title": "Conflict", "status": 409, "detail": "not allowed"}"#,
        ),
        Reply::answer(200, r#"{"id": "pod111", "status": "STARTING"}"#),
        Reply::answer(
            409,
            r#"{"title": "Conflict", "status": 409, "detail": "not allowed"}"#,
        ),
        Reply::answer(200, r#"{"id": "pod111", "status": "STARTING"}"#),
    ]));
    let rp = RunPod::new(API_KEY, http.clone());
    assert_eq!(rp.stop_pod("pod111").await, Ok(()));
    // Stopped already, as in RunPod's console.
    assert_eq!(rp.stop_pod("pod111").await, Ok(()));
    // Starting already.
    assert_eq!(rp.start_pod("pod111").await, Ok(()));
    // Starting, so not stopped.
    let error = rp.stop_pod("pod111").await.unwrap_err();
    assert_eq!(error.status_code(), Some(409));
    assert!(
        error
            .to_string()
            .starts_with("POST /pods/pod111/action -> HTTP 409\n"),
        "{error}"
    );
    let action = |body: &str| {
        (
            "POST https://api.runpod.io/v2/pods/pod111/action".to_string(),
            json!({"action": body}),
        )
    };
    let get = || {
        (
            "GET https://api.runpod.io/v2/pods/pod111".to_string(),
            Value::Null,
        )
    };
    assert_eq!(
        requests(&http),
        [
            action("stop"),
            action("stop"),
            get(),
            action("start"),
            get(),
            action("stop"),
            get(),
        ]
    );
}

#[tokio::test]
async fn lists_follow_their_pages() {
    let http = Arc::new(FakeHttp::new([
        Reply::answer(
            200,
            r#"{"pods": [{"id": "a", "status": "RUNNING"}], "pagination": {"nextCursor": "c/1=", "hasNextPage": true}}"#,
        ),
        Reply::answer(
            200,
            r#"{"pods": [{"id": "b", "status": "EXITED"}], "pagination": {"nextCursor": null, "hasNextPage": false}}"#,
        ),
        Reply::answer(
            200,
            r#"{"networkVolumes": [{"id": "v", "name": "w", "size": 100, "dataCenter": "EU-RO-1"}], "pagination": {"nextCursor": null, "hasNextPage": false}}"#,
        ),
    ]));
    let rp = RunPod::new(API_KEY, http.clone());
    assert_eq!(
        rp.list_pods().await.unwrap(),
        [
            json!({"id": "a", "desiredStatus": "RUNNING"}),
            json!({"id": "b", "desiredStatus": "EXITED"})
        ]
    );
    assert_eq!(
        rp.list_volumes().await.unwrap(),
        [json!({"id": "v", "name": "w", "size": 100, "dataCenterId": "EU-RO-1"})]
    );
    let urls: Vec<String> = http.sent().into_iter().map(|r| r.url).collect();
    assert_eq!(
        urls,
        [
            "https://api.runpod.io/v2/pods",
            "https://api.runpod.io/v2/pods?cursor=c%2F1%3D",
            "https://api.runpod.io/v2/network-volumes",
        ]
    );
}

// ---- RunPod's rate limit (kb d140) ---------------------------------------

const LIMITED: &str = r#"{"error":"rate limit exceeded for the hour window","status":429}"#;

fn limited(headers: &[(&str, &str)]) -> Reply {
    Reply::Headed {
        status: 429,
        headers: headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: vec![Piece::Data(LIMITED.into())],
    }
}

fn pod_reply() -> Reply {
    Reply::answer(200, r#"{"id":"pod111"}"#)
}

#[tokio::test(start_paused = true)]
async fn a_429_holds_every_clone_until_retry_after_and_says_how_long() {
    let http = Arc::new(FakeHttp::new([
        limited(&[("Retry-After", "1812")]),
        pod_reply(),
    ]));
    let rp = RunPod::new(API_KEY, http.clone());
    let refused = rp.get_pod("pod111").await.unwrap_err();
    assert_eq!(refused.status_code(), Some(429));
    assert!(refused.transient());
    assert_eq!(refused.retry_after(), Some(Duration::from_secs(1812)));
    assert!(
        refused.to_string().ends_with(
            "-> HTTP 429\n{\n  \"error\": \"rate limit exceeded for the hour window\",\n  \
             \"status\": 429\n}\nRunPod's rate limit: it takes requests again in 30m 12s."
        ),
        "{refused}"
    );

    // A clone, as the wait's progress holds one: not sent.
    let clone = rp.clone();
    tokio::time::advance(Duration::from_secs(812)).await;
    let held = clone.pod_logs("pod111", 30, LOG_BUDGET).await.unwrap_err();
    assert_eq!(held.retry_after(), Some(Duration::from_secs(1000)));
    assert_eq!(
        held.to_string(),
        "Platform logs -> not sent\nRunPod's rate limit: it takes requests again in 16m 40s."
    );
    let held = rp.get_pod("pod111").await.unwrap_err();
    assert!(
        held.to_string()
            .starts_with("GET /pods/pod111 -> not sent\n"),
        "{held}"
    );
    assert_eq!(http.sent().len(), 1);

    tokio::time::advance(Duration::from_secs(1000)).await;
    assert_eq!(rp.held_for(), None);
    assert_eq!(
        rp.get_pod("pod111").await.unwrap(),
        Some(json!({"id": "pod111"}))
    );
    assert_eq!(http.sent().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn without_retry_after_the_spent_windows_reset_holds_else_the_fallback() {
    let http = Arc::new(FakeHttp::new([
        limited(&[(
            "RateLimit",
            r#""minute";r=41;t=12, "hour";r=0;t=1812, "day";r=48000;t=80000"#,
        )]),
        limited(&[]),
    ]));
    let rp = RunPod::new(API_KEY, http.clone());
    let refused = rp.list_pods().await.unwrap_err();
    assert_eq!(refused.retry_after(), Some(Duration::from_secs(1812)));

    tokio::time::advance(Duration::from_secs(1812)).await;
    let refused = rp.data_centers().await.unwrap_err();
    assert_eq!(refused.status_code(), Some(429));
    assert_eq!(refused.retry_after(), Some(RATE_LIMIT_FALLBACK));
    assert!(
        refused.to_string().starts_with("GraphQL -> HTTP 429\n"),
        "{refused}"
    );
    assert_eq!(http.sent().len(), 2);
}

#[test]
fn a_wait_is_spoken_in_whole_seconds_rounded_up() {
    let said: Vec<String> = [0.0, 0.2, 45.0, 60.0, 1812.0, 3600.0, 86399.0]
        .into_iter()
        .map(|s| spoken(Duration::from_secs_f64(s)))
        .collect();
    assert_eq!(
        said,
        ["0s", "1s", "45s", "1m 0s", "30m 12s", "1h 0m", "23h 59m"]
    );
}
