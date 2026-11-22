//! The seam between viy's pod commands and a GPU rental provider (kb
//! d182, docs/provider-seam.md). A provider rents a pod with a GPU,
//! reports on it, and stops, starts and terminates it; what only some
//! providers have (persistent volumes) is a capability that may be absent.
//! RunPod is the first provider (`viy_pod::provider`); its region failover
//! and stock stay on its side.
//!
//! The types say what the pod commands decide on and nothing a provider
//! names its own way: a pod's state, its price, its GPU, its region, where
//! its SSH answers, the volume it mounts; and what an engine asks for.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// A provider call's future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a pod is, in the terms the commands act on. Whether it serves
/// yet is the readiness check's to say, not this: RunPod reports a booting
/// pod, and one in error, as running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodState {
    /// Asked for, not yet running (a provider that tells it apart).
    Starting,
    Running,
    /// Kept, not billing for compute; can be started again.
    Stopped,
    Terminated,
    /// A state the provider names that viy does not know.
    Unknown,
}

/// One pod as the provider reports it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pod {
    pub id: String,
    /// `None` when the provider gave none.
    pub name: Option<String>,
    pub state: Option<PodState>,
    /// The state in the provider's own word, for what viy shows.
    pub state_name: Option<String>,
    /// What the pod costs an hour while it runs, in USD.
    pub cost_per_hour: Option<f64>,
    /// The GPU's type, as the provider names it in a request.
    pub gpu: Option<String>,
    /// The GPU as viy shows it: the provider's display name, else its type.
    pub gpu_name: Option<String>,
    pub gpu_count: Option<u32>,
    pub region: Option<String>,
    /// Where its SSH answers, while it runs.
    pub ssh: Option<(String, u16)>,
    pub started_at: Option<String>,
    pub created_at: Option<String>,
    /// The persistent volume it mounts.
    pub volume: Option<String>,
}

/// What an engine asks a provider for. The provider turns it into its own
/// request; what only it has a setting for (RunPod's cloud tier) it reads
/// from its own configuration. Its `Debug` leaves out the environment's
/// values: the API keys and tokens are among them.
#[derive(Clone, PartialEq, Default)]
pub struct PodRequest {
    pub name: String,
    pub image: String,
    pub entrypoint: Vec<String>,
    pub command: Vec<String>,
    /// In order; the secrets among them no notice may show.
    pub env: Vec<(String, String)>,
    /// GPU types in the order wanted.
    pub gpu_types: Vec<String>,
    /// As the config says it, sent as it is (rp sends what it reads): a
    /// provider checks it is at least 1 before it asks for a pod.
    pub gpu_count: i64,
    pub region: String,
    /// The persistent volume, mounted at `/workspace`.
    pub volume: Option<String>,
    pub disk_gb: i64,
    /// `8000/http`, `22/tcp`.
    pub ports: Vec<String>,
    /// Acceptable host driver versions (CUDA), any when empty.
    pub cuda: Vec<String>,
    /// Host RAM per GPU, in GB.
    pub min_ram_gb: Option<u64>,
}

impl std::fmt::Debug for PodRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("PodRequest")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("entrypoint", &self.entrypoint)
            .field("command", &format_args!("<{} parts>", self.command.len()))
            .field("env", &env)
            .field("gpu_types", &self.gpu_types)
            .field("gpu_count", &self.gpu_count)
            .field("region", &self.region)
            .field("volume", &self.volume)
            .field("disk_gb", &self.disk_gb)
            .field("ports", &self.ports)
            .field("cuda", &self.cuda)
            .field("min_ram_gb", &self.min_ram_gb)
            .finish()
    }
}

/// A persistent volume.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Volume {
    pub id: String,
    /// `None` when the provider gave none.
    pub name: Option<String>,
    pub region: Option<String>,
    pub size_gb: Option<u64>,
}

/// One line of a pod's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub time: Option<String>,
    /// Who wrote it, where the provider says: RunPod's `system` (its own
    /// steps) or `container` (the pod's output).
    pub source: Option<String>,
    pub text: String,
}

/// What went wrong, with what the commands decide on.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    pub message: String,
    pub status: Option<u16>,
    /// No GPU of the kind asked for is free (failover, a refused resume).
    pub capacity: bool,
    /// Worth asking again (a 5xx, a dropped connection).
    pub transient: bool,
    /// Rate limited: when the provider takes requests again.
    pub retry_after: Option<Duration>,
}

pub type Result<T> = std::result::Result<T, ProviderError>;

/// A GPU rental provider.
pub trait Provider: Send + Sync {
    /// Its name, as the config says it (`runpod`).
    fn name(&self) -> &'static str;
    /// Every pod of the account that is not terminated, ours or not.
    fn pods(&self) -> BoxFuture<'_, Result<Vec<Pod>>>;
    /// One pod; `None` when the provider has none by that id.
    fn pod<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Pod>>>;
    fn create<'a>(&'a self, request: &'a PodRequest) -> BoxFuture<'a, Result<Pod>>;
    fn start<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>>;
    fn stop<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>>;
    fn terminate<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>>;
    /// The last `tail` lines of the pod's log, oldest first. The read stops
    /// at `budget` once a line has come; on a silent log it may wait past
    /// it for the first line or a short quiet gap.
    fn logs<'a>(
        &'a self,
        id: &'a str,
        tail: u32,
        budget: Duration,
    ) -> BoxFuture<'a, Result<Vec<LogLine>>>;
    /// Persistent volumes, if the provider has them.
    fn volumes(&self) -> Option<&dyn Volumes> {
        None
    }
    /// Its regions' GPU stock, if it shows one: what failover ranks.
    fn regions(&self) -> Option<&dyn Regions> {
        None
    }
}

/// A region a pod can mount a network volume in, and the GPU types it
/// shows free there, in the provider's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionStock {
    pub region: String,
    pub free: Vec<String>,
}

/// One GPU type in one region, by its hosts' CUDA driver: free on a host
/// at or above the floor asked for (`fit`), and free on one of the older
/// versions asked about (`old`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CudaStock {
    pub region: String,
    pub gpu: String,
    pub fit: bool,
    pub old: bool,
}

/// A provider's GPU stock by region, as failover and `viy backup` read it.
pub trait Regions: Send + Sync {
    /// Every region that takes a network volume and lists its stock, with
    /// the GPU types it shows free.
    fn stock(&self) -> BoxFuture<'_, Result<Vec<RegionStock>>>;
    /// The stock of `gpus` in `regions` by CUDA driver: at or above `floor`,
    /// and on any of `older`.
    fn cuda_stock<'a>(
        &'a self,
        regions: &'a [String],
        gpus: &'a [String],
        floor: &'a str,
        older: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<CudaStock>>>;
}

/// A provider's persistent volumes.
pub trait Volumes: Send + Sync {
    fn list(&self) -> BoxFuture<'_, Result<Vec<Volume>>>;
    fn get<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Volume>>>;
    /// A volume in the region's default storage: the primary's. Volumes the
    /// provider's own failover makes (RunPod's backup tier) are its own.
    fn create<'a>(
        &'a self,
        name: &'a str,
        size_gb: u64,
        region: &'a str,
    ) -> BoxFuture<'a, Result<Volume>>;
    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>>;
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
