//! The pod commands: rpctl pod.py's `up`, `status`, `stop`, `down` and
//! `logs`, with `ensure_volume`, `_create_pod`, `_our_pods` and `_sweep`.
//! Each is a library call that reports as it goes ([`Report`]), so the
//! `viy` subcommand of that name and the chat's slash command run the same
//! code (kb d041).
//!
//! `up`, `stop` and `down` hold `pod.lock` as rp's do (`crate::lock`). A
//! chat that runs `/up` in the background holds it as the chat's own pid,
//! which `down`'s preemption skips; its `/down` therefore cancels its `/up`
//! first (drops the future, which drops the lock) and tells [`down`] so
//! with `cancelled_up`, which then watches as long as it does after
//! killing another process's up.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::sync::watch;
use tokio::time::Instant;
use viy_config::Config;
use viy_config::Paths;
use viy_config::config::require_api_key;
use viy_config::config::tunnel_enabled;
use viy_config::profiles;
use viy_exec::ExecBackend;
use viy_http::HttpClient;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;
use viy_protocol::Answer;
use viy_protocol::Question;
use viy_time::TimeProvider;

use crate::api::RunPod;
use crate::api::RunPodError;
use crate::api::proxy_url;
use crate::api::spoken;
use crate::bootreport::Outcome;
use crate::bootreport::Timeline;
use crate::engine;
use crate::failover::Creation;
use crate::failover::User;
use crate::idle;
use crate::lock::LockError;
use crate::lock::PodLock;
use crate::notice::Alert;
use crate::notice::Asker;
use crate::notice::Report;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::notice::say;
use crate::provider::RunPodProvider;
use crate::regions;
use crate::ssh::Ssh;
use crate::ssh::find_ssh_pubkey;
use crate::state;
use crate::state::key;
use crate::tracked;
use crate::tracked::ResolveError;
use crate::tunnel;
use crate::wait;
use crate::wait::PodWait;
use crate::wait::WaitError;
use viy_provider::Pod;
use viy_provider::PodState;
use viy_provider::Provider;
use viy_provider::ProviderError;

/// This machine's side of the pod commands.
#[derive(Clone, Copy)]
pub struct Env<'a> {
    pub paths: &'a Paths,
    pub exec: &'a dyn ExecBackend,
    /// RunPod's API and vLLM's /health.
    pub http: &'a Arc<dyn HttpClient>,
    pub time: &'a dyn TimeProvider,
    /// viy itself, which collects a boot's logs ([`wait::CAPTURE_SUBCOMMAND`]).
    pub exe: &'a Path,
}

impl<'a> Env<'a> {
    /// What the ssh and tunnel functions work with.
    pub fn ssh(&self, cfg: &'a Config) -> Ssh<'a> {
        Ssh {
            paths: self.paths,
            cfg,
            exec: self.exec,
        }
    }
}

/// How a pod command ended other than well.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// rpctl's `SystemExit(message)`: the message on stderr, exit status 1.
    #[error("{0}")]
    Exit(String),
    /// rpctl's `SystemExit(1)`: everything was said already.
    #[error("failed")]
    Failed,
    /// Ctrl-C: exit status 130.
    #[error("interrupted")]
    Interrupted,
    /// A RunPod call failed where rpctl lets the error reach `main`: on
    /// stderr, exit status 1.
    #[error(transparent)]
    RunPod(#[from] RunPodError),
    /// The same through the provider seam (kb d182).
    #[error(transparent)]
    Provider(#[from] viy_provider::ProviderError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<ResolveError> for CommandError {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::Provider(e) => Self::Provider(e),
            ResolveError::State(e) => Self::Io(e),
        }
    }
}

impl From<WaitError> for CommandError {
    fn from(error: WaitError) -> Self {
        match error {
            WaitError::Startup(message) => Self::Exit(message),
            WaitError::Interrupted => Self::Interrupted,
            WaitError::Io(e) => Self::Io(e),
        }
    }
}

impl From<LockError> for CommandError {
    fn from(error: LockError) -> Self {
        match error {
            busy @ LockError::Busy { .. } => Self::Exit(busy.to_string()),
            LockError::Io(e) => Self::Io(e),
        }
    }
}

pub(crate) fn alert(report: Reporter<'_>, title: &str, body: impl Into<String>, urgent: bool) {
    report(Report::Alert(Alert {
        title: title.into(),
        body: body.into(),
        urgent,
    }));
}

/// Ctrl-C as it comes, counted. Each front end feeds it: the terminal's
/// SIGINT, the chat's own key.
#[derive(Debug, Clone)]
pub struct Interrupt(watch::Receiver<u32>);

impl Interrupt {
    /// The sender to count with, and the interrupt it feeds.
    pub fn channel() -> (watch::Sender<u32>, Self) {
        let (tx, rx) = watch::channel(0);
        (tx, Self(rx))
    }

    /// One that never comes.
    pub fn never() -> Self {
        Self::channel().1
    }

    pub(crate) fn count(&self) -> u32 {
        *self.0.borrow()
    }

    /// Resolves once `pred` holds for the count.
    async fn when(&self, pred: impl FnMut(&u32) -> bool) {
        let mut rx = self.0.clone();
        if rx.wait_for(pred).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// The next Ctrl-C from now.
    pub fn next(&self) -> impl Future<Output = ()> + Send + 'static {
        self.after(1)
    }

    /// The `n`th Ctrl-C from now.
    pub fn after(&self, n: u32) -> impl Future<Output = ()> + Send + 'static {
        let this = self.clone();
        let at = this.count() + n;
        async move { this.when(|c| *c >= at).await }
    }
}

/// The RunPod client, with the config's key.
pub fn connect(env: &Env<'_>, cfg: &Config) -> Result<RunPod, CommandError> {
    let key = require_api_key(cfg, env.paths).map_err(CommandError::Exit)?;
    Ok(RunPod::configured(key, Arc::clone(env.http), cfg))
}

/// RunPod behind the provider seam (kb d182): what the commands that have
/// moved onto it call.
pub fn provider(env: &Env<'_>, cfg: &Config) -> Result<RunPodProvider, CommandError> {
    Ok(RunPodProvider::new(connect(env, cfg)?, cfg))
}

/// Our pod, several of that name said as rpctl does; found and cached by
/// RunPod's rules while rp shares `pod.json` (`tracked`).
pub(crate) async fn resolve(
    seam: &RunPodProvider,
    env: &Env<'_>,
    cfg: &Config,
    report: Reporter<'_>,
) -> Result<Option<Pod>, ResolveError> {
    tracked::resolve(seam.client(), env.paths, cfg, |many| {
        say(report, Tone::Warn, many.to_string());
    })
    .await
}

fn text<'v>(pod: &'v Value, key: &str) -> Option<&'v str> {
    pod.get(key).and_then(Value::as_str)
}

/// A number RunPod sent as a number or a string, as Python's `float()`
/// reads it; 0 when it is neither.
fn number(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Python's truthiness, for a value RunPod sent.
fn truthy(value: Option<&Value>) -> bool {
    crate::api::truthy(value.unwrap_or(&Value::Null))
}

fn cfg_at<'c>(cfg: &'c Config, key: &str) -> &'c str {
    cfg.get(key).unwrap_or("")
}

// ------------------------------------------------------------------ up

/// `viy up`: resumes or reuses our pod, else creates one, and waits for
/// the engine (vLLM, or Strata under ENGINE=strata) within `timeout`. A
/// Ctrl-C ends it, as rpctl's KeyboardInterrupt does: during the wait, a
/// booting pod's boot is recorded first,
/// and a second Ctrl-C ends that too. With `backup` (`viy up --backup`)
/// DATA_CENTER_ID is skipped: a new pod goes on the backup volume, and an
/// existing one is used only if it runs there (rpctl kb d035).
pub async fn up(
    env: &Env<'_>,
    cfg: &mut Config,
    timeout: Duration,
    backup: bool,
    interrupt: &Interrupt,
    report: Reporter<'_>,
    ask: Asker<'_>,
) -> Result<(), CommandError> {
    // The Ctrl-C that ends `up` at once: the next one, except during the
    // wait, whose own handling of the first records the boot.
    let abort_at = AtomicU32::new(interrupt.count() + 1);
    let aborted = interrupt.when(|c| *c >= abort_at.load(Ordering::SeqCst));
    tokio::select! {
        biased;
        result = up_until(env, cfg, timeout, backup, interrupt, &abort_at, report, ask) => result,
        () = aborted => Err(CommandError::Interrupted),
    }
}

/// The pod pod.json tracks runs the engine it was made with: a profile on
/// another engine (`viy model use` without `viy down`) needs its own pod,
/// or viy would serve and record one engine as the other. A pod pod.json
/// does not track, or tracks without an engine (rp's, vLLM's), is vLLM's.
fn check_pod_engine(env: &Env<'_>, cfg: &Config, pod_id: &str) -> Result<(), CommandError> {
    let saved = state::load(env.paths)?;
    if saved.get(key::POD_ID).and_then(Value::as_str) != Some(pod_id) {
        return Ok(());
    }
    let runs = saved
        .get(key::ENGINE)
        .and_then(Value::as_str)
        .unwrap_or(engine::EngineName::Vllm.as_str());
    let wanted = engine::name(cfg).as_str();
    if runs == wanted {
        return Ok(());
    }
    Err(CommandError::Exit(format!(
        "pod {pod_id} runs {runs}, but this profile's ENGINE is {wanted}. Terminate it first \
         (viy down), then viy up."
    )))
}

/// [`up`]'s work; `abort_at` is raised around the wait, and while
/// failover has new volumes to delete.
#[allow(clippy::too_many_arguments)]
async fn up_until(
    env: &Env<'_>,
    cfg: &mut Config,
    timeout: Duration,
    backup: bool,
    interrupt: &Interrupt,
    abort_at: &AtomicU32,
    report: Reporter<'_>,
    ask: Asker<'_>,
) -> Result<(), CommandError> {
    // An engine viy cannot run stops `up` before a pod is resumed or reused
    // and before a volume is made, so nothing billed is left behind.
    engine::for_config(cfg).map_err(CommandError::Exit)?;
    let _lock = PodLock::exclusive(env.paths, "up")?;
    let mut started = Instant::now();
    let mut started_wall = env.time.now();
    let mut timeline = Timeline::start(env.time);
    let runpod = Arc::new(provider(env, cfg)?);
    let seam: Arc<dyn Provider> = runpod.clone();
    // The backup volumes are RunPod's own (docs/provider-seam.md 2).
    let rp = runpod.client();
    let vllm_key = profiles::ensure_vllm_api_key(cfg, env.paths)?;

    // Never create a duplicate: adopt any existing pod with our name first.
    let existing = match resolve(&runpod, env, cfg, report).await {
        Ok(existing) => existing,
        Err(ResolveError::Provider(error)) => {
            // A 429 says when RunPod takes requests again (kb d140).
            let when = error.retry_after.map_or_else(
                || "in a moment".to_string(),
                |wait| format!("in {}", spoken(wait)),
            );
            say(
                report,
                Tone::Error,
                format!(
                    "Could not check RunPod for an existing pod, so not creating one (that \
                     would risk a duplicate). Try again {when}."
                ),
            );
            say(report, Tone::Dim, error.to_string());
            return Err(CommandError::Failed);
        }
        Err(error) => return Err(error.into()),
    };
    // A stopped pod whose host has no GPU free is terminated and `up` goes
    // on to create a pod, as with no pod at all (kb f175, f181).
    'adopt: {
        let Some(existing) = existing else {
            break 'adopt;
        };
        let cfg = &*cfg;
        if backup {
            check_backup_pod(env, cfg, &existing)?;
        }
        let pod_id = existing.id.clone();
        check_pod_engine(env, cfg, &pod_id)?;
        regions::note_pod(env.paths, &existing, regions::now(env.time))?;
        let booting = existing.state == Some(PodState::Stopped);
        let name = cfg_at(cfg, "POD_NAME");
        if booting {
            say(
                report,
                Tone::Dim,
                format!("Resuming stopped pod {pod_id} (named '{name}')..."),
            );
            match seam.start(&pod_id).await {
                Ok(()) => {
                    timeline.mark("resume requested", [("pod_id", json!(pod_id))]);
                    // A new run: reap counts its idle time and cap from now.
                    if let Err(error) = idle::forget(env.paths) {
                        say(
                            report,
                            Tone::Warn,
                            format!(
                                "Could not reset viy reap's count for the resumed pod: {error}"
                            ),
                        );
                    }
                }
                Err(error) if error.capacity => {
                    retire_stopped(env, cfg, &*seam, &existing, &error, report).await?;
                    timeline.mark("resume refused", [("pod_id", json!(pod_id))]);
                    break 'adopt;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            say(
                report,
                Tone::Dim,
                format!(
                    "Pod {pod_id} named '{name}' already exists ({}); reusing it.",
                    existing.state_name.as_deref().unwrap_or("None")
                ),
            );
        }
        if state::load(env.paths)?.get(key::TUNNEL_ONLY).is_none() && tunnel_enabled(cfg) {
            say(
                report,
                Tone::Warn,
                "This pod predates tunnel mode. The tunnel works on it, but the pod also still \
                 exposes an http port through RunPod's proxy; only recreating it (viy down && \
                 viy up) closes that route.",
            );
        }
        let w = wait_for(env, cfg, &seam, &pod_id);
        abort_at.store(interrupt.count() + 2, Ordering::SeqCst);
        let ready = wait::await_recorded(
            &w,
            timeout,
            started_wall,
            booting,
            &mut timeline,
            interrupt.next(),
            report,
        )
        .await?;
        abort_at.store(interrupt.count() + 1, Ordering::SeqCst);
        if ready.is_some() {
            let ready_wall = env.time.now();
            let took = started.elapsed().as_secs_f64();
            say(
                report,
                Tone::Ok,
                format!("Ready in {took:.0}s. Run viy chat"),
            );
            idle::touch(env.paths, env.time, true);
            alert(
                report,
                &format!("viy up: {} is serving", engine::label(cfg)),
                format!("ready in {took:.0}s"),
                false,
            );
            if booting {
                wait::report_boot(
                    &w,
                    started_wall,
                    ready_wall,
                    Outcome::Ready,
                    &timeline,
                    report,
                )
                .await;
            }
            let volume_id = existing.volume.as_deref().unwrap_or_default();
            note_serving(env, cfg, volume_id, report);
            retire_replaced(env, cfg, rp, volume_id, report).await;
            return Ok(());
        }
        if booting {
            let now = env.time.now();
            wait::report_boot(&w, started_wall, now, Outcome::Failed, &timeline, report).await;
        }
        say(
            report,
            Tone::Error,
            "Startup did not complete. Run viy logs. The pod remains allocated.",
        );
        alert(
            report,
            "viy up FAILED",
            "the pod is still billing; run viy logs",
            true,
        );
        return Err(CommandError::Failed);
    }

    let pubkey = find_ssh_pubkey(env.paths)?;
    if tunnel_enabled(cfg) && pubkey.is_none() {
        // sshd is the only route into a tunnel-only pod, and the key can
        // only be injected at creation, so making one now would strand it.
        return Err(CommandError::Exit(
            "Tunnel mode needs an SSH key, and there is none in ~/.ssh/.\n\
             The public key can only be injected when the pod is created, so a\n\
             pod made now would be reachable only through RunPod's web console.\n\
             \n  ssh-keygen -t ed25519      then re-run viy up\n\
             \n\
             Or accept RunPod's TLS-terminating proxy instead:  TUNNEL=0 viy up"
                .to_string(),
        ));
    }
    // --backup: the primary volume is neither looked up nor made.
    let (volume_id, volume_is_new) = if backup {
        (String::new(), false)
    } else {
        ensure_volume(env, cfg, &*seam, report).await?
    };
    timeline.mark("pod requested", []);
    let user = User {
        report,
        ask,
        interrupt,
        abort_at,
    };
    let creation = Creation::new(env, cfg, &runpod, &vllm_key, pubkey.as_deref(), user);
    let created = if backup {
        creation.run_on_backup().await?
    } else {
        creation.run(&volume_id, volume_is_new).await?
    };
    // Time spent answering failover questions, or waiting between retries
    // for a free GPU, is the user's or RunPod's, not the boot's.
    started += created.waited;
    started_wall += chrono::Duration::from_std(created.waited).unwrap_or_default();
    // This boot's copy only: the boot record shows where it ran, and the
    // next `viy up` (the chat's too, on the same config) starts from the
    // primary again.
    let mut booted = cfg.clone();
    booted.set("DATA_CENTER_ID", &created.region);
    let cfg = &booted;
    let (pod, volume_id, volume_is_new) = (created.pod, created.volume_id, created.volume_is_new);
    let pod_id = text(&pod, "id").unwrap_or_default().to_string();
    let cost = pod.get("costPerHr");
    timeline.mark("pod created", [("pod_id", json!(pod_id))]);
    // The transport is recorded now: the exposed ports and vLLM's bind
    // address are fixed for this pod's life, so a later TUNNEL flip must
    // not change how it is addressed.
    let created_at = env.time.now().timestamp_micros() as f64 / 1e6;
    state::save(
        env.paths,
        [
            (key::POD_ID, json!(pod_id)),
            (key::CREATED_AT, json!(created_at)),
            (key::COST_PER_HR, cost.cloned().unwrap_or(Value::Null)),
            (key::TUNNEL_ONLY, json!(tunnel_enabled(cfg))),
            (key::VOLUME_ID, json!(volume_id)),
        ],
    )?;
    // The engine the pod runs, for a later `up` under another profile;
    // vLLM's pod.json stays as rp wrote it.
    let made_with = engine::name(cfg);
    state::update(env.paths, |map| {
        if made_with == engine::EngineName::Vllm {
            map.shift_remove(key::ENGINE);
        } else {
            map.insert(key::ENGINE.into(), json!(made_with.as_str()));
        }
    })?;
    say(report, Tone::Ok, format!("  pod id:   {pod_id}"));
    if truthy(cost) {
        say(
            report,
            Tone::Warn,
            format!(
                "  cost:     ${:.2}/hr  <-- billing has started",
                number(cost)
            ),
        );
    }
    say(
        report,
        Tone::Plain,
        format!("  transport: {}", tracked::transport_label(env.paths, cfg)?),
    );
    say(
        report,
        Tone::Dim,
        match (engine::name(cfg), volume_is_new) {
            (engine::EngineName::Vllm, true) => {
                "\nFresh volume: this boot must download ~56GB of weights.\nWeights and \
                 successful compilation caches are retained for later boots."
            }
            (engine::EngineName::Vllm, false) => {
                "\nReusing the network volume; checking model and compilation caches.\nImage \
                 pulls, missing files, and new hardware/configuration can add startup time."
            }
            // Strata's first boot prepares the volume (kb d169).
            (engine::EngineName::Strata, true) => {
                "\nFresh volume: this boot prepares it (Strata's release, the model, its pack \
                 and draft head; about 8 minutes).\nLater boots start Strata from the volume."
            }
            (engine::EngineName::Strata, false) => {
                "\nReusing the network volume; Strata starts from it.\nImage pulls and a new \
                 release or model (prepared again) can add startup time."
            }
        },
    );
    let w = wait_for(env, cfg, &seam, &pod_id);
    abort_at.store(interrupt.count() + 2, Ordering::SeqCst);
    let ready = wait::await_recorded(
        &w,
        timeout,
        started_wall,
        true,
        &mut timeline,
        interrupt.next(),
        report,
    )
    .await?;
    abort_at.store(interrupt.count() + 1, Ordering::SeqCst);
    if ready.is_some() {
        let ready_wall = env.time.now();
        let took = started.elapsed().as_secs_f64();
        state::save(
            env.paths,
            [(
                key::LAST_STARTUP_SECONDS,
                json!((took * 10.0).round() / 10.0),
            )],
        )?;
        idle::touch(env.paths, env.time, true);
        say(
            report,
            Tone::Ok,
            format!(
                "{} is serving. Startup took {took:.0}s. Run viy chat",
                engine::label(cfg)
            ),
        );
        alert(
            report,
            &format!("viy up: {} is serving", engine::label(cfg)),
            format!("startup took {took:.0}s"),
            false,
        );
        wait::report_boot(
            &w,
            started_wall,
            ready_wall,
            Outcome::Ready,
            &timeline,
            report,
        )
        .await;
        note_serving(env, cfg, &volume_id, report);
        retire_replaced(env, cfg, rp, &volume_id, report).await;
        return Ok(());
    }
    let now = env.time.now();
    wait::report_boot(&w, started_wall, now, Outcome::Failed, &timeline, report).await;
    say(
        report,
        Tone::Error,
        "Startup did not complete. Run viy logs. The pod remains allocated; viy down releases \
         it.",
    );
    alert(
        report,
        "viy up FAILED",
        "the pod is still billing; run viy logs or viy down",
        true,
    );
    Err(CommandError::Failed)
}

/// `viy up --backup` with a pod of ours already there: one on a backup
/// volume is resumed or reused as `viy up` always does; any other is left
/// alone, since only a new pod can go on the backup (rpctl
/// `_check_backup_pod`).
pub fn check_backup_pod(env: &Env<'_>, cfg: &Config, existing: &Pod) -> Result<(), CommandError> {
    if regions::on_backup(env.paths, existing) {
        return Ok(());
    }
    Err(CommandError::Exit(format!(
        "Pod {} named '{}' already exists ({}) and is not on the backup volume, so viy up \
         --backup leaves it alone. viy down terminates it (the weights stay on the volume), then \
         viy up --backup makes one on the backup; viy up alone resumes or reuses it.",
        existing.id,
        cfg_at(cfg, "POD_NAME"),
        existing.state_name.as_deref().unwrap_or("None"),
    )))
}

fn wait_for<'a>(
    env: &Env<'a>,
    cfg: &'a Config,
    seam: &Arc<dyn Provider>,
    pod_id: &'a str,
) -> PodWait<'a> {
    PodWait {
        ssh: env.ssh(cfg),
        seam: Arc::clone(seam),
        http: &**env.http,
        time: env.time,
        pod_id,
        exe: env.exe,
    }
}

/// The network volume to boot from, and whether this call made it (the
/// one case where the weights are surely absent): NETWORK_VOLUME_ID if it
/// exists, else the volume named NETWORK_VOLUME_NAME, else a new one in
/// DATA_CENTER_ID. A volume found by name is remembered, and a pod lives
/// where its volume lives.
pub async fn ensure_volume(
    env: &Env<'_>,
    cfg: &mut Config,
    seam: &dyn Provider,
    report: Reporter<'_>,
) -> Result<(String, bool), CommandError> {
    let Some(volumes) = seam.volumes() else {
        return Err(CommandError::Exit(format!(
            "{} has no network volumes for the weights.",
            seam.name()
        )));
    };
    if let Some(id) = cfg.get("NETWORK_VOLUME_ID").filter(|v| !v.is_empty()) {
        match volumes.get(id).await {
            Ok(Some(volume)) => {
                let found = if volume.id.is_empty() { id } else { &volume.id };
                return Ok((found.to_string(), false));
            }
            Ok(None) => {}
            Err(_) => say(
                report,
                Tone::Warn,
                "Configured NETWORK_VOLUME_ID no longer exists; making a new one.",
            ),
        }
    }
    let name = cfg_at(cfg, "NETWORK_VOLUME_NAME").to_string();
    for volume in volumes.list().await? {
        if volume.name.as_deref() != Some(name.as_str()) {
            continue;
        }
        let region = cfg_at(cfg, "DATA_CENTER_ID").to_string();
        let Some(theirs) = volume.region.as_deref() else {
            // A pod must live where its volume lives; rpctl stops here too.
            return Err(CommandError::Exit(format!(
                "RunPod lists volume '{name}' without its data center, so there is no telling \
                 where the pod must run."
            )));
        };
        if theirs != region {
            say(
                report,
                Tone::Warn,
                format!(
                    "Volume '{name}' is in {theirs} but DATA_CENTER_ID={region}. Using the \
                     volume's region, since a pod must live where its volume lives."
                ),
            );
            cfg.set("DATA_CENTER_ID", theirs);
        }
        let id = volume.id.clone();
        profiles::persist(cfg, env.paths, "NETWORK_VOLUME_ID", &id)?;
        return Ok((id, false));
    }
    let size = cfg_at(cfg, "NETWORK_VOLUME_SIZE_GB");
    let region = cfg_at(cfg, "DATA_CENTER_ID").to_string();
    say(
        report,
        Tone::Dim,
        format!("Creating {size}GB network volume '{name}' in {region}..."),
    );
    let size_gb: u64 = size.trim().parse().map_err(|_| {
        CommandError::Exit(format!(
            "NETWORK_VOLUME_SIZE_GB must be a whole number, not {size:?}"
        ))
    })?;
    // The primary takes its region's default tier, as rp.
    let id = volumes.create(&name, size_gb, &region).await?.id;
    profiles::persist(cfg, env.paths, "NETWORK_VOLUME_ID", &id)?;
    say(report, Tone::Ok, format!("  volume id: {id}"));
    Ok((id, true))
}

/// A stopped pod RunPod will not start because its host's GPUs are taken
/// (`400`, kb f175): stopped pods keep their host (kb f174), so it cannot
/// be resumed now and may not be for a long time. It is logged with the
/// create refusals, its forward ended and the pod terminated, then the pod
/// state forgotten, so `up` goes on as with no pod.
async fn retire_stopped(
    env: &Env<'_>,
    cfg: &Config,
    seam: &dyn Provider,
    pod: &Pod,
    error: &ProviderError,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let pod_id = pod.id.as_str();
    let region = pod
        .region
        .as_deref()
        .unwrap_or_else(|| cfg_at(cfg, "DATA_CENTER_ID"));
    let volume_id = pod.volume.as_deref().unwrap_or_default();
    // An array of type ids, as the create refusals log theirs.
    let gpus = json!(pod.gpu.iter().collect::<Vec<_>>());
    regions::log_failure(
        env.paths, env.time, cfg, region, "resume", volume_id, &gpus, error,
    );
    say(
        report,
        Tone::Warn,
        format!("Pod {pod_id} cannot be resumed: its host has no GPU free now."),
    );
    say(report, Tone::Dim, error.to_string());
    // The forward goes before the state file, as in `down`, or its pid is
    // lost and the ssh holds the port the new pod's tunnel needs.
    let ssh = env.ssh(cfg);
    let mut said = |notice| report(Report::Notice(notice));
    tunnel::stop(&ssh, true, &mut said).await?;
    tunnel::kill_strays(&ssh).await?;
    match seam.terminate(pod_id).await {
        // Gone already: terminated by the reaper or the other tool.
        Err(error) if error.status != Some(404) => return Err(error.into()),
        _ => {}
    }
    state::clear(env.paths)?;
    say(
        report,
        Tone::Dim,
        format!("Terminated {pod_id}; creating a new pod."),
    );
    Ok(())
}

/// After readiness: the primary volume served, so its PRIMARY_IDLE_DAYS
/// countdown starts again (kb d178). Never fails `up`.
fn note_serving(env: &Env<'_>, cfg: &Config, volume_id: &str, report: Reporter<'_>) {
    // DATA_CENTER_ID as `up` ran it: the volume's own region (ensure_volume).
    let region = cfg_at(cfg, "DATA_CENTER_ID");
    if let Err(error) =
        regions::note_primary(env.paths, cfg, volume_id, region, regions::now(env.time))
    {
        say(
            report,
            Tone::Warn,
            format!("Could not record the volume's use for viy reap: {error}"),
        );
    }
}

/// After readiness only: a new backup that just served deletes the one it
/// replaced. Never fails `up`; a miss is retried on the next ready.
async fn retire_replaced(
    env: &Env<'_>,
    cfg: &Config,
    rp: &RunPod,
    volume_id: &str,
    report: Reporter<'_>,
) {
    let mut tell = |text: String| say(report, Tone::Dim, text);
    if let Err(error) = regions::retire_replaced(rp, env.paths, cfg, volume_id, &mut tell).await {
        say(
            report,
            Tone::Dim,
            format!("(old backup not deleted yet: {:?})", error.kind()),
        );
    }
}

// -------------------------------------------------------------- volume

/// What `viy volume` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeAction {
    List,
    Create,
    Delete,
}

/// `viy volume list|create|delete`: the weights network volume. Delete
/// asks first, and with nobody to ask deletes nothing.
pub async fn volume(
    env: &Env<'_>,
    cfg: &mut Config,
    action: VolumeAction,
    volume_id: Option<&str>,
    report: Reporter<'_>,
    ask: Asker<'_>,
) -> Result<(), CommandError> {
    let seam = provider(env, cfg)?;
    let Some(volumes) = seam.volumes() else {
        return Err(CommandError::Exit(format!(
            "{} has no network volumes.",
            seam.name()
        )));
    };
    let print = |line: String| say(report, Tone::Plain, line);
    // rp shows what is missing as Python's None.
    let or_none = |v: Option<String>| v.unwrap_or_else(|| "None".into());
    match action {
        VolumeAction::List => {
            let listed = volumes.list().await?;
            if listed.is_empty() {
                print("no network volumes".to_string());
            }
            for v in listed {
                print(format!(
                    "  {}  {:<24} {}GB  {}",
                    v.id,
                    or_none(v.name),
                    or_none(v.size_gb.map(|s| s.to_string())),
                    or_none(v.region)
                ));
            }
        }
        VolumeAction::Create => {
            let (id, created) = ensure_volume(env, cfg, &seam, report).await?;
            let existed = if created { "" } else { " (already existed)" };
            print(format!("volume: {id}{existed}"));
        }
        VolumeAction::Delete => {
            let configured = cfg.get("NETWORK_VOLUME_ID").filter(|v| !v.is_empty());
            let Some(id) = volume_id.filter(|v| !v.is_empty()).or(configured) else {
                return Err(CommandError::Exit(
                    "nothing to delete: pass an id or set NETWORK_VOLUME_ID".to_string(),
                ));
            };
            let id = id.to_string();
            let question = Question::yes_no(
                None,
                format!("Permanently delete volume {id} and its weights?"),
            );
            let answer = match ask(&question) {
                Some(asking) => asking.await,
                None => question.unattended,
            };
            if answer == Answer::No {
                print("aborted".to_string());
                return Ok(());
            }
            volumes.delete(&id).await?;
            regions::forget(env.paths, &id)?;
            if cfg.get("NETWORK_VOLUME_ID") == Some(id.as_str()) {
                profiles::persist(cfg, env.paths, "NETWORK_VOLUME_ID", "")?;
            }
            print(format!("deleted {id}"));
        }
    }
    Ok(())
}

// -------------------------------------------------------------- status

/// `viy status`: the pod's state, GPU, region, cost, endpoint and
/// transport, the tunnel, and whether vLLM answers.
pub async fn status(env: &Env<'_>, cfg: &Config, report: Reporter<'_>) -> Result<(), CommandError> {
    let seam = provider(env, cfg)?;
    let pod = match resolve(&seam, env, cfg, report).await {
        Ok(pod) => pod,
        Err(ResolveError::Provider(error)) => {
            say(
                report,
                Tone::Warn,
                "Could not reach RunPod to check status; local state left as-is.",
            );
            say(report, Tone::Dim, error.to_string());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    status_of(env, cfg, pod, report).await
}

/// [`status`]'s lines for the pod found, whatever the provider: the pod
/// comes from RunPod's tracking (`tracked`) today (kb d182).
pub(crate) async fn status_of(
    env: &Env<'_>,
    cfg: &Config,
    pod: Option<Pod>,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let lifetimes = regions::lifetimes(env.paths, cfg, regions::now(env.time));
    let Some(pod) = pod else {
        say(
            report,
            Tone::Dim,
            format!(
                "No pod named '{}' found. Run viy up",
                cfg_at(cfg, "POD_NAME")
            ),
        );
        for line in lifetimes {
            say(report, Tone::Plain, line);
        }
        return Ok(());
    };
    let pod_id = pod.id.as_str();
    let line = |text: String| say(report, Tone::Plain, text);
    let tunnelled = tracked::tunnel_only(env.paths, cfg)?;
    let url = if tunnelled {
        tracked::local_url(cfg)
    } else {
        proxy_url(pod_id, cfg_at(cfg, "VLLM_PORT"))
    };
    let gpu_name = pod.gpu_name.as_deref().unwrap_or("?");
    let count = pod.gpu_count.map_or("?".to_string(), |c| c.to_string());
    // rp shows a missing value as Python's None.
    let or_none = |v: &Option<String>| v.clone().unwrap_or_else(|| "None".into());
    line(format!("  id:        {pod_id}"));
    line(format!("  name:      {}", or_none(&pod.name)));
    line(format!("  state:     {}", or_none(&pod.state_name)));
    line(format!("  gpu:       {gpu_name} x{count}"));
    line(format!(
        "  region:    {}",
        pod.region
            .as_deref()
            .filter(|r| !r.is_empty())
            .unwrap_or("?")
    ));
    line(format!(
        "  cost:      ${:.2}/hr",
        pod.cost_per_hour.unwrap_or(0.0)
    ));
    if let Some(started) = &pod.started_at {
        line(format!("  started:   {started}"));
    }
    line(format!("  endpoint:  {url}"));
    line(format!(
        "  transport: {}",
        tracked::transport_label(env.paths, cfg)?
    ));
    if tunnelled {
        let mut said = |notice| report(Report::Notice(notice));
        tunnel::status(&env.ssh(cfg), &mut said).await?;
    }
    for text in lifetimes {
        line(text);
    }
    if pod.state != Some(viy_provider::PodState::Running) {
        return Ok(());
    }
    let request = Request::new(Method::GET, format!("{url}/health"))
        .header("User-Agent", user_agent::RP_REQUESTS)
        .header("Accept", "*/*")
        .timeouts(Duration::from_secs(10), Duration::from_secs(10));
    let served = match viy_http::send(&**env.http, request).await {
        Ok(response) if matches!(response.status, 200 | 401) => "ready".to_string(),
        Ok(response) => format!("not ready (HTTP {})", response.status),
        Err(error) => {
            let mut served = format!("not reachable ({})", wait::failure_name(&error));
            if tunnelled {
                served.push_str(" -- the tunnel carries this, so check it first");
            }
            served
        }
    };
    line(format!("  vllm:      {served}"));
    let limits = idle::limits(cfg);
    if let Some(last) = idle::last_activity(env.paths)
        && limits.ttl != 0.0
    {
        let minutes = (env.time.now() - last).as_seconds_f64() / 60.0;
        line(format!(
            "  idle:      {minutes:.0} min since this machine last streamed; the pod shuts \
             itself down after {:.0} min without tokens",
            limits.ttl / 60.0
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- stop

/// `viy stop`: stops the pod, which ends GPU billing and can be resumed.
pub async fn stop(env: &Env<'_>, cfg: &Config, report: Reporter<'_>) -> Result<(), CommandError> {
    stop_pod(env, cfg, false, report).await
}

/// `viy stop` for `viy reap` and `viy shutdown`: RunPod out of reach
/// fails, so they try again and flag it rather than take it for a stopped
/// pod.
pub async fn stop_confirmed(
    env: &Env<'_>,
    cfg: &Config,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    stop_pod(env, cfg, true, report).await
}

/// Pods of ours RunPod lists as neither stopped nor terminated: after the
/// shutdown's stop there should be none, or `down` ends them.
pub async fn running_pods(env: &Env<'_>, cfg: &Config) -> Result<Vec<String>, CommandError> {
    let seam = provider(env, cfg)?;
    let ids: HashSet<String> = state::load(env.paths)?
        .pod_id()
        .map(str::to_string)
        .into_iter()
        .collect();
    Ok(our_pods(&seam, cfg, &ids)
        .await?
        .into_iter()
        .filter(|p| p.state != Some(PodState::Stopped))
        .map(|p| p.id)
        .filter(|id| !id.is_empty())
        .collect())
}

async fn stop_pod(
    env: &Env<'_>,
    cfg: &Config,
    confirmed: bool,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let _lock = PodLock::exclusive(env.paths, "stop")?;
    let seam = provider(env, cfg)?;
    let pod = match resolve(&seam, env, cfg, report).await {
        Ok(pod) => pod,
        Err(ResolveError::Provider(error)) => {
            say(
                report,
                Tone::Warn,
                "Could not reach RunPod; nothing was stopped.",
            );
            say(report, Tone::Dim, error.to_string());
            return if confirmed {
                Err(CommandError::Failed)
            } else {
                Ok(())
            };
        }
        Err(error) => return Err(error.into()),
    };
    let Some(pod) = pod else {
        say(report, Tone::Dim, "No pod to stop.");
        return Ok(());
    };
    stop_on(env, cfg, &seam, &pod, report).await
}

/// Stops `pod` on `seam`, its forward first, and says what that costs.
pub(crate) async fn stop_on(
    env: &Env<'_>,
    cfg: &Config,
    seam: &dyn Provider,
    pod: &Pod,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let pod_id = pod.id.as_str();
    // Nothing left to forward to.
    let mut said = |notice| report(Report::Notice(notice));
    tunnel::stop(&env.ssh(cfg), true, &mut said).await?;
    seam.stop(pod_id).await?;
    say(
        report,
        Tone::Ok,
        format!("Stopped {pod_id}. GPU billing ends; the pod can be resumed with viy up."),
    );
    say(
        report,
        Tone::Dim,
        "Note: a stopped pod costs nothing but its network volume; if its GPU is taken meanwhile, \
         viy up makes a new pod.",
    );
    Ok(())
}

// ---------------------------------------------------------------- down

/// How long [`sweep`] tries in all.
const SWEEP_BUDGET: Duration = Duration::from_secs(300);

/// `viy down`: makes sure no pod of ours is running, starting, stopped or
/// about to be created. It comes before everything: an up or stop of
/// either tool that holds the pod lock is killed first, and success is
/// said only once RunPod confirms it. `cancelled_up`: the caller has just
/// cancelled an up of its own (module docs).
pub async fn down(
    env: &Env<'_>,
    cfg: &Config,
    cancelled_up: bool,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    down_on(env, cfg, &provider(env, cfg)?, cancelled_up, report).await
}

/// [`down`] on `seam`: nothing in it is RunPod's but the console it names
/// when it cannot confirm.
pub(crate) async fn down_on(
    env: &Env<'_>,
    cfg: &Config,
    seam: &dyn Provider,
    cancelled_up: bool,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    // Preemption blocks for up to 15 s plus a grace per kill; its notices
    // are said as they happen.
    let paths = env.paths.clone();
    let (tx, mut notices) = tokio::sync::mpsc::unbounded_channel();
    let preempting = tokio::task::spawn_blocking(move || {
        PodLock::preempt(&paths, |n| {
            let _ = tx.send(n);
        })
    });
    while let Some(notice) = notices.recv().await {
        say(report, Tone::Warn, notice.to_string());
    }
    let preempted = preempting.await.map_err(io::Error::other)?;
    let preempted = preempted?;
    let killed_up = preempted
        .killed
        .iter()
        .find(|h| h.verb == "up")
        .map(crate::lock::Holder::tool_name);
    let lock = preempted.lock;
    // The forward goes before the state file, or its pid is lost and the
    // ssh outlives the pod it pointed at.
    let ssh = env.ssh(cfg);
    let mut said = |notice| report(Report::Notice(notice));
    tunnel::stop(&ssh, true, &mut said).await?;
    tunnel::kill_strays(&ssh).await?;

    let mut ids: HashSet<String> = state::load(env.paths)?
        .pod_id()
        .map(str::to_string)
        .into_iter()
        .collect();
    // A killed up may have had a create request in flight, which RunPod
    // can finish after the client is gone: watch long enough to catch it.
    let killed = killed_up.or(cancelled_up.then_some("viy"));
    let settle = Duration::from_secs(if killed.is_some() { 60 } else { 10 });
    if let Some(tool) = killed {
        say(
            report,
            Tone::Dim,
            format!(
                "The killed {tool} up may already have asked RunPod for a pod, so watching {}s \
                 for one to appear.",
                settle.as_secs()
            ),
        );
    }
    let (done, error) = sweep(seam, cfg, &mut ids, settle, SWEEP_BUDGET, report).await;
    if let Some(error) = error {
        say(
            report,
            Tone::Error,
            "COULD NOT CONFIRM THAT NO POD IS RUNNING. Money may still be spent.",
        );
        say(report, Tone::Dim, error.to_string());
        say(
            report,
            Tone::Warn,
            format!(
                "Re-run viy down, or check https://console.runpod.io/pods for pods named '{}'.",
                cfg_at(cfg, "POD_NAME")
            ),
        );
        return Err(CommandError::Failed);
    }
    state::clear(env.paths)?;
    drop(lock);
    if done.is_empty() {
        say(
            report,
            Tone::Ok,
            "No pod was running or starting. Nothing is billing for a pod.",
        );
    } else {
        say(
            report,
            Tone::Ok,
            format!(
                "Terminated {}. No pod is running or starting; GPU and pod-disk billing has \
                 ended.",
                done.join(", ")
            ),
        );
    }
    say(
        report,
        Tone::Dim,
        "Model and startup caches stay on the network volume for the next viy up.",
    );
    Ok(())
}

/// Every live pod that could be ours: named POD_NAME, or one we tracked
/// (the provider lists none terminated).
async fn our_pods(
    seam: &dyn Provider,
    cfg: &Config,
    ids: &HashSet<String>,
) -> Result<Vec<Pod>, ProviderError> {
    let name = cfg_at(cfg, "POD_NAME");
    Ok(seam
        .pods()
        .await?
        .into_iter()
        .filter(|p| p.name.as_deref() == Some(name) || (!p.id.is_empty() && ids.contains(&p.id)))
        .collect())
}

/// Terminates every pod of ours, then lists again until RunPod has shown
/// none for `settle` straight. The pods terminated, and the error that
/// stopped it, when it could not confirm: a pod may still be billing.
async fn sweep(
    seam: &dyn Provider,
    cfg: &Config,
    ids: &mut HashSet<String>,
    settle: Duration,
    budget: Duration,
    report: Reporter<'_>,
) -> (Vec<String>, Option<ProviderError>) {
    let started = Instant::now();
    let mut clear_since: Option<Instant> = None;
    let mut done: Vec<String> = Vec::new();
    let mut error: Option<ProviderError> = None;
    let mut delay = Duration::from_secs(2);
    while started.elapsed() < budget {
        let pods = match our_pods(seam, cfg, ids).await {
            Ok(pods) => {
                error = None;
                Some(pods)
            }
            Err(e) if !e.transient => return (done, Some(e)),
            Err(e) => {
                error = Some(e);
                None
            }
        };
        if pods.as_ref().is_some_and(Vec::is_empty) {
            let since = *clear_since.get_or_insert_with(Instant::now);
            let Some(left) = settle.checked_sub(since.elapsed()).filter(|l| !l.is_zero()) else {
                return (done, None);
            };
            report(Report::Passing(format!(
                "  confirming nothing comes back... {:2}s ",
                left.as_secs_f64().ceil()
            )));
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        clear_since = None;
        for pod in pods.unwrap_or_default() {
            if pod.id.is_empty() {
                continue;
            }
            let id = pod.id.clone();
            ids.insert(id.clone());
            if let Err(e) = seam.terminate(&id).await
                && e.status != Some(404)
            {
                let transient = e.transient;
                error = Some(e);
                if !transient {
                    return (done, error);
                }
                continue;
            }
            if !done.contains(&id) {
                say(
                    report,
                    Tone::Ok,
                    format!(
                        "  terminated {id} (was {})",
                        pod.state_name.as_deref().unwrap_or("None")
                    ),
                );
                done.push(id);
            }
        }
        if let Some(e) = &error {
            let first = e.to_string().lines().next().unwrap_or_default().to_string();
            report(Report::Passing(format!(
                "  RunPod API error, retrying in {}s: {first}",
                delay.as_secs()
            )));
        }
        tokio::time::sleep(delay).await;
        delay = if error.is_some() {
            (delay * 2).min(Duration::from_secs(20))
        } else {
            Duration::from_secs(2)
        };
    }
    let error = error.unwrap_or_else(|| {
        crate::provider::error_of(&RunPodError::new(
            format!("pods still listed after {}s", budget.as_secs()),
            None,
        ))
    });
    (done, Some(error))
}

// ---------------------------------------------------------------- logs

/// `viy logs`: RunPod's platform and container log of our pod, secrets
/// masked, available before SSH; with `follow`, every 2 s until
/// `interrupted`.
pub async fn logs(
    env: &Env<'_>,
    cfg: &Config,
    follow: bool,
    interrupted: impl Future<Output = ()>,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let seam = provider(env, cfg)?;
    let pod = match resolve(&seam, env, cfg, report).await {
        Ok(pod) => pod,
        Err(ResolveError::Provider(error)) => {
            say(report, Tone::Warn, "Could not reach RunPod.");
            say(report, Tone::Dim, error.to_string());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let Some(pod) = pod else {
        say(report, Tone::Dim, "No pod tracked.");
        return Ok(());
    };
    let pod_id = pod.id.as_str();
    say(
        report,
        Tone::Dim,
        "RunPod platform + container logs (available before SSH):",
    );
    let print = async {
        let mut seen: HashSet<(Option<String>, Option<String>, String)> = HashSet::new();
        loop {
            let entries = seam.logs(pod_id, 200, Duration::from_secs(8)).await?;
            for entry in &entries {
                let identity = (entry.time.clone(), entry.source.clone(), entry.text.clone());
                if !seen.insert(identity) {
                    continue;
                }
                say(
                    report,
                    Tone::Plain,
                    format!(
                        "{} [{}] {}",
                        entry.time.as_deref().unwrap_or(""),
                        entry.source.as_deref().unwrap_or("?"),
                        wait::redact_log(&entry.text, cfg)
                    ),
                );
            }
            if !follow {
                return Ok::<(), ProviderError>(());
            }
            if seen.len() > 5000 {
                seen = entries
                    .into_iter()
                    .map(|e| (e.time, e.source, e.text))
                    .collect();
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };
    let result = tokio::select! {
        biased;
        () = interrupted => return Ok(()),
        result = print => result,
    };
    if let Err(error) = result {
        say(report, Tone::Warn, error.to_string());
        say(
            report,
            Tone::Dim,
            format!("Web console logs: https://console.runpod.io/pods/{pod_id}"),
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "commands_tests.rs"]
mod tests;
