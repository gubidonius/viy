//! This machine's backup for the pod's own idle watchdog: rpctl's
//! `idle.py`.
//!
//! The pod stops itself after IDLE_TTL_MIN without tokens flowing, which
//! works while this machine sleeps. This covers the pod's watchdog
//! failing: the chat records when it last streamed ([`touch`]), and
//! [`reap`], run every 5 minutes by a systemd user timer, runs `viy stop`
//! once nothing has streamed for IDLE_TTL_MIN + REAP_GRACE_MIN, and leaves
//! a stopped pod alone: it costs nothing but its volume, and `viy up`
//! resumes or replaces it (kb d177, d176). It sees only this machine's
//! activity, which is why it waits longer than the pod.
//!
//! Until cutover rp's `rpctl-reap.timer` reaps the shared pod, and reads
//! the same `last_activity` and `reap.json` (kb d006); [`install`] refuses
//! while that timer is installed. `viy migrate --finish` removes it
//! ([`remove_rpctl_timer`]) and installs viy's (kb d135).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::PoisonError;

use chrono::DateTime;
use chrono::Utc;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_config::Config;
use viy_config::Paths;
use viy_config::atomic::write_atomically;
use viy_exec::ExecBackend;
use viy_time::TimeProvider;

use crate::backend::Backend;
use crate::commands;
use crate::commands::CommandError;
use crate::commands::Env;
use crate::commands::alert;
use crate::lock::PodLock;
use crate::notice::Report;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::notice::say;
use crate::regions;
use crate::regions::first_line;
use crate::tracked::ResolveError;
use crate::tunnel;

/// [`touch`] writes at most this often.
const TOUCH_EVERY_S: i64 = 30;
/// [`reap`] warns this long before the pod's own watchdog fires.
const WARN_BEFORE_S: f64 = 10.0 * 60.0;

/// The pod's idle limits from the config, in seconds: how long without
/// tokens, the reaper's grace on top, and the hard cap. Zero means off; a
/// value that is not a number counts as zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub ttl: f64,
    pub grace: f64,
    pub max: f64,
}

pub fn limits(cfg: &Config) -> Limits {
    let seconds = |key: &str, unit: f64| {
        cfg.get(key)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(0.0)
            * unit
    };
    Limits {
        ttl: seconds("IDLE_TTL_MIN", 60.0),
        grace: seconds("REAP_GRACE_MIN", 60.0),
        max: seconds("MAX_POD_HOURS", 3600.0),
    }
}

/// Records that the model is in use, at most every 30 s per state
/// directory unless `force`: cheap enough per streamed chunk. Best effort.
pub fn touch(paths: &Paths, time: &dyn TimeProvider, force: bool) {
    static LAST: Mutex<BTreeMap<PathBuf, DateTime<Utc>>> = Mutex::new(BTreeMap::new());
    let now = time.now();
    let path = paths.activity_file();
    let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
    if !force
        && last
            .get(&path)
            .is_some_and(|at| (now - *at).num_seconds() < TOUCH_EVERY_S)
    {
        return;
    }
    last.insert(path, now);
    let _ = fs::create_dir_all(paths.pod_state_dir()).and_then(|()| {
        fs::write(
            paths.activity_file(),
            format!("{:.0}\n", now.timestamp_micros() as f64 / 1e6),
        )
    });
}

/// When this machine last streamed, if it ever did.
pub fn last_activity(paths: &Paths) -> Option<DateTime<Utc>> {
    let text = fs::read_to_string(paths.activity_file()).ok()?;
    let seconds: f64 = text.trim().parse().ok()?;
    DateTime::from_timestamp_micros((seconds * 1e6) as i64)
}

/// [`last_activity`] as rp's files hold times, seconds since the epoch.
fn activity_seconds(paths: &Paths) -> Option<f64> {
    last_activity(paths).map(|at| at.timestamp_micros() as f64 / 1e6)
}

// -------------------------------------------------------------------- reap

/// `reap.json`, for the pod reap is watching: when it first saw it, and
/// the activity time it last warned about (new activity re-arms the
/// warning). Shared with rp's reap.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub pod_id: String,
    pub first_seen: f64,
    pub warned_for: Option<f64>,
}

impl Seen {
    /// The record for `pod_id`, or a first sighting at `now` when the file
    /// is about another pod, or missing, or not one.
    pub fn load(paths: &Paths, pod_id: &str, now: f64) -> Self {
        let saved = fs::read_to_string(paths.reap_state_file())
            .ok()
            .and_then(|text| serde_json::from_str::<Map<String, Value>>(&text).ok())
            .filter(|seen| seen.get("pod_id").and_then(Value::as_str) == Some(pod_id));
        let number = |key| saved.as_ref()?.get(key)?.as_f64();
        Self {
            pod_id: pod_id.to_string(),
            first_seen: number("first_seen").unwrap_or(now),
            warned_for: number("warned_for"),
        }
    }

    pub fn save(&self, paths: &Paths) -> io::Result<()> {
        let mut seen = json!({"pod_id": self.pod_id, "first_seen": self.first_seen});
        if let (Some(at), Some(map)) = (self.warned_for, seen.as_object_mut()) {
            map.insert("warned_for".into(), json!(at));
        }
        fs::create_dir_all(paths.pod_state_dir())?;
        write_atomically(&paths.reap_state_file(), &seen.to_string())
    }
}

/// What [`reap`] does about the pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Tell the user the pod is about to shut itself down.
    Warn,
    /// Stop it.
    Stop,
}

/// When the pod was last in use as far as this machine knows: its last
/// stream, but not before reap first saw the pod. A warning is for one
/// such time ([`Seen::warned_for`]).
pub fn last_active(activity: Option<f64>, seen: &Seen) -> f64 {
    activity.unwrap_or(0.0).max(seen.first_seen)
}

/// What `viy reap` should do, and why. `desired` is the pod's
/// `desiredStatus`, `activity` this machine's last stream.
pub fn decide(
    now: f64,
    desired: Option<&str>,
    activity: Option<f64>,
    seen: &Seen,
    limits: Limits,
) -> (Option<Verdict>, String) {
    let Limits {
        ttl,
        grace,
        max: cap,
    } = limits;
    if desired == Some("EXITED") {
        // Stopped already: nothing bills but the volume (kb d177).
        return (None, "the pod is stopped".into());
    }
    if ttl == 0.0 {
        return (None, "IDLE_TTL_MIN=0".into());
    }
    let last = last_active(activity, seen);
    let idle = now - last;
    if idle >= ttl + grace {
        return (
            Some(Verdict::Stop),
            format!(
                "nothing streamed from this machine for {:.0} min",
                idle / 60.0
            ),
        );
    }
    if cap != 0.0 && now - seen.first_seen >= cap + grace {
        return (
            Some(Verdict::Stop),
            format!("pod running for {:.1} h", (now - seen.first_seen) / 3600.0),
        );
    }
    if idle >= ttl - WARN_BEFORE_S && seen.warned_for != Some(last) {
        let left = (ttl - idle).max(0.0);
        return (
            Some(Verdict::Warn),
            format!(
                "idle {:.0} min; the pod shuts itself down in ~{:.0} min",
                idle / 60.0,
                left / 60.0
            ),
        );
    }
    (None, format!("idle {:.0} min", idle / 60.0))
}

/// `viy reap`: stops our pod once this machine has streamed nothing for
/// IDLE_TTL_MIN + REAP_GRACE_MIN, or it has run past MAX_POD_HOURS; warns
/// ahead; leaves a stopped pod alone, forgetting what it saw of it so a
/// resumed pod is counted from then; deletes idle backup and primary
/// volumes, a volume's stopped pods of ours first (kb d178). Never while an up, stop or down of either tool holds the pod
/// lock: the next run looks again. `quiet` (the timer) says nothing but
/// the desktop alerts and what `viy stop` says.
pub async fn reap(
    env: &Env<'_>,
    cfg: &Config,
    quiet: bool,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let print = |text: String| {
        if !quiet {
            say(report, Tone::Plain, text);
        }
    };
    // BACKEND as up reads it.
    if Backend::resolve(cfg).map_err(CommandError::Exit)? != Backend::Managed {
        return Ok(());
    }
    let Ok(_lease) = crate::rental::lease(env.paths, false) else {
        return Ok(());
    };
    crate::rental::current(env.paths)?;
    if !PodLock::is_free(env.paths)? {
        print("rp or viy up/stop/down is running; not checking now".into());
        return Ok(());
    }
    let seam = crate::rental::selected(env, cfg)?;
    // Volumes and tracking stay RunPod's (kb d182).

    let now = regions::now(env.time);
    let mut tell = |message: String| {
        alert(report, "viy reap", message.clone(), false);
        print(message);
    };
    // Volumes unused for BACKUP_IDLE_DAYS or PRIMARY_IDLE_DAYS (expiry
    // takes the pod lock itself when one is due).
    if env.paths.rental == viy_config::Rental::Runpod {
        let rp = commands::connect(env, cfg)?;
        match regions::expire(&rp, env.paths, cfg, now, &mut tell).await {
            Err(CommandError::RunPod(error)) => {
                print(format!("volume check failed: {}", first_line(&error)));
            }
            result => {
                result?;
            }
        }
    }
    if env.paths.rental != viy_config::Rental::Runpod
        && cfg.get("RUNPOD_API_KEY").is_some_and(|key| !key.is_empty())
    {
        let mut legacy = env.paths.clone();
        legacy.rental = viy_config::Rental::Runpod;
        let rp = commands::connect(
            &Env {
                paths: &legacy,
                ..*env
            },
            cfg,
        )?;
        if let Err(e) = regions::expire(&rp, &legacy, cfg, now, &mut tell).await {
            tell(format!("RunPod volume check failed: {e}"));
        }
    }
    let scopes = crate::rental::nebius_scopes(env.paths)?;
    if !scopes.is_empty() && viy_nebius::Settings::load(env.paths)?.workspace_idle_days != 0 {
        let _lock = PodLock::exclusive(env.paths, "volume-expiry")?;
        for scoped in scopes {
            let provider = viy_nebius::NebiusProvider::new(&scoped, env.http.clone())?;
            match provider.expire_workspace(now as u64).await {
                Ok(Some(disk)) => tell(format!("Expired unused Nebius workspace {disk}")),
                Ok(None) => {}
                Err(e) => tell(format!("Nebius workspace expiry: {e}")),
            }
        }
    }
    let found = match commands::resolve(&*seam, env, cfg, report).await {
        Ok(found) => found,
        Err(ResolveError::Provider(error)) => {
            let first = error.message.lines().next().unwrap_or_default();
            print(format!(
                "could not reach {}: {first}",
                env.paths.rental.display_name()
            ));
            return Ok(());
        }
        Err(ResolveError::State(error)) => return Err(error.into()),
    };
    let Some(pod) = found else {
        remove(&env.paths.reap_state_file())?;
        // The pod shut itself down; the forward is dead.
        let mut said = |notice| report(Report::Notice(notice));
        tunnel::stop(&env.ssh(cfg), true, &mut said).await?;
        print("no pod running".into());
        return Ok(());
    };
    // The pod through the seam: its state in RunPod's word, which
    // `decide` reads as rp's `idle.decide` does.
    let desired = if pod.state == Some(viy_provider::PodState::Stopped) {
        Some("EXITED")
    } else {
        pod.state_name.as_deref()
    };
    if desired != Some("EXITED") && env.paths.rental == viy_config::Rental::Runpod {
        // A stopped pod is not a use of its volume (kb d178).
        regions::note_pod(env.paths, &pod, now)?;
        let volume = pod.volume.as_deref().unwrap_or_default();
        let region = pod
            .region
            .as_deref()
            .unwrap_or_else(|| cfg.get("DATA_CENTER_ID").unwrap_or(""));
        regions::note_primary(env.paths, cfg, volume, region, now)?;
    }
    if desired == Some("EXITED") {
        // A resumed pod is a new run: its cap counts from when reap sees
        // it running again. The forward points at nothing now.
        forget(env.paths)?;
        let mut said = |notice| report(Report::Notice(notice));
        tunnel::stop(&env.ssh(cfg), true, &mut said).await?;
        print("the pod is stopped".into());
        return Ok(());
    }
    let id = pod.id.as_str();
    let mut seen = Seen::load(env.paths, id, now);
    let activity = activity_seconds(env.paths);
    let (verdict, reason) = decide(now, desired, activity, &seen, limits(cfg));
    if verdict == Some(Verdict::Warn) {
        seen.warned_for = Some(last_active(activity, &seen));
        alert(report, "viy: the pod is idle", reason.clone(), false);
    }
    seen.save(env.paths)?;
    print(reason.clone());
    if verdict == Some(Verdict::Stop) {
        alert(report, "viy reap: stopping the pod", reason, true);
        // A stop RunPod never got keeps reap.json, so the next run tries
        // again from the same count.
        commands::stop_confirmed(env, cfg, report).await?;
        remove(&env.paths.reap_state_file())?;
    }
    Ok(())
}

/// Forgets what reap saw of the pod, so its next run counts the pod from
/// then: a resumed pod's idle time and cap start anew, as its watchdog's do.
pub fn forget(paths: &Paths) -> io::Result<()> {
    remove(&paths.reap_state_file())
}

/// Removes `path`; one that is not there is no error.
fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

// ----------------------------------------------------------------- install

/// rp's timer, which reaps the shared pod until cutover.
const RPCTL_TIMER: &str = "rpctl-reap.timer";

fn service(viy: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=viy reap: stop an idle RunPod pod (backup for the pod's own watchdog)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} reap --quiet\n",
        viy.display()
    )
}

const TIMER: &str = "[Unit]
Description=Run viy reap every 5 minutes

[Timer]
OnCalendar=*:0/5
# Fires on wake when a run was missed during suspend.
Persistent=true

[Install]
WantedBy=timers.target
";

/// `viy reap --install`: a systemd user timer that runs `viy reap --quiet`
/// with `viy` (the one on PATH) every 5 minutes. Refused while rp's timer
/// is installed: it reaps the shared pod already, and two reapers would
/// warn twice; viy's replaces it at cutover (ROADMAP phase 8).
pub async fn install(
    paths: &Paths,
    exec: &dyn ExecBackend,
    viy: Option<&Path>,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let units = paths.systemd_user_dir();
    if rpctl_timer_installed(paths) {
        return Err(CommandError::Exit(format!(
            "{RPCTL_TIMER} is installed and runs rp reap, which reaps viy's pod too; \
             viy-reap.timer replaces it at cutover (ROADMAP phase 8)"
        )));
    }
    let Some(viy) = viy else {
        return Err(CommandError::Exit(
            "viy is not on PATH; install it first".into(),
        ));
    };
    fs::create_dir_all(&units)?;
    fs::write(units.join("viy-reap.service"), service(viy))?;
    fs::write(units.join("viy-reap.timer"), TIMER)?;
    systemctl(exec, &["daemon-reload"]).await?;
    systemctl(exec, &["enable", "--now", "viy-reap.timer"]).await?;
    say(
        report,
        Tone::Plain,
        format!(
            "installed {}/viy-reap.timer: viy reap runs every 5 minutes",
            units.display()
        ),
    );
    say(
        report,
        Tone::Plain,
        "check it with:  systemctl --user list-timers viy-reap.timer",
    );
    Ok(())
}

/// The two folders a user's systemd units can be in: `~/.config/systemd/user`
/// and the one under this shell's `XDG_CONFIG_HOME`, when that differs.
fn unit_dirs(paths: &Paths) -> Vec<PathBuf> {
    let mut dirs = vec![paths.systemd_user_dir()];
    let xdg = paths.config_home.join("systemd").join("user");
    if !dirs.contains(&xdg) {
        dirs.push(xdg);
    }
    dirs
}

/// Whether rp's reap timer is installed, in either folder.
pub fn rpctl_timer_installed(paths: &Paths) -> bool {
    unit_dirs(paths)
        .iter()
        .any(|dir| dir.join(RPCTL_TIMER).exists())
}

/// Whether viy's reap timer is installed.
pub fn timer_installed(paths: &Paths) -> bool {
    paths.systemd_user_dir().join("viy-reap.timer").exists()
}

/// `viy migrate --finish` (kb d135): stops and disables rp's reap timer
/// and removes its two units, so that, once the pod state has moved to
/// viy's state folder, rp reap no longer looks at a pod it has no state
/// for. Nothing to do when it is not installed.
pub async fn remove_rpctl_timer(
    paths: &Paths,
    exec: &dyn ExecBackend,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    if !rpctl_timer_installed(paths) {
        return Ok(());
    }
    systemctl(exec, &["disable", "--now", RPCTL_TIMER]).await?;
    for dir in unit_dirs(paths) {
        for unit in [RPCTL_TIMER, "rpctl-reap.service"] {
            remove(&dir.join(unit))?;
        }
    }
    systemctl(exec, &["daemon-reload"]).await?;
    say(
        report,
        Tone::Plain,
        format!("stopped and removed {RPCTL_TIMER}: rp reap no longer runs"),
    );
    Ok(())
}

/// `systemctl --user <args>`, an error unless it exits 0.
async fn systemctl(exec: &dyn ExecBackend, args: &[&str]) -> Result<(), CommandError> {
    let command = viy_exec::Command::new("systemctl")
        .arg("--user")
        .args(args.iter().copied());
    let shown = command.argv().join(" ");
    let out = exec.run(command).await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(CommandError::Exit(format!(
            "{shown} exited {}",
            viy_exec::exit_text(out.status)
        )))
    }
}

#[cfg(test)]
#[path = "idle_tests.rs"]
mod tests;
