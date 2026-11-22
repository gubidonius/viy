//! RunPod behind the provider seam (WP 9.35, kb d182): `RunPodProvider`
//! implements `viy_provider::Provider` and `Volumes` over [`RunPod`], the
//! client the pod commands use today, so its requests are the same ones.
//! RunPod's v1-shaped pods (`wire.rs`, kb d151) become [`Pod`]s here, and a
//! [`PodRequest`] becomes RunPod's v1 create body through the one
//! translation the engines use too (`viy_engine::pod::runpod_body`), with
//! RunPod's own settings
//! (CLOUD_TYPE, GPU_TYPE_PRIORITY) from its configuration. `viy status`,
//! `stop`, `down`'s sweep, the shutdown's running-pod check and the reaper
//! go through it since WP 9.36; `up` and its path (tracked, wait, ssh,
//! tunnel), adopt, create and failover in 9.37b; `up`'s resume and volumes
//! moved in 9.37, the engines' requests in 9.38. What viy writes to files
//! shared with rp (pod.json's `costPerHr` as RunPod sent it) stays read
//! from RunPod's own answer on RunPod's side.

use std::time::Duration;

use serde_json::Value;
use viy_config::Config;
use viy_provider::BoxFuture;
use viy_provider::LogLine;
use viy_provider::Pod;
use viy_provider::PodRequest;
use viy_provider::PodState;
use viy_provider::Provider;
use viy_provider::ProviderError;
use viy_provider::Volume;
use viy_provider::Volumes;

use crate::api::RunPod;
use crate::api::RunPodError;
use crate::regions::is_capacity_error;
use crate::tracked::pod_volume_id;

pub struct RunPodProvider {
    rp: RunPod,
    /// CLOUD_TYPE: SECURE or COMMUNITY.
    cloud: String,
    /// GPU_TYPE_PRIORITY: how RunPod reads the list of types.
    gpu_priority: String,
}

impl RunPodProvider {
    /// The client, for what stays RunPod's (tracking, regions, failover).
    pub fn client(&self) -> &RunPod {
        &self.rp
    }

    pub fn new(rp: RunPod, cfg: &Config) -> Self {
        let get = |key: &str| cfg.get(key).unwrap_or("").to_string();
        Self {
            rp,
            cloud: get("CLOUD_TYPE"),
            gpu_priority: get("GPU_TYPE_PRIORITY"),
        }
    }

    /// The v1 create body for `request`: the engines' own translation
    /// (`viy_engine::pod::runpod_body`), with this provider's settings.
    pub fn body(&self, request: &PodRequest) -> Value {
        viy_engine::pod::runpod_body(&self.cloud, &self.gpu_priority, request)
    }
}

/// A pod as RunPod's v1 shape has it. `wire.rs` already reads v2's
/// PROVISIONING, STARTING and ERROR as RUNNING, as v1 did, so RunPod never
/// gives [`PodState::Starting`]: readiness says when a pod serves.
pub fn pod_of(v1: &Value) -> Pod {
    let text = |key: &str| v1.get(key).and_then(Value::as_str).map(str::to_string);
    let state_name = text("desiredStatus");
    let state = state_name.as_deref().map(|s| match s {
        "RUNNING" => PodState::Running,
        "EXITED" => PodState::Stopped,
        "TERMINATED" => PodState::Terminated,
        _ => PodState::Unknown,
    });
    let number = |v: Option<&Value>| match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    };
    let machine = v1.get("machine");
    let gpu_field = |key: &str| {
        v1.get("gpu")
            .and_then(|g| g.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let gpu = gpu_field("id");
    // `viy status`'s rule: the type's display name, else its id.
    let gpu_name = gpu_field("displayName").or_else(|| gpu.clone());
    Pod {
        id: text("id").unwrap_or_default(),
        name: text("name"),
        state,
        state_name,
        cost_per_hour: number(v1.get("costPerHr")),
        gpu,
        gpu_name,
        // A number, or a number RunPod sent as a string.
        gpu_count: match v1.get("gpuCount") {
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        }
        .and_then(|n| u32::try_from(n).ok()),
        region: machine
            .and_then(|m| m.get("dataCenterId"))
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
            .map(str::to_string),
        // The rules the tunnel and the waits use today.
        ssh: crate::ssh::ssh_endpoint(v1),
        started_at: text("lastStartedAt").filter(|s| !s.is_empty()),
        created_at: text("createdAt"),
        volume: pod_volume_id(v1).map(str::to_string),
    }
}

/// A volume as RunPod's v1 shape has it.
pub fn volume_of(v1: &Value) -> Volume {
    let text = |key: &str| v1.get(key).and_then(Value::as_str).map(str::to_string);
    Volume {
        id: text("id").unwrap_or_default(),
        name: text("name"),
        region: text("dataCenterId"),
        // A number, or a number sent as a string, as `viy volume` lists it.
        size_gb: match v1.get("size") {
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        },
    }
}

/// A RunPod error with what the commands decide on.
pub fn error_of(error: &RunPodError) -> ProviderError {
    ProviderError {
        message: error.to_string(),
        status: error.status_code(),
        capacity: is_capacity_error(error),
        transient: error.transient(),
        retry_after: error.retry_after(),
    }
}

fn err(error: RunPodError) -> ProviderError {
    error_of(&error)
}

/// An answer without the id it should carry, as `failover` and `up` refuse
/// one today ("RunPod created no pod").
fn without_id(what: &str) -> ProviderError {
    error_of(&RunPodError::new(format!("RunPod created no {what}"), None))
}

impl Provider for RunPodProvider {
    fn name(&self) -> &'static str {
        "runpod"
    }

    fn pods(&self) -> BoxFuture<'_, viy_provider::Result<Vec<Pod>>> {
        Box::pin(async move {
            let pods = self.rp.list_pods().await.map_err(err)?;
            Ok(pods
                .iter()
                .map(pod_of)
                .filter(|p| p.state != Some(PodState::Terminated))
                .collect())
        })
    }

    fn pod<'a>(&'a self, id: &'a str) -> BoxFuture<'a, viy_provider::Result<Option<Pod>>> {
        Box::pin(async move {
            Ok(self.rp.get_pod(id).await.map_err(err)?.map(|v1| {
                // An answer without the id keeps the one asked for (kb d054).
                let mut pod = pod_of(&v1);
                if pod.id.is_empty() {
                    pod.id = id.to_string();
                }
                pod
            }))
        })
    }

    fn create<'a>(&'a self, request: &'a PodRequest) -> BoxFuture<'a, viy_provider::Result<Pod>> {
        Box::pin(async move {
            let created = self.rp.create_pod(self.body(request)).await.map_err(err)?;
            created
                .as_ref()
                .map(pod_of)
                .filter(|p| !p.id.is_empty())
                .ok_or_else(|| without_id("pod"))
        })
    }

    fn start<'a>(&'a self, id: &'a str) -> BoxFuture<'a, viy_provider::Result<()>> {
        Box::pin(async move { self.rp.start_pod(id).await.map_err(err) })
    }

    fn stop<'a>(&'a self, id: &'a str) -> BoxFuture<'a, viy_provider::Result<()>> {
        Box::pin(async move { self.rp.stop_pod(id).await.map_err(err) })
    }

    fn terminate<'a>(&'a self, id: &'a str) -> BoxFuture<'a, viy_provider::Result<()>> {
        Box::pin(async move { self.rp.delete_pod(id).await.map_err(err) })
    }

    fn logs<'a>(
        &'a self,
        id: &'a str,
        tail: u32,
        budget: Duration,
    ) -> BoxFuture<'a, viy_provider::Result<Vec<LogLine>>> {
        Box::pin(async move {
            let entries = self.rp.pod_logs(id, tail, budget).await.map_err(err)?;
            Ok(entries
                .into_iter()
                .map(|e| LogLine {
                    time: e.ts,
                    source: e.source,
                    text: e.line,
                })
                .collect())
        })
    }

    fn volumes(&self) -> Option<&dyn Volumes> {
        Some(self)
    }
}

impl Volumes for RunPodProvider {
    fn list(&self) -> BoxFuture<'_, viy_provider::Result<Vec<Volume>>> {
        Box::pin(async move {
            let volumes = self.rp.list_volumes().await.map_err(err)?;
            Ok(volumes.iter().map(volume_of).collect())
        })
    }

    fn get<'a>(&'a self, id: &'a str) -> BoxFuture<'a, viy_provider::Result<Option<Volume>>> {
        Box::pin(async move {
            Ok(self
                .rp
                .get_volume(id)
                .await
                .map_err(err)?
                .as_ref()
                .map(volume_of))
        })
    }

    fn create<'a>(
        &'a self,
        name: &'a str,
        size_gb: u64,
        region: &'a str,
    ) -> BoxFuture<'a, viy_provider::Result<Volume>> {
        Box::pin(async move {
            // The region's default tier, as `up` makes the primary.
            let made = self
                .rp
                .create_volume(name, size_gb, region, None)
                .await
                .map_err(err)?;
            made.as_ref()
                .map(volume_of)
                .filter(|v| !v.id.is_empty())
                .ok_or_else(|| without_id("volume"))
        })
    }

    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, viy_provider::Result<()>> {
        Box::pin(async move { self.rp.delete_volume(id).await.map_err(err) })
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
