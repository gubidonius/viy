//! Provider selection is global. An operation holds a snapshot of the previous choice.
use crate::{
    commands::{self, CommandError, Env},
    lock::PodLock,
    notice::{Asker, Reporter, Tone, say},
    state, tunnel,
};
use std::fs;
use std::sync::Arc;
use viy_config::{Config, Paths, Rental};
use viy_nebius::Account;
use viy_protocol::{Answer, Question};
use viy_provider::{PodState, Provider};

/// Keep the shared inference port attached to its provider for an entire turn.
pub fn lease(paths: &Paths, switching: bool) -> std::io::Result<fs::File> {
    fs::create_dir_all(paths.state_dir())?;
    let file = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.state_dir().join("gpu-provider.lock"))?;
    let result = if switching {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => std::io::Error::other("GPU provider is in use by a turn, scheduled shutdown or provider switch. Retry when it finishes."),
        std::fs::TryLockError::Error(error) => error,
    })?;
    Ok(file)
}

pub fn selected(env: &Env<'_>, cfg: &Config) -> Result<Arc<dyn Provider>, CommandError> {
    match &env.paths.rental {
        Rental::Runpod => Ok(Arc::new(commands::provider(env, cfg)?)),
        Rental::Nebius { .. } => Ok(Arc::new(env.nebius()?)),
    }
}

pub fn current(paths: &Paths) -> Result<(), CommandError> {
    if Rental::load(paths)? != paths.rental {
        return Err(CommandError::Exit(
            "GPU provider changed while this command was starting. Repeat the command.".into(),
        ));
    }
    Ok(())
}

pub fn settings(paths: &Paths, cfg: &Config) -> Config {
    let mut effective = cfg.clone();
    if let Rental::Nebius { region, .. } = &paths.rental {
        effective.set("DATA_CENTER_ID", region);
        effective.set("NETWORK_VOLUME_ID", "");
        effective.set("GPU_TYPE_IDS", "RTX PRO 6000");
        effective.set("GPU_PREFERRED", "");
        effective.set("CUDA_VERSIONS", "");
        effective.set("TUNNEL", "1");
        effective.set("RUNPOD_WATCHDOG_API_KEY", "");
    }
    effective
}

pub async fn stock(env: &Env<'_>, report: Reporter<'_>) -> Result<(), CommandError> {
    let account =
        Account::new(env.paths, env.http.clone())?.with_terminal_login(env.terminal_login);
    say(report, Tone::Plain, account.stock(env.paths).await?);
    Ok(())
}

fn observation_path(paths: &Paths) -> std::path::PathBuf {
    if paths.rental == Rental::Runpod {
        paths.state_dir().join("providers/runpod/observation.json")
    } else {
        paths.pod_state_dir().join("observation.json")
    }
}

pub(crate) fn observe(
    paths: &Paths,
    status: &str,
    region: Option<&str>,
    workspace: Option<&str>,
) -> std::io::Result<()> {
    viy_config::atomic::write_atomically(&observation_path(paths), &serde_json::json!({"state":status,"region":region,"workspace":workspace,"at":chrono::Utc::now().to_rfc3339()}).to_string())
}

pub(crate) fn nebius_scopes(paths: &Paths) -> std::io::Result<Vec<Paths>> {
    let root = paths.state_dir().join("providers/nebius");
    let mut scopes = Vec::new();
    if root.exists() {
        for project in fs::read_dir(root)? {
            let project = project?;
            if !project.file_type()?.is_dir() {
                continue;
            }
            for region in fs::read_dir(project.path())? {
                let region = region?;
                if !region.file_type()?.is_dir() {
                    continue;
                }
                let mut scoped = paths.clone();
                let label = region.file_name().to_string_lossy().into_owned();
                scoped.rental = Rental::nebius(&project.file_name().to_string_lossy(), &label)?;
                scopes.push(scoped);
            }
        }
    }
    Ok(scopes)
}

pub fn list(paths: &Paths, report: Reporter<'_>) {
    say(report, Tone::Plain, "GPU providers (* = selected):");
    for name in Rental::NAMES {
        let mark = if paths.rental.name() == name {
            '*'
        } else {
            ' '
        };
        say(report, Tone::Plain, format!("  {mark} {name}"));
    }
}

pub fn show(
    paths: &Paths,
    provider: Option<&str>,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    say(
        report,
        Tone::Plain,
        match &paths.rental {
            Rental::Runpod => "Selected GPU provider: runpod".into(),
            Rental::Nebius { project, region } => {
                format!("Selected GPU provider: nebius ({region}, {project})")
            }
        },
    );
    let mut runpod = paths.clone();
    runpod.rental = Rental::Runpod;
    let describe = |paths: &Paths, region: &str| -> Result<(), CommandError> {
        let cached = state::load(paths)?;
        let record: serde_json::Value = match fs::read(observation_path(paths)) {
            Ok(data) => serde_json::from_slice(&data).map_err(std::io::Error::other)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
            Err(e) => return Err(e.into()),
        };
        let observed = record["at"].as_str().unwrap_or("time unknown");
        let status = record["state"].as_str().unwrap_or("unknown");
        let region = record["region"].as_str().unwrap_or(region);
        say(
            report,
            Tone::Plain,
            format!(
                "{} {region} · cached {status} ({observed}) · machine {} · workspace {}",
                paths.rental.name(),
                cached.pod_id().unwrap_or("none recorded"),
                cached
                    .volume_id()
                    .or_else(|| record["workspace"].as_str())
                    .unwrap_or("none recorded")
            ),
        );
        Ok(())
    };
    if provider != Some("nebius") {
        describe(&runpod, "")?;
    }
    if provider == Some("runpod") {
        return Ok(());
    }
    let scopes = nebius_scopes(paths)?;
    for scope in &scopes {
        if let Rental::Nebius { region, .. } = &scope.rental {
            describe(scope, region)?;
        }
    }
    if scopes.is_empty() {
        say(
            report,
            Tone::Plain,
            "nebius · no cached machine or workspace",
        );
    }
    Ok(())
}

pub(crate) async fn retry_capacity(
    env: &Env<'_>,
    report: Reporter<'_>,
    ask: Asker<'_>,
) -> Result<bool, CommandError> {
    stock(env, report).await?;
    say(
        report,
        Tone::Plain,
        "For another region, use viy provider use nebius --region REGION, then viy up. Regions without a workspace need a new disk and model download.",
    );
    let q = Question::yes_no(None, "Wait 30 seconds and retry the billed start here?");
    if let Some(answer) = ask(&q)
        && answer.await == Answer::Yes
    {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        return Ok(true);
    }
    Ok(false)
}

pub async fn switch(
    env: &Env<'_>,
    cfg: &Config,
    name: &str,
    region: Option<&str>,
    report: Reporter<'_>,
    ask: Asker<'_>,
) -> Result<(), CommandError> {
    let _lease = lease(env.paths, true)?;
    let _lock = PodLock::exclusive(env.paths, "provider")?;
    current(env.paths)?;
    let target = match (name, region) {
        ("runpod", None) => Rental::Runpod,
        ("nebius", region) => {
            let a =
                Account::new(env.paths, env.http.clone())?.with_terminal_login(env.terminal_login);
            a.select(region.unwrap_or(&a.settings.region)).await?
        }
        _ => {
            return Err(CommandError::Exit(
                "Use runpod or nebius [--region REGION].".into(),
            ));
        }
    };
    if env.paths.rental == target {
        say(
            report,
            Tone::Plain,
            format!("{} is already selected.", target.name()),
        );
        return Ok(());
    }
    if crate::shutdown::Scheduled::peek(env.paths).is_some() {
        return Err(CommandError::Exit(
            "Cancel the scheduled shutdown before switching GPU providers.".into(),
        ));
    }
    let effective = settings(env.paths, cfg);
    let cached = state::load(env.paths)?;
    let unconfigured = env.paths.rental == Rental::Runpod
        && cached.pod_id().is_none()
        && effective.get("RUNPOD_API_KEY").is_none_or(str::is_empty);
    let seam = if unconfigured {
        None
    } else {
        Some(selected(env, &effective)?)
    };
    let pods = match &seam {
        Some(provider) => provider.pods().await?,
        None => Vec::new(),
    };
    let tracked_name = cached
        .get(state::key::NAME)
        .and_then(serde_json::Value::as_str);
    let active: Vec<_> = pods
        .iter()
        .filter(|p| {
            (env.paths.rental != Rental::Runpod
                || Some(p.id.as_str()) == cached.pod_id()
                || p.name.as_deref() == effective.get("POD_NAME")
                || tracked_name.is_some() && p.name.as_deref() == tracked_name)
                && !matches!(p.state, Some(PodState::Stopped | PodState::Terminated))
        })
        .collect();
    for pod in &active {
        let question = Question::yes_no(
            None,
            format!(
                "Stop {} {} ({}) and switch to {}?",
                env.paths.rental.display_name(),
                pod.name.as_deref().unwrap_or("machine"),
                pod.id,
                target.name()
            ),
        );
        let Some(answer) = ask(&question) else {
            return Err(CommandError::Exit(
                "A GPU is still allocated. Run viy stop, then repeat the provider switch.".into(),
            ));
        };
        if answer.await != Answer::Yes {
            return Err(CommandError::Exit("Provider switch cancelled.".into()));
        }
    }
    for pod in active {
        let seam = seam
            .as_ref()
            .ok_or_else(|| CommandError::Exit("Missing outgoing provider".into()))?;
        seam.stop(&pod.id).await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if seam
                .pod(&pod.id)
                .await?
                .is_none_or(|p| matches!(p.state, Some(PodState::Stopped | PodState::Terminated)))
            {
                observe(
                    env.paths,
                    "STOPPED",
                    pod.region.as_deref(),
                    pod.volume.as_deref(),
                )?;
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CommandError::Exit(format!(
                    "{} has not confirmed stop for {}. Selection retained.",
                    seam.name(),
                    pod.id
                )));
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
    let mut said = |n| report(crate::notice::Report::Notice(n));
    tunnel::stop(&env.ssh(&effective), true, &mut said).await?;
    target.save(env.paths)?;
    say(
        report,
        Tone::Ok,
        format!(
            "Selected {}. Run viy up when you want compute.",
            target.name()
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use viy_testkit::scratch::ScratchDir;
    #[test]
    fn selection_and_turn_leases_are_global_but_workspaces_and_pins_are_regional() {
        let home = ScratchDir::new("providers").unwrap();
        let paths = Paths::under(home.path());
        assert_eq!(paths.selected().unwrap().rental, Rental::Runpod);
        let a = lease(&paths, false).unwrap();
        let b = lease(&paths, false).unwrap();
        assert!(lease(&paths, true).is_err());
        drop((a, b));
        let change = lease(&paths, true).unwrap();
        assert!(lease(&paths, false).is_err());
        let target = Rental::nebius("project-test", "region-one").unwrap();
        target.save(&paths).unwrap();
        let selected = paths.selected().unwrap();
        assert_eq!(selected.rental, target);
        assert_ne!(paths.known_hosts_file(), selected.known_hosts_file());
        assert_eq!(paths.pod_lock_file(), selected.pod_lock_file());
        assert_eq!(paths.local_state_file(), selected.local_state_file());
        assert!(current(&paths).is_err());
        drop(change);
        Rental::Runpod.save(&selected).unwrap();
        assert_eq!(
            paths.selected().unwrap().pod_state_file(),
            paths.pod_state_file()
        );
        assert!(Rental::nebius("project-test", "../escape").is_err());
    }
}
