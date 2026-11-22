//! RunPod behind the provider seam (WP 9.35, kb d182): its pods, volumes
//! and errors as the seam's types, its create body the engines' own, and
//! its calls the client's requests.

use std::sync::Arc;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_http::HttpClient;
use viy_testkit::fixtures;
use viy_testkit::http::Reply;
use viy_testkit::http::RouteHttp;
use viy_testkit::runpod_v1::V1;

use super::*;
use crate::engine;

const API_KEY: &str = "rpa_SYNTHETICKEY0000";

/// The engine body's fields as a request: what an engine will hand the
/// seam in WP 9.38.
fn request_from(body: &Value) -> PodRequest {
    let text = |k: &str| body[k].as_str().unwrap_or_default().to_string();
    let list = |k: &str| -> Vec<String> {
        body[k]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    };
    PodRequest {
        name: text("name"),
        image: text("imageName"),
        entrypoint: list("dockerEntrypoint"),
        command: list("dockerStartCmd"),
        env: body["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect(),
        gpu_types: list("gpuTypeIds"),
        gpu_count: body["gpuCount"].as_i64().unwrap(),
        region: body["dataCenterIds"][0].as_str().unwrap().to_string(),
        volume: Some(text("networkVolumeId")).filter(|v| !v.is_empty()),
        disk_gb: body["containerDiskInGb"].as_i64().unwrap(),
        ports: list("ports"),
        cuda: list("allowedCudaVersions"),
        min_ram_gb: body["minRAMPerGPU"].as_u64(),
    }
}

fn provider(
    answer: impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static,
) -> (RunPodProvider, Arc<V1>) {
    let api = Arc::new(V1::new(Arc::new(RouteHttp::new(answer))));
    let http: Arc<dyn HttpClient> = api.clone();
    let cfg = Config::defaults();
    (RunPodProvider::new(RunPod::new(API_KEY, http), &cfg), api)
}

/// The seam's request turns back into the very body each engine builds,
/// key for key and in order, for both engines and with and without a CUDA
/// filter and a host RAM floor.
#[test]
fn a_request_is_the_engines_body() {
    for (engine_name, cuda) in [("vllm", "13.0"), ("vllm", ""), ("strata", "13.0")] {
        let mut cfg = Config::defaults();
        cfg.set("ENGINE", engine_name);
        cfg.set("CUDA_VERSIONS", cuda);
        cfg.set("VLLM_API_KEY", "sk-SYNTHETICENDPOINT");
        let body = engine::for_config(&cfg)
            .unwrap()
            .pod_body(&cfg, API_KEY, "vol222", Some("ssh-ed25519 AAAA test"))
            .unwrap();
        let (seam, _) = provider(|_| panic!("no request"));
        let seam = RunPodProvider::new(seam.rp, &cfg);
        let rebuilt = seam.body(&request_from(&body));
        assert_eq!(
            serde_json::to_string(&rebuilt).unwrap(),
            serde_json::to_string(&body).unwrap(),
            "{engine_name} cuda {cuda:?}"
        );
    }
}

/// Every pod the `pod/api` fixture's replies hold reads as a pod: its id,
/// name, state and price where it has them.
#[test]
fn the_fixtures_pods_read_as_pods() {
    let fixture = fixtures::load("pod/api").unwrap();
    let mut seen = 0;
    for case in &fixture.cases {
        let Some(body) = case["reply"]["body"].as_str() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            continue;
        };
        let rows: Vec<Value> = match &value {
            Value::Array(rows) => rows.clone(),
            Value::Object(o) if o.contains_key("data") => {
                o["data"].as_array().cloned().unwrap_or_default()
            }
            Value::Object(o) if o.contains_key("desiredStatus") => vec![value.clone()],
            _ => continue,
        };
        for row in rows {
            let pod = pod_of(&row);
            assert_eq!(pod.id, row["id"].as_str().unwrap_or_default());
            if row["desiredStatus"] == "RUNNING" {
                assert_eq!(pod.state, Some(PodState::Running));
            }
            if let Some(cost) = row["costPerHr"].as_f64() {
                assert_eq!(pod.cost_per_hour, Some(cost));
            }
            seen += 1;
        }
    }
    assert!(seen >= 3, "{seen}");
}

#[test]
fn a_pod_reads_whole() {
    let v1 = json!({
        "id": "pod111", "name": "qwen-pod", "desiredStatus": "EXITED", "costPerHr": "2.19",
        "gpu": {"id": "NVIDIA A100-SXM4-80GB"}, "gpuCount": 1,
        "machine": {"dataCenterId": "EUR-IS-1"}, "publicIp": "157.0.0.1",
        "portMappings": {"22": 16219}, "lastStartedAt": "2026-08-08T23:40:24Z",
        "createdAt": "2026-08-08T23:40:21Z", "networkVolumeId": "trn123",
    });
    assert_eq!(
        pod_of(&v1),
        Pod {
            id: "pod111".into(),
            name: Some("qwen-pod".into()),
            state: Some(PodState::Stopped),
            state_name: Some("EXITED".into()),
            cost_per_hour: Some(2.19),
            gpu: Some("NVIDIA A100-SXM4-80GB".into()),
            gpu_name: Some("NVIDIA A100-SXM4-80GB".into()),
            gpu_count: Some(1),
            region: Some("EUR-IS-1".into()),
            ssh: Some(("157.0.0.1".into(), 16219)),
            started_at: Some("2026-08-08T23:40:24Z".into()),
            created_at: Some("2026-08-08T23:40:21Z".into()),
            volume: Some("trn123".into()),
        }
    );
    assert_eq!(
        pod_of(&json!({"id": "p", "desiredStatus": "WHO KNOWS"})).state,
        Some(PodState::Unknown)
    );
    assert_eq!(pod_of(&json!({"id": "p"})).state, None);
}

#[test]
fn errors_keep_what_the_commands_decide_on() {
    let capacity = RunPodError::new(
        "POST /pods -> HTTP 400\nThere are no longer any instances available",
        Some(400),
    );
    let seam = error_of(&capacity);
    assert!(seam.capacity && !seam.transient);
    assert_eq!(seam.status, Some(400));
    assert_eq!(seam.message, capacity.to_string());
    let busy = error_of(&RunPodError::new("GET /pods -> HTTP 503", Some(503)));
    assert!(busy.transient && !busy.capacity);
}

/// The seam's calls are the client's requests.
#[tokio::test(start_paused = true)]
async fn calls_are_the_clients_requests() {
    let (seam, api) = provider(|r| {
        let url = r.url.as_str();
        if url.ends_with("/v1/pods") {
            Reply::answer(
                200,
                json!([{"id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING"}])
                    .to_string(),
            )
        } else if url.ends_with("/v1/networkvolumes") {
            Reply::answer(
                200,
                json!([{"id": "vol222", "name": "w", "dataCenterId": "EUR-IS-1", "size": 100}])
                    .to_string(),
            )
        } else {
            Reply::answer(200, "{}")
        }
    });
    let pods = seam.pods().await.unwrap();
    assert_eq!(pods[0].id, "pod111");
    seam.start("pod111").await.unwrap();
    seam.stop("pod111").await.unwrap();
    seam.terminate("pod111").await.unwrap();
    let volumes = seam.volumes().unwrap().list().await.unwrap();
    assert_eq!(
        volumes,
        [Volume {
            id: "vol222".into(),
            name: Some("w".into()),
            region: Some("EUR-IS-1".into()),
            size_gb: Some(100)
        }]
    );
    let sent: Vec<String> = api
        .sent()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url))
        .collect();
    assert_eq!(
        sent,
        [
            "GET https://rest.runpod.io/v1/pods",
            "POST https://rest.runpod.io/v1/pods/pod111/start",
            "POST https://rest.runpod.io/v1/pods/pod111/stop",
            "DELETE https://rest.runpod.io/v1/pods/pod111",
            "GET https://rest.runpod.io/v1/networkvolumes",
        ]
    );
}

/// The SSH endpoint follows the tunnel's rules: no empty address, no port
/// 0; a booting pod's state is RunPod's RUNNING, its word kept.
#[test]
fn ssh_and_state_follow_todays_rules() {
    for v1 in [
        json!({"id": "p", "publicIp": "", "portMappings": {"22": 22}}),
        json!({"id": "p", "publicIp": "1.2.3.4", "portMappings": {"22": 0}}),
    ] {
        assert_eq!(pod_of(&v1).ssh, None, "{v1}");
    }
    let odd = pod_of(&json!({"id": "p", "desiredStatus": "PAUSED"}));
    assert_eq!(odd.state, Some(PodState::Unknown));
    assert_eq!(odd.state_name.as_deref(), Some("PAUSED"));
    assert_eq!(pod_of(&json!({"id": "p"})).name, None);
}

/// No terminated pod in the list; an answer without an id keeps the one
/// asked for; a create without an id is RunPod's "created no pod".
#[tokio::test(start_paused = true)]
async fn the_contract_on_lists_ids_and_creates() {
    let (seam, _) = provider(|r| {
        let url = r.url.as_str();
        if url.ends_with("/v1/pods") && r.method == viy_http::Method::GET {
            Reply::answer(
                200,
                json!([{"id": "a", "desiredStatus": "RUNNING"},
                       {"id": "b", "desiredStatus": "TERMINATED"}])
                .to_string(),
            )
        } else if url.ends_with("/v1/pods") {
            Reply::answer(200, "{}")
        } else if url.ends_with("/v1/pods/pod111") {
            Reply::answer(200, json!({"desiredStatus": "RUNNING"}).to_string())
        } else {
            Reply::answer(200, "{}")
        }
    });
    let ids: Vec<String> = seam
        .pods()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(ids, ["a"]);
    assert_eq!(seam.pod("pod111").await.unwrap().unwrap().id, "pod111");
    let error = Provider::create(&seam, &PodRequest::default())
        .await
        .unwrap_err();
    assert_eq!(error.message, "RunPod created no pod");
}

/// The GPU's type stays its id; what viy shows prefers a display name; a
/// count sent as a string is read.
#[test]
fn the_gpu_and_its_count() {
    let pod = pod_of(&json!({"id": "p", "gpu": {"id": "NVIDIA A100-SXM4-80GB",
                                                 "displayName": "A100 SXM"},
                             "gpuCount": "2"}));
    assert_eq!(pod.gpu.as_deref(), Some("NVIDIA A100-SXM4-80GB"));
    assert_eq!(pod.gpu_name.as_deref(), Some("A100 SXM"));
    assert_eq!(pod.gpu_count, Some(2));
}

#[test]
fn a_volume_size_sent_as_a_string_is_read() {
    assert_eq!(
        volume_of(&json!({"id": "v", "size": "100"})).size_gb,
        Some(100)
    );
    assert_eq!(volume_of(&json!({"id": "v"})).name, None);
}
