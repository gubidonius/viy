//! Creating the pod: rpctl pod.py's `_create_pod` with its `attempt` and
//! `_ask`, and regions.py's `failover` and `_grab`.
//!
//! The success path is the single create request it always was. It asks
//! for GPU_PREFERRED's types only (the A100s); when DATA_CENTER_ID has none
//! free, the user is asked whether to try the backup volume for them. When
//! the backup has none either, or there is no backup yet, RunPod's stock is
//! sampled and the user asked whether to move the backup to the regions
//! that showed a preferred type free. Declined, or nowhere free, the other
//! types are asked for in DATA_CENTER_ID and then in the backup's region,
//! the cheapest first as GPU_TYPE_IDS orders them; and when no region
//! showed a preferred type, the move is offered once more for the regions
//! that showed any of our types (kb d138, d147). With no GPU_PREFERRED,
//! as rpctl: each region gets one request for every type, and the move
//! goes to the regions that showed any type free, the cheapest type
//! first. On yes each of
//! them gets a new volume and a pod request, refusals are retried for
//! FAILOVER_RETRY_MIN (the primary and the old backup too), and the first
//! region to take the pod wins. Volumes no pod used are deleted at once,
//! also on Ctrl-C; the old backup only once `viy up` serves on the new one
//! (`regions::retire_replaced`).
//!
//! Questions go to an [`Asker`]; with nobody to ask, the backup is tried
//! and nothing is made or deleted.
//!
//! `viy up --backup` ([`Creation::run_on_backup`]) starts at the backup
//! step, FAILOVER or not: the backup is tried without a question, and
//! DATA_CENTER_ID is never asked, for any type, nor retried (rpctl kb d035).

use std::collections::HashMap;
use std::collections::HashSet;
use std::pin::pin;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;
use viy_config::Config;
use viy_config::config::gpu_tiers;
use viy_config::config::gpu_type_list;
use viy_protocol::Answer;
use viy_protocol::Choices;
use viy_protocol::Question;
use viy_provider::Pod;
use viy_provider::PodRequest;
use viy_provider::Provider;
use viy_provider::ProviderError;

use crate::api::RunPod;
use crate::api::RunPodError;
use crate::commands::CommandError;
use crate::commands::Env;
use crate::commands::Interrupt;
use crate::commands::alert;
use crate::engine;
use crate::notice::Asker;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::notice::say;
use crate::provider::RunPodProvider;
use crate::provider::error_of;
use crate::regions;
use crate::regions::RETRY_GAP_S;
use crate::regions::id;
use crate::regions::short;

/// The pod a region took.
#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub pod: Pod,
    pub volume_id: String,
    /// The volume is new and empty: the weights must be downloaded.
    pub volume_is_new: bool,
    /// Where the pod runs: DATA_CENTER_ID, or the backup's region.
    pub region: String,
    /// Time spent on the user's answers and between retries for a free
    /// GPU: the user's or RunPod's, not the boot's.
    pub waited: Duration,
}

/// Which of GPU_TYPE_IDS a pod request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// Every type: a region's one request without GPU_PREFERRED, and
    /// every request of the backup's move to where any type is free.
    Every,
    /// GPU_PREFERRED's types.
    Preferred,
    /// The types GPU_PREFERRED leaves.
    Others,
}

/// Why failover ended without a pod.
enum Stop {
    /// The user said no; nothing was created or deleted.
    Declined(String),
    /// No region took the pod.
    NoCapacity(String),
    /// A refusal no other region can fix, as the provider said it.
    Refused(ProviderError),
    Command(CommandError),
}

impl From<CommandError> for Stop {
    fn from(error: CommandError) -> Self {
        match error {
            CommandError::RunPod(e) => Self::Refused(error_of(&e)),
            CommandError::Provider(e) => Self::Refused(e),
            other => Self::Command(other),
        }
    }
}

impl From<std::io::Error> for Stop {
    fn from(error: std::io::Error) -> Self {
        Self::Command(error.into())
    }
}

/// What failover won: the pod, its region and volume, whether that volume
/// is new.
struct Won {
    pod: Pod,
    region: String,
    volume_id: String,
    created: bool,
}

/// The user's side of a pod creation: what it says, who answers its
/// questions, and Ctrl-C.
#[derive(Clone, Copy)]
pub struct User<'a> {
    pub report: Reporter<'a>,
    pub ask: Asker<'a>,
    pub interrupt: &'a Interrupt,
    /// The Ctrl-C at which `up` ends at once; raised while new volumes
    /// exist, so that they are deleted first.
    pub abort_at: &'a AtomicU32,
}

/// One pod creation by `viy up`.
pub struct Creation<'a> {
    env: &'a Env<'a>,
    cfg: &'a Config,
    /// The provider the pods are asked of.
    seam: &'a dyn Provider,
    /// RunPod's own side, for failover: the backup volumes and
    /// backups.json. `None` on another provider: one request, no failover.
    runpod: Option<&'a RunPodProvider>,
    vllm_key: &'a str,
    pubkey: Option<&'a str>,
    user: User<'a>,
    /// DATA_CENTER_ID.
    primary: String,
    /// GPU_PREFERRED's types and the others, when it splits GPU_TYPE_IDS.
    tiers: Option<(Vec<String>, Vec<String>)>,
    /// The engine's pod request, built once; each attempt sets its region,
    /// volume and GPU types.
    request: PodRequest,
    /// Whether a request was sent, which "Creating pod in" announces.
    tried: bool,
    waited: Duration,
    /// `viy up --backup` asked for the backup: it is tried without a
    /// question, and DATA_CENTER_ID is neither asked nor retried.
    requested: bool,
}

impl<'a> Creation<'a> {
    pub fn new(
        env: &'a Env<'a>,
        cfg: &'a Config,
        seam: &'a dyn Provider,
        runpod: Option<&'a RunPodProvider>,
        vllm_key: &'a str,
        pubkey: Option<&'a str>,
        user: User<'a>,
    ) -> Self {
        Self {
            env,
            cfg,
            seam,
            runpod,
            vllm_key,
            pubkey,
            user,
            primary: cfg.get("DATA_CENTER_ID").unwrap_or("").to_string(),
            tiers: gpu_tiers(cfg).map(|(preferred, others)| {
                let owned = |gpus: Vec<&str>| gpus.into_iter().map(str::to_string).collect();
                (owned(preferred), owned(others))
            }),
            request: PodRequest::default(),
            tried: false,
            waited: Duration::ZERO,
            requested: false,
        }
    }

    fn say(&self, tone: Tone, text: impl Into<String>) {
        say(self.user.report, tone, text);
    }

    /// Says a failed write of backups.json and goes on: once a pod exists,
    /// or while cleaning up, the record is not worth losing the pod over.
    /// What it missed, `viy reap` and the next `viy up` mend.
    fn keep(&self, written: std::io::Result<()>) {
        if let Err(error) = written {
            self.say(
                Tone::Dim,
                format!("(backups.json not updated: {:?})", error.kind()),
            );
        }
    }

    /// Whether a refusal for capacity goes on to the backup: FAILOVER, on
    /// RunPod, whose backup volumes it moves.
    fn failover_on(&self) -> bool {
        self.runpod.is_some() && regions::failover_enabled(self.cfg)
    }

    /// RunPod's side, for the stock and the backup volumes; failover runs
    /// only there ([`Self::failover_on`]).
    fn runpod(&self) -> Result<&'a RunPodProvider, Stop> {
        self.runpod.ok_or_else(|| {
            Stop::Command(CommandError::Exit(format!(
                "{} has no backup volumes to fail over to.",
                self.seam.name()
            )))
        })
    }

    fn rp(&self) -> Result<&'a RunPod, Stop> {
        Ok(self.runpod()?.client())
    }

    /// What a region is asked for first.
    fn first(&self) -> Ask {
        if self.tiers.is_some() {
            Ask::Preferred
        } else {
            Ask::Every
        }
    }

    /// Creates the pod in DATA_CENTER_ID, and when that region has no GPU
    /// free offers the backup region. Ends with everything said when no
    /// region takes the pod.
    pub async fn run(
        mut self,
        volume_id: &str,
        volume_is_new: bool,
    ) -> Result<Created, CommandError> {
        let primary = self.primary.clone();
        // Only the region, volume and GPU types differ between requests.
        self.request = engine::for_config(self.cfg)
            .map_err(CommandError::Exit)?
            .pod_request(self.cfg, self.vllm_key, volume_id, self.pubkey)
            .map_err(|e| CommandError::Exit(e.to_string()))?;
        if self.env.paths.rental != viy_config::Rental::Runpod {
            self.request.env.push((
                "NEBIUS_ENGINE_PORT".into(),
                self.cfg.get("VLLM_PORT").unwrap_or("8000").into(),
            ));
        }
        let first = self.first();
        let mut refused = match self
            .attempt(&primary, volume_id, "primary", false, first)
            .await
        {
            Ok(pod) => return Ok(self.created(pod, volume_id, volume_is_new, &primary)),
            Err(error) => error,
        };
        while self.env.paths.rental != viy_config::Rental::Runpod && refused.capacity {
            let began = Instant::now();
            if !crate::rental::retry_capacity(self.env, self.user.report, self.user.ask).await? {
                return Err(refused.into());
            }
            self.waited += began.elapsed();
            match self
                .attempt(&primary, volume_id, "primary", false, first)
                .await
            {
                Ok(pod) => return Ok(self.created(pod, volume_id, volume_is_new, &primary)),
                Err(error) => refused = error,
            }
        }
        if self.env.paths.rental != viy_config::Rental::Runpod {
            return Err(refused.into());
        }
        // With no backup to offer, the other types follow at once.
        if first == Ask::Preferred && refused.capacity && !self.failover_on() {
            refused = match self
                .attempt(&primary, volume_id, "primary", false, Ask::Others)
                .await
            {
                Ok(pod) => return Ok(self.created(pod, volume_id, volume_is_new, &primary)),
                Err(error) => error,
            };
        }
        let (message, capacity) = if refused.capacity && self.failover_on() {
            match self.failover(volume_id).await {
                Ok(won) if won.region == primary => {
                    self.say(
                        Tone::Ok,
                        format!(
                            "  pod created in {} on the primary volume after all",
                            won.region
                        ),
                    );
                    return Ok(self.created(won.pod, &won.volume_id, volume_is_new, &primary));
                }
                Ok(won) => {
                    // The boot record shows where this boot ran; the next
                    // `viy up` starts from the primary again.
                    self.say(
                        Tone::Ok,
                        format!(
                            "  pod created in {} on the backup volume {}",
                            won.region, won.volume_id
                        ),
                    );
                    return Ok(self.created(won.pod, &won.volume_id, won.created, &won.region));
                }
                Err(stop) => self.stopped(stop)?,
            }
        } else {
            (refused.to_string(), refused.capacity)
        };
        self.fail(message, capacity)
    }

    /// `viy up --backup`: failover from its backup step, DATA_CENTER_ID
    /// never asked (rpctl `_create_pod` with `backup`). The region it ran
    /// in is the boot's only; the next `viy up` starts from the primary.
    pub async fn run_on_backup(mut self) -> Result<Created, CommandError> {
        self.requested = true;
        // Only the region, volume and GPU types differ between requests.
        self.request = engine::for_config(self.cfg)
            .map_err(CommandError::Exit)?
            .pod_request(self.cfg, self.vllm_key, "", self.pubkey)
            .map_err(|e| CommandError::Exit(e.to_string()))?;
        let (message, capacity) = match self.failover("").await {
            Ok(won) => {
                self.say(
                    Tone::Ok,
                    format!(
                        "  pod created in {} on the backup volume {}",
                        won.region, won.volume_id
                    ),
                );
                return Ok(self.created(won.pod, &won.volume_id, won.created, &won.region));
            }
            Err(stop) => self.stopped(stop)?,
        };
        self.fail(message, capacity)
    }

    /// How failover ended without a pod: the message to show and whether
    /// it was for lack of capacity, or the error to end `up` with at once.
    fn stopped(&self, stop: Stop) -> Result<(String, bool), CommandError> {
        match stop {
            Stop::Declined(message) => {
                self.say(
                    Tone::Dim,
                    format!("{message} No pod was created; nothing is billing."),
                );
                Err(CommandError::Failed)
            }
            Stop::Command(error) => Err(error),
            Stop::NoCapacity(message) => Ok((message, true)),
            Stop::Refused(error) => Ok((error.to_string(), error.capacity)),
        }
    }

    /// Says why no pod was created, and what to try.
    fn fail(&self, message: String, capacity: bool) -> Result<Created, CommandError> {
        let primary = &self.primary;
        self.say(Tone::Error, message);
        let hint = if capacity {
            let first = if self.requested || self.failover_on() {
                "No region took the pod".to_string()
            } else if self.runpod.is_none() {
                format!(
                    "No GPU free in {primary} ({} has no backup volumes)",
                    self.seam.name()
                )
            } else {
                format!("No GPU free in {primary} (FAILOVER=0: no backup offered)")
            };
            if self.runpod.is_some() {
                format!(
                    "{first}. Retry in a minute (stock flickers); viy backup shows RunPod's stock of our GPU types."
                )
            } else {
                format!("{first}. Retry in a minute.")
            }
        } else {
            let who = if self.runpod.is_some() {
                "RunPod"
            } else {
                self.seam.name()
            };
            format!(
                "Common causes: insufficient {who} credit, or a request {who} rejects (see the \
                 message above)."
            )
        };
        self.say(
            Tone::Warn,
            format!(
                "\n{hint}\nEvery refused pod creation is logged in {}",
                self.env.paths.up_failures_file().display()
            ),
        );
        Err(CommandError::Failed)
    }

    fn created(&self, pod: Pod, volume_id: &str, volume_is_new: bool, region: &str) -> Created {
        Created {
            pod,
            volume_id: volume_id.to_string(),
            volume_is_new,
            region: region.to_string(),
            waited: self.waited,
        }
    }

    /// One pod request in `region` on `volume_id` for the types of `ask`,
    /// logged when refused. A capacity refusal where the free GPUs run only newer CUDA drivers
    /// than CUDA_VERSIONS lists is asked again without the exact version.
    /// `quiet` leaves out the per-region notices of a retry round.
    async fn attempt(
        &mut self,
        region: &str,
        volume_id: &str,
        role: &str,
        quiet: bool,
        ask: Ask,
    ) -> Result<Pod, ProviderError> {
        let mut request = self.request.clone();
        request.region = region.to_string();
        request.volume = Some(volume_id.to_string()).filter(|v| !v.is_empty());
        let (role, none) = match (ask, &self.tiers) {
            (Ask::Preferred, Some((preferred, _))) => {
                request.gpu_types.clone_from(preferred);
                (
                    format!("{role}, preferred types"),
                    "no preferred GPU type free",
                )
            }
            (Ask::Others, Some((_, others))) => {
                request.gpu_types.clone_from(others);
                (format!("{role}, other types"), "no other GPU type free")
            }
            _ => (role.to_string(), "no GPU free"),
        };
        let role = role.as_str();
        if !self.tried {
            let gpus: Vec<&str> = gpu_type_list(self.cfg).into_iter().take(3).collect();
            self.say(
                Tone::Dim,
                format!(
                    "Creating pod in {region} ({}x from: {}...)",
                    self.cfg.get("GPU_COUNT").unwrap_or(""),
                    gpus.join(", ")
                ),
            );
        }
        self.tried = true;
        let result = self
            .create(region, volume_id, role, quiet, &mut request)
            .await;
        if let Err(error) = &result
            && error.capacity
            && !quiet
        {
            self.say(Tone::Warn, format!("  {region} ({role}): {none}"));
        }
        result
    }

    async fn create(
        &mut self,
        region: &str,
        volume_id: &str,
        role: &str,
        quiet: bool,
        request: &mut PodRequest,
    ) -> Result<Pod, ProviderError> {
        let error = match self.seam.create(request).await {
            Ok(pod) => return Ok(pod),
            Err(error) => error,
        };
        let env = self.env;
        let gpus = json!(request.gpu_types);
        regions::log_failure(
            env.paths, env.time, self.cfg, region, role, volume_id, &gpus, &error,
        );
        let asked = request.gpu_types.clone();
        let filtered = !request.cuda.is_empty();
        if !(error.capacity
            && filtered
            && match self.seam.regions() {
                Some(stock) => regions::any_cuda_is_safe(stock, self.cfg, region, &asked).await,
                // No stock to tell newer drivers by: the filter stays.
                None => false,
            })
        {
            return Err(error);
        }
        // The free GPUs here are on drivers newer than CUDA_VERSIONS lists
        // (13.1 and 13.2 cannot be requested over REST) and none on older
        // ones, so the exact-version filter is all that refuses the pod.
        if !quiet {
            self.say(
                Tone::Dim,
                format!(
                    "  {region} ({role}): its free GPUs run newer CUDA drivers; asking without \
                     the exact version"
                ),
            );
        }
        request.cuda.clear();
        self.seam.create(request).await.inspect_err(|error| {
            let role = format!("{role}, any cuda");
            regions::log_failure(
                env.paths, env.time, self.cfg, region, &role, volume_id, &gpus, error,
            );
        })
    }

    /// Asks a yes/no question, or takes `default` with nobody to ask.
    async fn ask(&mut self, prompt: String, default: bool) -> bool {
        let answer = if default { Answer::Yes } else { Answer::No };
        let question = Question {
            kind: None,
            prompt,
            choices: Choices::YesNo,
            default: answer,
            unattended: answer,
            reason: Vec::new(),
            options: Vec::new(),
        };
        let Some(asking) = (self.user.ask)(&question) else {
            let shown = if default { "yes" } else { "no" };
            self.say(
                Tone::Dim,
                format!("{} -> {shown} (no terminal to ask)", question.prompt),
            );
            return default;
        };
        alert(
            self.user.report,
            "viy up needs an answer",
            &question.prompt,
            false,
        );
        let asked = Instant::now();
        let answer = asking.await;
        self.waited += asked.elapsed();
        answer != Answer::No
    }

    /// The types GPU_PREFERRED leaves, asked for in DATA_CENTER_ID: the
    /// pod, or None when none is free there, nothing was left out, or
    /// there is no primary volume to ask on (`viy up --backup`).
    async fn others_in_primary(&mut self, volume_id: &str) -> Result<Option<Won>, Stop> {
        if self.tiers.is_none() || volume_id.is_empty() {
            return Ok(None);
        }
        let primary = self.primary.clone();
        match self
            .attempt(&primary, volume_id, "primary", false, Ask::Others)
            .await
        {
            Ok(pod) => Ok(Some(Won {
                pod,
                region: primary,
                volume_id: volume_id.to_string(),
                created: false,
            })),
            Err(error) if !error.capacity => Err(Stop::Refused(error)),
            Err(_) => Ok(None),
        }
    }

    /// One request on the backup volume: the pod, or None when its region
    /// has none of these types free.
    async fn in_backup(
        &mut self,
        backup: &Value,
        region: &str,
        ask: Ask,
    ) -> Result<Option<Won>, Stop> {
        match self.attempt(region, id(backup), "backup", false, ask).await {
            Ok(pod) => {
                let now = regions::now(self.env.time);
                self.keep(regions::touch(self.env.paths, backup, now));
                Ok(Some(Won {
                    pod,
                    region: region.to_string(),
                    volume_id: id(backup).to_string(),
                    created: false,
                }))
            }
            Err(error) if !error.capacity => Err(Stop::Refused(error)),
            Err(_) => Ok(None),
        }
    }

    /// regions.py's `failover`: called once DATA_CENTER_ID refused the
    /// pod for lack of capacity. With GPU_PREFERRED that request asked for
    /// its types only: the backup is asked for them next, then the backup
    /// is offered a move to a region RunPod shows one of them free in, and
    /// only then are DATA_CENTER_ID and the backup asked for the others
    /// (kb d147). Without GPU_PREFERRED, as rpctl: the backup, then a move
    /// to where any of our types is free.
    async fn failover(&mut self, primary_volume: &str) -> Result<Won, Stop> {
        let (paths, time) = (self.env.paths, self.env.time);
        let primary = self.primary.clone();
        let backup = regions::find_backup(self.rp()?, paths, self.cfg, regions::now(time)).await?;
        let backup_region = backup
            .as_ref()
            .map(|b| crate::api::shown(b.get("dataCenterId")));
        let first = self.first();
        let gpu = if first == Ask::Preferred {
            "preferred GPU type"
        } else {
            "GPU"
        };
        if let (Some(backup), Some(region)) = (&backup, &backup_region) {
            let question = if first == Ask::Preferred {
                format!(
                    "{primary} has no preferred GPU type free. Try the backup volume in {region} \
                     before the other types?"
                )
            } else {
                format!("{primary} has no GPU free. Try the backup volume in {region}?")
            };
            if !self.requested && !self.ask(question, true).await {
                if let Some(won) = self.others_in_primary(primary_volume).await? {
                    return Ok(won);
                }
                return Err(Stop::Declined(format!(
                    "Not trying the backup in {region}."
                )));
            }
            if let Some(won) = self.in_backup(backup, region, first).await? {
                return Ok(won);
            }
            let either = if self.requested { "" } else { " either" };
            self.say(
                Tone::Warn,
                format!("The backup region {region} has no {gpu} free{either}."),
            );
        } else if self.requested {
            self.say(Tone::Warn, "There is no backup volume yet.");
        } else {
            self.say(
                Tone::Warn,
                format!("{primary} has no {gpu} free, and there is no backup volume yet."),
            );
        }

        // Read before these samples join it.
        let past = regions::history(paths, regions::now(time), regions::HISTORY_DAYS);
        self.say(
            Tone::Warn,
            "Checking RunPod's stock of our GPU types in every region with network volumes...",
        );
        let sampled = regions::sample_stock(
            self.runpod()?,
            paths,
            time,
            self.cfg,
            regions::SAMPLES,
            regions::SAMPLE_GAP,
        )
        .await
        .map_err(|error| {
            let first = error.message.lines().next().unwrap_or_default().to_string();
            Stop::NoCapacity(format!("Could not read RunPod's stock: {first}"))
        });
        let old = backup.as_ref().zip(backup_region.as_deref());
        // How the move to a preferred type ended, when one was offered.
        let mut moved: Option<Stop> = None;
        if let Some((preferred, _)) = self.tiers.clone() {
            let offered = match &sampled {
                Ok(samples) => {
                    let samples = regions::showing(samples, &preferred);
                    self.offer_move(&samples, &past, old, primary_volume, Ask::Preferred)
                        .await
                }
                Err(_) => Ok(None),
            };
            match offered {
                Ok(Some(won)) => return Ok(won),
                Ok(None) if sampled.is_ok() => self.say(
                    Tone::Warn,
                    "No other region with network volumes shows a preferred GPU type free right \
                     now.",
                ),
                Ok(None) => {}
                Err(stop @ (Stop::Declined(_) | Stop::NoCapacity(_))) => moved = Some(stop),
                Err(stop) => return Err(stop),
            }
            // The other types where the weights already are.
            if let Some(won) = self.others_in_primary(primary_volume).await? {
                return Ok(won);
            }
            if let Some((backup, region)) = old
                && let Some(won) = self.in_backup(backup, region, Ask::Others).await?
            {
                return Ok(won);
            }
        }
        if let Some(stop) = moved {
            return Err(stop);
        }
        let samples = sampled?;
        if let Some(won) = self
            .offer_move(&samples, &past, old, primary_volume, Ask::Every)
            .await?
        {
            return Ok(won);
        }
        let kept = backup_region
            .as_ref()
            .map(|r| format!(" The backup in {r} was kept."))
            .unwrap_or_default();
        Err(Stop::NoCapacity(format!(
            "No other region with network volumes shows one of our GPU types free right now.\
             {kept}"
        )))
    }

    /// Offers to move the backup (or make one) where `samples` show a GPU
    /// free, outside DATA_CENTER_ID and the old backup's region, and on
    /// yes [`grab`](Self::grab)s a pod there asking for `ask`'s types. None
    /// when no such region showed one.
    async fn offer_move(
        &mut self,
        samples: &[regions::Snapshot],
        past: &HashMap<String, f64>,
        old: Option<(&Value, &str)>,
        primary_volume: &str,
        ask: Ask,
    ) -> Result<Option<Won>, Stop> {
        let primary = self.primary.clone();
        let mut exclude = HashSet::from([primary.as_str()]);
        exclude.extend(old.map(|(_, region)| region));
        let types = regions::stock_types(self.cfg);
        let candidates = regions::rank(samples, &exclude, past, &types);
        if candidates.is_empty() {
            return Ok(None);
        }
        let shown: Vec<String> = candidates
            .iter()
            .map(|r| {
                format!(
                    "{r} {}/{} ({})",
                    regions::free_in(samples, r),
                    samples.len(),
                    regions::cheapest_seen(samples, r, &types).unwrap_or("None")
                )
            })
            .collect();
        let what = if ask == Ask::Preferred {
            "Preferred GPU type"
        } else {
            "GPU"
        };
        self.say(
            Tone::Warn,
            format!(
                "{what} free now (of {} samples): {}",
                samples.len(),
                shown.join(", ")
            ),
        );
        let minutes = regions::grab_minutes(self.cfg);
        let mut place = if let [only] = candidates.as_slice() {
            only.clone()
        } else {
            format!("the first of {} that takes the pod", candidates.join(", "))
        };
        if ask == Ask::Preferred {
            place.push_str(" for a preferred GPU type, before trying the other types");
        }
        // The regions `grab` retries besides the candidates: the primary
        // only with its volume, which `viy up --backup` leaves out.
        let primary_retried = (!primary_volume.is_empty()).then_some(primary.as_str());
        let also: Vec<&str> = [primary_retried, old.map(|(_, region)| region)]
            .into_iter()
            .flatten()
            .collect();
        let retry = if minutes != 0.0 {
            let elsewhere = if also.is_empty() {
                String::new()
            } else {
                format!(" and in {}", also.join(" and "))
            };
            format!(
                ", retrying refusals there{elsewhere} for up to {} min; new volumes left unused \
                 are deleted",
                short(minutes)
            )
        } else {
            String::new()
        };
        let question = match old {
            Some((backup, region)) => format!(
                "Switch the backup to {place}? It makes a new volume in each region it tries and \
                 boots cold (~5 min: weights download, first compile){retry}. The old backup {} \
                 in {region} is deleted once viy is up on the new one.",
                id(backup)
            ),
            None => format!(
                "Make a backup volume in {place}? It makes a new volume in each region it tries \
                 and boots cold (~5 min: weights download, first compile){retry}. ~$7/month, \
                 deleted after {:.0} days unused.",
                regions::idle_days(self.cfg)
            ),
        };
        if !self.ask(question, false).await {
            let what = if old.is_some() { "switched." } else { "made." };
            return Err(Stop::Declined(format!("The backup was not {what}")));
        }
        // New volumes exist from here on: a Ctrl-C deletes them first.
        self.user.abort_at.store(u32::MAX, Ordering::SeqCst);
        let won = self
            .grab(&candidates, old, primary_volume, minutes, ask)
            .await;
        let next = self.user.interrupt.count() + 1;
        self.user.abort_at.store(next, Ordering::SeqCst);
        won.map(Some)
    }

    /// regions.py's `_grab`: the first region that accepts the pod wins,
    /// each candidate on a new volume of its own, and from the second
    /// round on also the primary and the old backup, which need no new
    /// volume and boot warm. Stock flickers, so refusals are retried every
    /// [`RETRY_GAP_S`] until `minutes` run out. Every request asks for
    /// `ask`'s types.
    async fn grab(
        &mut self,
        offered: &[String],
        backup: Option<(&Value, &str)>,
        primary_volume: &str,
        minutes: f64,
        ask: Ask,
    ) -> Result<Won, Stop> {
        let paths = self.env.paths;
        // A candidate whose region RunPod's pod API does not take is
        // dropped from here (rpctl 1d7eead).
        let mut candidates: Vec<String> = offered.to_vec();
        let mut warm: Vec<(String, String, &str)> = Vec::new();
        if !primary_volume.is_empty() {
            warm.push((self.primary.clone(), primary_volume.to_string(), "primary"));
        }
        if let Some((backup, region)) = backup {
            warm.push((region.to_string(), id(backup).to_string(), "backup"));
        }
        // Region to the new volume made there, empty until a pod uses it.
        let mut made: IndexMap<String, Value> = IndexMap::new();
        let deadline = Duration::try_from_secs_f64(minutes * 60.0)
            .ok()
            .and_then(|d| Instant::now().checked_add(d));
        let mut stopped = pin!(self.user.interrupt.next());
        let mut rounds = 0u32;
        let mut won: Option<Won> = None;
        let mut failed: Option<Stop> = None;
        let mut interrupted = false;
        'rounds: loop {
            let fresh = candidates
                .iter()
                .map(|r| (r.clone(), String::new(), "new backup"));
            let targets: Vec<(String, String, &str)> = if rounds > 0 {
                warm.iter().cloned().chain(fresh).collect()
            } else {
                fresh.collect()
            };
            for (region, mut volume_id, role) in targets {
                if volume_id.is_empty() {
                    if !made.contains_key(&region) {
                        let made_now = tokio::select! {
                            biased;
                            () = &mut stopped => { interrupted = true; break 'rounds; }
                            made_now = self.new_volume(&region) => made_now,
                        };
                        match made_now {
                            Ok(volume) => {
                                made.insert(region.clone(), volume);
                            }
                            Err(Stop::Refused(error)) if regions::is_tier_unavailable(&error) => {
                                candidates.retain(|r| *r != region);
                                self.say(
                                    Tone::Warn,
                                    format!(
                                        "  {region}: no {} storage for a volume there; dropped",
                                        regions::backup_tier(self.cfg).unwrap_or("such")
                                    ),
                                );
                                continue;
                            }
                            Err(stop) => {
                                failed = Some(stop);
                                break 'rounds;
                            }
                        }
                    }
                    volume_id = made.get(&region).map(id).unwrap_or_default().to_string();
                }
                let attempt = tokio::select! {
                    biased;
                    () = &mut stopped => { interrupted = true; break 'rounds; }
                    attempt = self.attempt(&region, &volume_id, role, rounds > 0, ask) => attempt,
                };
                match attempt {
                    Ok(pod) => {
                        let created = made.get(&region).is_some_and(|v| id(v) == volume_id);
                        won = Some(Won {
                            pod,
                            region,
                            volume_id,
                            created,
                        });
                        break 'rounds;
                    }
                    Err(error) if role == "new backup" && regions::is_region_rejected(&error) => {
                        candidates.retain(|r| *r != region);
                        self.say(
                            Tone::Warn,
                            format!(
                                "  {region}: RunPod's pod API does not take this region; \
                                 dropped, its new volume deleted"
                            ),
                        );
                    }
                    Err(error) if !error.capacity => {
                        failed = Some(Stop::Refused(error));
                        break 'rounds;
                    }
                    Err(_) => {}
                }
            }
            rounds += 1;
            let left = deadline.map_or(f64::INFINITY, |d| {
                d.saturating_duration_since(Instant::now()).as_secs_f64()
            });
            if left <= RETRY_GAP_S as f64 {
                break;
            }
            if rounds == 1 {
                self.say(
                    Tone::Warn,
                    format!(
                        "No region took the pod; retrying every {RETRY_GAP_S}s for {} min \
                         (Ctrl-C stops and deletes the new volumes).",
                        short(minutes)
                    ),
                );
            } else if rounds.is_multiple_of(6) {
                self.say(
                    Tone::Warn,
                    format!(
                        "  still no GPU free; {:.0} min of retries left",
                        left / 60.0
                    ),
                );
            }
            let gap = Duration::from_secs(RETRY_GAP_S);
            tokio::select! {
                biased;
                () = &mut stopped => { interrupted = true; break; }
                () = tokio::time::sleep(gap) => self.waited += gap,
            }
        }

        // Whatever ended the rounds, the new volumes no pod took go now. A
        // further Ctrl-C skips the one being deleted, which stays recorded
        // as unused for `viy reap`, and goes on with the rest.
        let winner = won.as_ref().map(|w| w.volume_id.as_str());
        let rp = self.rp()?;
        for volume in made.values().filter(|v| Some(id(v)) != winner) {
            let deleted = tokio::select! {
                biased;
                () = self.user.interrupt.next() => continue,
                deleted = regions::delete_empty(rp, paths, volume, self.user.report) => deleted,
            };
            self.keep(deleted);
        }
        if interrupted {
            return Err(Stop::Command(CommandError::Interrupted));
        }
        if let Some(stop) = failed {
            return Err(stop);
        }
        let Some(won) = won else {
            let mut tried = candidates.to_vec();
            if minutes != 0.0 {
                tried.extend(warm.iter().map(|(r, _, _)| r.clone()));
            }
            let during = if minutes != 0.0 {
                format!(" in {} min of retries", short(minutes))
            } else {
                String::new()
            };
            let kept = if backup.is_some() {
                ", the old backup kept."
            } else {
                "."
            };
            return Err(Stop::NoCapacity(format!(
                "No region took the pod{during} ({}). The volumes made for it were deleted{kept}",
                tried.join(", ")
            )));
        };
        // A pod exists now: backups.json failing must not lose it.
        let now = regions::now(self.env.time);
        if won.created {
            if let Some(volume) = made.get(&won.region) {
                self.keep(regions::touch(paths, volume, now));
            }
            if let Some((backup, _)) = backup {
                // The old backup goes only once `viy up` serves on the new
                // one: a boot that fails now leaves it in place.
                self.keep(regions::mark_replaces(paths, &won.volume_id, id(backup)));
            }
        } else if let Some((backup, _)) = backup.filter(|(b, _)| id(b) == won.volume_id) {
            self.keep(regions::touch(paths, backup, now));
        }
        Ok(won)
    }

    /// A new volume for the backup in `region`, recorded as unused until
    /// a pod takes it.
    async fn new_volume(&mut self, region: &str) -> Result<Value, Stop> {
        let size = self.cfg.get("NETWORK_VOLUME_SIZE_GB").unwrap_or("");
        let size_gb: u64 = size.trim().parse().map_err(|_| {
            Stop::Command(CommandError::Exit(format!(
                "NETWORK_VOLUME_SIZE_GB must be a whole number, not {size:?}"
            )))
        })?;
        let volume = self
            .rp()?
            .create_volume(
                &regions::backup_name(self.cfg),
                size_gb,
                region,
                regions::backup_tier(self.cfg),
            )
            .await
            .map_err(|e| Stop::Refused(error_of(&e)))?
            .filter(|v| !id(v).is_empty())
            .ok_or_else(|| {
                Stop::Refused(error_of(&RunPodError::new(
                    "RunPod created no volume",
                    None,
                )))
            })?;
        regions::record_unused(self.env.paths, &volume, regions::now(self.env.time))?;
        Ok(volume)
    }
}

#[cfg(test)]
#[path = "failover_tests.rs"]
mod tests;
