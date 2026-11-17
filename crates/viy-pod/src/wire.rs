//! RunPod's REST v2 shapes, sent and read as the v1 shapes the rest of viy
//! uses. RunPod retired REST v1 (`rest.runpod.io/v1`) on 2026-08-02 (kb
//! f150); rpctl's code, and so viy's failover, status, reap and tunnel,
//! read v1's names. [`crate::api`] speaks v2 on the wire and translates
//! here, so nothing above it changed (kb d151).
//!
//! Only what viy reads crosses: a v2 pod's `env` and command, which carry
//! keys, never leave this module.

use serde_json::Map;
use serde_json::Value;
use serde_json::json;

/// Where the weights volume is mounted, as rpctl's create body says.
const VOLUME_MOUNT: &str = "/workspace";

/// v2's pod status as v1's `desiredStatus`, what the pod is meant to be.
/// A pod being placed or started, or whose container failed, is still
/// meant to run: v1 said RUNNING for all of them, and so `viy up` waits on
/// it, `viy reap` counts it as billed and `viy down` terminates it.
fn desired_status(status: &str) -> &str {
    match status {
        "PROVISIONING" | "STARTING" | "ERROR" => "RUNNING",
        other => other,
    }
}

/// A v2 pod as v1's: `id`, `name`, `desiredStatus`, `costPerHr`, `gpu`
/// (`id`), `gpuCount`, `machine` (`dataCenterId`), `publicIp` and
/// `portMappings` (port 22 only), `lastStartedAt`, `createdAt` and
/// `networkVolumeId`, each only when v2 has it. v2 has no host id (v1's
/// `machineId`). A stopped pod can keep its old SSH mapping, which no
/// longer leads anywhere, so it is left out then, as v1 left it out.
pub(crate) fn pod(v2: Value) -> Value {
    let Value::Object(v2) = v2 else {
        return v2;
    };
    let text = |key: &str| v2.get(key).and_then(Value::as_str);
    let mut pod = Map::new();
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value.filter(|v| !v.is_null()) {
            pod.insert(key.to_string(), value);
        }
    };
    put("id", v2.get("id").cloned());
    put("name", v2.get("name").cloned());
    let status = text("status");
    put("desiredStatus", status.map(|s| desired_status(s).into()));
    put("costPerHr", v2.get("cost").cloned());
    if let Some(gpu) = v2.get("gpu") {
        put("gpu", gpu.get("id").map(|id| json!({ "id": id })));
        put("gpuCount", gpu.get("count").cloned());
    }
    put(
        "machine",
        text("dataCenterId").map(|dc| json!({ "dataCenterId": dc })),
    );
    let stopped = matches!(status, Some("EXITED" | "TERMINATED"));
    let direct = v2.get("ssh").and_then(|ssh| ssh.get("direct"));
    if let Some(direct) = direct.filter(|_| !stopped) {
        put("publicIp", direct.get("host").cloned());
        put(
            "portMappings",
            direct.get("port").map(|port| json!({ "22": port })),
        );
    }
    put("lastStartedAt", v2.get("startedAt").cloned());
    put("createdAt", v2.get("createdAt").cloned());
    put(
        "networkVolumeId",
        v2.get("mounts")
            .and_then(|mounts| mounts.pointer("/network/0/volumeId"))
            .cloned(),
    );
    Value::Object(pod)
}

/// A v2 network volume as v1's: `dataCenter` is v1's `dataCenterId`; the
/// rest (`id`, `name`, `size`, `type`) keeps its name.
pub(crate) fn volume(v2: Value) -> Value {
    let Value::Object(v2) = v2 else {
        return v2;
    };
    Value::Object(
        v2.into_iter()
            .map(|(key, value)| match key.as_str() {
                "dataCenter" => ("dataCenterId".to_string(), value),
                _ => (key, value),
            })
            .collect(),
    )
}

/// The v2 create requests for rpctl's v1 create body, one per GPU type of
/// its `gpuTypeIds`, in order: v2 places one type per request and never
/// falls back to another (v1 with `gpuTypePriority: custom` tried them in
/// this order itself). `computeType`, `gpuTypePriority` and
/// `volumeMountPath` have no v2 field of their own.
pub(crate) fn pod_requests(v1: &Value) -> Vec<Value> {
    let field = |key: &str| v1.get(key).filter(|v| !v.is_null()).cloned();
    let types: Vec<Option<Value>> = match v1.get("gpuTypeIds").and_then(Value::as_array) {
        Some(types) if !types.is_empty() => types.iter().cloned().map(Some).collect(),
        // RunPod says what is missing.
        _ => vec![None],
    };
    types
        .into_iter()
        .map(|gpu_type| {
            let mut gpu = Map::new();
            if let Some(id) = gpu_type {
                gpu.insert("id".into(), id);
            }
            if let Some(count) = field("gpuCount") {
                gpu.insert("count".into(), count);
            }
            if let Some(cuda) = field("allowedCudaVersions") {
                gpu.insert("allowedCudaVersions".into(), cuda);
            }
            let mut body = Map::new();
            let mut put = |key: &str, value: Option<Value>| {
                if let Some(value) = value {
                    body.insert(key.to_string(), value);
                }
            };
            put("name", field("name"));
            put("image", field("imageName"));
            put("cloud", field("cloudType"));
            put("gpu", Some(Value::Object(gpu)));
            put("dataCenterIds", field("dataCenterIds"));
            put(
                "mounts",
                field("networkVolumeId").map(|id| {
                    let path = field("volumeMountPath").unwrap_or_else(|| VOLUME_MOUNT.into());
                    json!({ "network": [{ "volumeId": id, "path": path }] })
                }),
            );
            put("disk", field("containerDiskInGb"));
            put("ports", field("ports"));
            put("env", field("env"));
            put("entrypoint", field("dockerEntrypoint"));
            put("cmd", field("dockerStartCmd"));
            Value::Object(body)
        })
        .collect()
}

/// A v2 list page: its rows under `key`, and the cursor of the next page
/// when there is one. Anything else is no rows.
pub(crate) fn page(value: Option<Value>, key: &str) -> (Vec<Value>, Option<String>) {
    let Some(Value::Object(mut page)) = value else {
        return (Vec::new(), None);
    };
    let rows = match page.remove(key) {
        Some(Value::Array(rows)) => rows,
        _ => Vec::new(),
    };
    let pagination = page.get("pagination");
    let more = pagination
        .and_then(|p| p.get("hasNextPage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let next = pagination
        .and_then(|p| p.get("nextCursor"))
        .and_then(Value::as_str)
        .filter(|cursor| more && !cursor.is_empty())
        .map(str::to_string);
    (rows, next)
}

/// `text` as a URL query value: RFC 3986's unreserved characters as they
/// are, every other byte percent-encoded.
pub(crate) fn query_value(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
