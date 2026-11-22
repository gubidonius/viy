//! A scripted second GPU provider (WP 9.39, 9.40, kb d182): pods held in
//! memory, volumes only when the test gives one, no regions, every call
//! recorded. The pod commands run on it in the tests, to show that they
//! need nothing of RunPod's.

use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use viy_provider::BoxFuture;
use viy_provider::LogLine;
use viy_provider::Pod;
use viy_provider::PodRequest;
use viy_provider::PodState;
use viy_provider::Provider;
use viy_provider::ProviderError;
use viy_provider::Result;
use viy_provider::Volume;
use viy_provider::Volumes;

/// The provider's state: its pods, their log, the calls made.
#[derive(Debug, Default)]
struct Book {
    pods: Vec<Pod>,
    logs: Vec<LogLine>,
    calls: Vec<String>,
    /// How many `pod` calls answer with the pod's SSH endpoint hidden, as
    /// a booting pod's is.
    ssh_hidden_for: usize,
    created: usize,
    /// `None`: the provider has no volumes at all.
    volumes: Option<Vec<Volume>>,
    /// Every create is refused for capacity with this message.
    refusal: Option<String>,
}

/// The scripted provider. Its pods change as the calls say: `start` runs
/// a pod, `stop` stops it, `terminate` removes it, `create` adds one.
#[derive(Debug, Default)]
pub struct Scripted {
    book: Mutex<Book>,
}

impl Scripted {
    pub fn new(pods: impl IntoIterator<Item = Pod>) -> Self {
        let scripted = Self::default();
        scripted.book().pods = pods.into_iter().collect();
        scripted
    }

    /// The pod's log lines, oldest first.
    pub fn with_logs(self, lines: impl IntoIterator<Item = &'static str>) -> Self {
        self.book().logs = lines
            .into_iter()
            .map(|text| LogLine {
                time: None,
                source: Some("container".into()),
                text: text.into(),
            })
            .collect();
        self
    }

    /// A network volume, which also gives the provider volumes.
    pub fn with_volume(self, id: &str, name: &str, region: &str) -> Self {
        self.book()
            .volumes
            .get_or_insert_with(Vec::new)
            .push(Volume {
                id: id.into(),
                name: Some(name.into()),
                region: Some(region.into()),
                size_gb: Some(100),
            });
        self
    }

    /// Every create refused: no GPU of the kind asked for is free.
    pub fn refusing(self, message: &str) -> Self {
        self.book().refusal = Some(message.into());
        self
    }

    /// The first `n` `pod` calls give no SSH endpoint.
    pub fn ssh_after(self, n: usize) -> Self {
        self.book().ssh_hidden_for = n;
        self
    }

    /// The calls made so far, as `"stop pod1"`, `"pods"`.
    pub fn calls(&self) -> Vec<String> {
        self.book().calls.clone()
    }

    /// The pods it holds now.
    pub fn pods_now(&self) -> Vec<Pod> {
        self.book().pods.clone()
    }

    fn book(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn call(&self, what: String) -> std::sync::MutexGuard<'_, Book> {
        let mut book = self.book();
        book.calls.push(what);
        book
    }

    fn set_state(&self, verb: &str, id: &str, state: PodState, word: &str) -> Result<()> {
        let mut book = self.call(format!("{verb} {id}"));
        let pod = book
            .pods
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| missing(id))?;
        pod.state = Some(state);
        pod.state_name = Some(word.into());
        Ok(())
    }
}

/// A scripted pod: named, in a state, with an SSH endpoint when it runs.
pub fn pod(id: &str, name: &str, state: PodState) -> Pod {
    let word = match state {
        PodState::Running => "running",
        PodState::Stopped => "stopped",
        PodState::Starting => "starting",
        PodState::Terminated => "terminated",
        PodState::Unknown => "unknown",
    };
    Pod {
        id: id.into(),
        name: Some(name.into()),
        state: Some(state),
        state_name: Some(word.into()),
        cost_per_hour: Some(0.5),
        gpu: Some("scripted-gpu".into()),
        gpu_name: Some("Scripted GPU".into()),
        gpu_count: Some(1),
        region: Some("lab-1".into()),
        ssh: None,
        started_at: None,
        created_at: None,
        volume: None,
    }
}

fn missing(id: &str) -> ProviderError {
    ProviderError {
        message: format!("no pod {id}"),
        status: Some(404),
        capacity: false,
        transient: false,
        retry_after: None,
    }
}

impl Provider for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn pods(&self) -> BoxFuture<'_, Result<Vec<Pod>>> {
        Box::pin(async move { Ok(self.call("pods".into()).pods.clone()) })
    }

    fn pod<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Pod>>> {
        Box::pin(async move {
            let mut book = self.call(format!("pod {id}"));
            let hide = book.ssh_hidden_for > 0;
            book.ssh_hidden_for = book.ssh_hidden_for.saturating_sub(1);
            Ok(book.pods.iter().find(|p| p.id == id).cloned().map(|mut p| {
                if hide {
                    p.ssh = None;
                }
                p
            }))
        })
    }

    fn create<'a>(&'a self, request: &'a PodRequest) -> BoxFuture<'a, Result<Pod>> {
        Box::pin(async move {
            let mut book = self.call(format!("create {}", request.name));
            if let Some(message) = &book.refusal {
                return Err(ProviderError {
                    message: message.clone(),
                    status: Some(400),
                    capacity: true,
                    transient: false,
                    retry_after: None,
                });
            }
            book.created += 1;
            let mut made = pod(
                &format!("scripted{}", book.created),
                &request.name,
                PodState::Running,
            );
            made.region = Some(request.region.clone());
            made.volume.clone_from(&request.volume);
            book.pods.push(made.clone());
            Ok(made)
        })
    }

    fn start<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.set_state("start", id, PodState::Running, "running") })
    }

    fn stop<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.set_state("stop", id, PodState::Stopped, "stopped") })
    }

    fn terminate<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut book = self.call(format!("terminate {id}"));
            let before = book.pods.len();
            book.pods.retain(|p| p.id != id);
            if book.pods.len() == before {
                return Err(missing(id));
            }
            Ok(())
        })
    }

    fn logs<'a>(
        &'a self,
        id: &'a str,
        tail: u32,
        _budget: Duration,
    ) -> BoxFuture<'a, Result<Vec<LogLine>>> {
        Box::pin(async move {
            let book = self.call(format!("logs {id}"));
            let skip = book.logs.len().saturating_sub(tail as usize);
            Ok(book.logs[skip..].to_vec())
        })
    }

    fn volumes(&self) -> Option<&dyn Volumes> {
        self.book()
            .volumes
            .is_some()
            .then_some(self as &dyn Volumes)
    }
}

impl Volumes for Scripted {
    fn list(&self) -> BoxFuture<'_, Result<Vec<Volume>>> {
        Box::pin(async move {
            Ok(self
                .call("volumes".into())
                .volumes
                .clone()
                .unwrap_or_default())
        })
    }

    fn get<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Volume>>> {
        Box::pin(async move {
            let book = self.call(format!("volume {id}"));
            Ok(book.volumes.iter().flatten().find(|v| v.id == id).cloned())
        })
    }

    fn create<'a>(
        &'a self,
        name: &'a str,
        size_gb: u64,
        region: &'a str,
    ) -> BoxFuture<'a, Result<Volume>> {
        Box::pin(async move {
            let mut book = self.call(format!("create volume {name}"));
            let made = Volume {
                id: format!("vol-{name}"),
                name: Some(name.into()),
                region: Some(region.into()),
                size_gb: Some(size_gb),
            };
            book.volumes.get_or_insert_with(Vec::new).push(made.clone());
            Ok(made)
        })
    }

    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut book = self.call(format!("delete volume {id}"));
            if let Some(volumes) = &mut book.volumes {
                volumes.retain(|v| v.id != id);
            }
            Ok(())
        })
    }
}
