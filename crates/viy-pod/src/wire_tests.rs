use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

/// RunPod's own example of a created pod (api.runpod.io/v2/openapi.json,
/// `createPod`'s 201, retrieved 2026-08-03), made running, with a network
/// volume and a direct SSH mapping on a documentation address.
fn running() -> Value {
    json!({
        "id": "7h9k2m4n6p", "name": "pytorch-training",
        "image": "runpod/pytorch:1.0.2-cu1281-torch280-ubuntu2404", "args": "", "disk": 50,
        "ports": ["8888/http", "22/tcp"], "env": {"MODEL_NAME": "llama-3"}, "registry": null,
        "status": "RUNNING", "actions": ["stop", "restart", "terminate"],
        "mounts": {"network": [{"volumeId": "vol222", "path": "/workspace"}]},
        "gpu": {"id": "NVIDIA GeForce RTX 4090", "count": 1, "vcpuCount": 16, "memory": 64},
        "cloud": "SECURE", "dataCenterId": "US-KS-2", "cudaVersion": "12.8",
        "ssh": {
            "proxy": {"host": "ssh.runpod.io", "port": 22, "username": "7h9k2m4n6p-64411eb2",
                      "command": "ssh 7h9k2m4n6p-64411eb2@ssh.runpod.io"},
            "direct": {"host": "203.0.113.7", "port": 40123, "username": "root",
                       "command": "ssh root@203.0.113.7 -p 40123"}
        },
        "template": null, "cost": 0.44, "locked": false, "globalNetworking": {"enabled": false},
        "runtime": null, "createdAt": "2026-02-16T12:00:00Z", "startedAt": "2026-02-16T12:01:00Z"
    })
}

#[test]
fn a_v2_pod_reads_as_v1s_and_keeps_nothing_else() {
    assert_eq!(
        pod(running()),
        json!({
            "id": "7h9k2m4n6p", "name": "pytorch-training", "desiredStatus": "RUNNING",
            "costPerHr": 0.44, "gpu": {"id": "NVIDIA GeForce RTX 4090"}, "gpuCount": 1,
            "machine": {"dataCenterId": "US-KS-2"}, "publicIp": "203.0.113.7",
            "portMappings": {"22": 40123}, "lastStartedAt": "2026-02-16T12:01:00Z",
            "createdAt": "2026-02-16T12:00:00Z", "networkVolumeId": "vol222"
        })
    );
}

#[test]
fn a_pod_meant_to_run_is_running_to_v1() {
    for (status, desired) in [
        ("PROVISIONING", "RUNNING"),
        ("STARTING", "RUNNING"),
        ("RUNNING", "RUNNING"),
        ("ERROR", "RUNNING"),
        ("EXITED", "EXITED"),
        ("TERMINATED", "TERMINATED"),
    ] {
        let read = pod(json!({"id": "p", "status": status}));
        assert_eq!(read["desiredStatus"], desired, "{status}");
    }
}

#[test]
fn a_stopped_pods_old_ssh_mapping_is_left_out() {
    for status in ["EXITED", "TERMINATED"] {
        let mut v2 = running();
        v2["status"] = json!(status);
        let read = pod(v2);
        assert_eq!(read.get("publicIp"), None, "{status}");
        assert_eq!(read.get("portMappings"), None, "{status}");
    }
    let mut starting = running();
    starting["status"] = json!("STARTING");
    assert_eq!(pod(starting)["portMappings"], json!({"22": 40123}));
}

#[test]
fn what_v2_leaves_null_or_out_is_not_there() {
    let listed = json!({
        "id": "p", "name": "n", "status": "EXITED", "cost": 1.89, "dataCenterId": null,
        "ssh": {"proxy": null, "direct": null}, "startedAt": null, "runtime": null,
        "mounts": {}
    });
    assert_eq!(
        pod(listed),
        json!({"id": "p", "name": "n", "desiredStatus": "EXITED", "costPerHr": 1.89})
    );
    assert_eq!(pod(json!(null)), json!(null));
}

#[test]
fn a_volume_reads_data_center_as_v1s_id() {
    assert_eq!(
        volume(
            json!({"id": "v", "name": "w", "size": 100, "dataCenter": "EU-RO-1",
                      "type": "STANDARD"})
        ),
        json!({"id": "v", "name": "w", "size": 100, "dataCenterId": "EU-RO-1",
               "type": "STANDARD"})
    );
}

/// rpctl's create body, as `viy_vllm::build_pod_body` and failover make it.
fn v1_body() -> Value {
    json!({
        "name": "qwen38-abliterated", "imageName": "vllm/vllm-openai:v0.33.0",
        "computeType": "GPU", "cloudType": "SECURE", "gpuCount": 1,
        "gpuTypeIds": ["NVIDIA A100 80GB PCIe", "NVIDIA A100-SXM4-80GB"],
        "gpuTypePriority": "custom", "dataCenterIds": ["EUR-IS-1"],
        "networkVolumeId": "vol222", "volumeMountPath": "/workspace",
        "containerDiskInGb": 60, "ports": ["22/tcp"], "env": {"HF_HOME": "/workspace/hf"},
        "dockerEntrypoint": ["/bin/bash", "-c"], "dockerStartCmd": ["echo boot"],
        "allowedCudaVersions": ["13.0"]
    })
}

#[test]
fn a_v1_create_is_one_v2_request_per_gpu_type_in_order() {
    let requests = pod_requests(&v1_body());
    let first = json!({
        "name": "qwen38-abliterated", "image": "vllm/vllm-openai:v0.33.0", "cloud": "SECURE",
        "gpu": {"id": "NVIDIA A100 80GB PCIe", "count": 1, "allowedCudaVersions": ["13.0"]},
        "dataCenterIds": ["EUR-IS-1"],
        "mounts": {"network": [{"volumeId": "vol222", "path": "/workspace"}]},
        "disk": 60, "ports": ["22/tcp"], "env": {"HF_HOME": "/workspace/hf"},
        "entrypoint": ["/bin/bash", "-c"], "cmd": ["echo boot"]
    });
    let mut second = first.clone();
    second["gpu"]["id"] = json!("NVIDIA A100-SXM4-80GB");
    assert_eq!(requests, [first, second]);
}

/// A Strata pod's host RAM filter (viy-strata's `minRAMPerGPU`) goes with
/// the GPU in each v2 request.
#[test]
fn the_host_ram_filter_goes_with_the_gpu() {
    let mut body = v1_body();
    body["minRAMPerGPU"] = json!(150);
    let requests = pod_requests(&body);
    assert_eq!(requests[0]["gpu"]["minRamPerGpu"], 150);
    assert_eq!(requests[1]["gpu"]["minRamPerGpu"], 150);
    assert!(requests[0].get("minRAMPerGPU").is_none());
    // vLLM's body has none, and its requests stay as they were
    assert!(
        pod_requests(&v1_body())[0]["gpu"]
            .get("minRamPerGpu")
            .is_none()
    );
}

#[test]
fn a_create_without_the_cuda_filter_or_types_still_asks_once() {
    let mut body = v1_body();
    let fields = body.as_object_mut().unwrap();
    fields.shift_remove("allowedCudaVersions");
    fields.shift_remove("gpuTypeIds");
    let requests = pod_requests(&body);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["gpu"], json!({"count": 1}));
}

#[test]
fn a_page_gives_its_rows_and_the_next_cursor_while_there_is_one() {
    let page_of = |pagination: Value| {
        page(
            Some(json!({"pods": [{"id": "a"}], "pagination": pagination})),
            "pods",
        )
    };
    assert_eq!(
        page_of(json!({"nextCursor": "c1", "hasNextPage": true})),
        (vec![json!({"id": "a"})], Some("c1".to_string()))
    );
    assert_eq!(
        page_of(json!({"nextCursor": "c1", "hasNextPage": false})).1,
        None
    );
    assert_eq!(
        page_of(json!({"nextCursor": null, "hasNextPage": true})).1,
        None
    );
    assert_eq!(page(Some(json!([{"id": "a"}])), "pods"), (Vec::new(), None));
    assert_eq!(page(None, "pods"), (Vec::new(), None));
}

#[test]
fn a_query_value_escapes_all_but_the_unreserved() {
    assert_eq!(query_value("aZ09-._~"), "aZ09-._~");
    assert_eq!(query_value("a/b+c=d é"), "a%2Fb%2Bc%3Dd%20%C3%A9");
}
