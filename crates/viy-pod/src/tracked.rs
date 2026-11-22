//! The tracked pod, which RunPod decides: rpctl's `resolve_pod`,
//! `find_pod_by_name`, `_cache_pod`, `base_url`, `tunnel_only`, `local_url`
//! and `transport_label`, and regions' `pod_volume_id`. `pod.json` only
//! caches the id ([`crate::state`]): a cached id whose pod still lives
//! costs one GET, and otherwise the pod named `POD_NAME` is adopted, so a
//! pod started from the console, on another machine or by rp is found.
//! The cache is cleared only when RunPod answers that no such pod exists;
//! a failed call is an error, so a live pod is never forgotten because the
//! API was unreachable (rpctl docs/architecture.md 4). Pinned by the
//! pod/resolve fixture.
//!
//! The pods come through the provider seam as [`Pod`]s (WP 9.40, kb d182),
//! and `pod.json` keeps what the seam reads: the name, the price as a
//! number, the volume. Since `viy migrate --finish` (kb d135) `pod.json` is
//! viy's own, so it no longer keeps RunPod's answer as rp wrote it.
//!
//! Where rpctl stops on an answer without a string `id` (a `KeyError`;
//! RunPod always sends one), viy keeps the pod it asked for by the cached
//! id and skips such a pod in the name search, so a malformed answer never
//! clears the cache ([[d054-pod-answer-without-id-keeps-the-cache]]).

use std::fmt;
use std::io;
use std::sync::Arc;

use serde_json::Value;
use viy_config::Config;
use viy_config::Paths;
use viy_config::config::tunnel_enabled;
use viy_config::config::tunnel_local_port;
use viy_http::HttpClient;

use viy_provider::Pod;
use viy_provider::PodState as Run;
use viy_provider::Provider;
use viy_provider::ProviderError;

use crate::api::RunPod;
use crate::api::proxy_url;
use crate::provider::RunPodProvider;
use crate::state;
use crate::state::PodState;
use crate::state::key;

/// How a pod is reached through RunPod's proxy, in words.
pub const PROXY_LABEL: &str = "RunPod http proxy (TLS terminated by RunPod)";
const TUNNEL_LABEL: &str = "SSH tunnel only";
/// A pod from before tunnel mode was created with an http port, a route
/// only recreating it closes: "tunnel only" would be wrong in the
/// direction that matters.
const PREDATES_TUNNEL_LABEL: &str =
    "SSH tunnel -- but this pod predates tunnel mode, so its proxy route is still open";

/// A failed [`resolve`]: RunPod did not answer for sure, or `pod.json`
/// could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    State(#[from] io::Error),
}

/// Several live pods carry `POD_NAME`; the first id is the one taken. A
/// warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiplePods {
    pub name: String,
    pub ids: Vec<String>,
}

impl fmt::Display for MultiplePods {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Multiple pods named '{}' ({}); using {}.",
            self.name,
            self.ids.join(", "),
            self.ids.first().map_or("", String::as_str)
        )
    }
}

/// Our pod as the provider has it, or `None` when it has none: the
/// cached id while that pod lives (stopped included), else the pod named
/// `POD_NAME`, cached for next time.
pub async fn resolve(
    seam: &dyn Provider,
    paths: &Paths,
    cfg: &Config,
    notice: impl FnMut(MultiplePods),
) -> Result<Option<Pod>, ResolveError> {
    let cached = state::load(paths)?.pod_id().map(str::to_string);
    if let Some(id) = &cached
        && let Some(mut pod) = seam.pod(id).await?.filter(not_terminated)
    {
        // An answer without the id keeps the one asked for (d054).
        if pod.id.is_empty() {
            pod.id.clone_from(id);
        }
        cache(paths, &pod)?;
        return Ok(Some(pod));
    }
    let found = find_by_name(seam, cfg, notice).await?;
    match &found {
        Some(pod) => cache(paths, pod)?,
        None if cached.is_some() => state::clear(paths)?,
        None => {}
    }
    Ok(found)
}

/// The live pod named `POD_NAME`: a running one first, then the one
/// started last (its start, else its creation; ISO times sort as text),
/// ties in the provider's order. `None` only when the provider answered.
pub async fn find_by_name(
    seam: &dyn Provider,
    cfg: &Config,
    mut notice: impl FnMut(MultiplePods),
) -> Result<Option<Pod>, ProviderError> {
    let name = cfg.get("POD_NAME").unwrap_or("");
    // The seam lists no terminated pod; one without an id is skipped.
    let mut matches: Vec<Pod> = seam
        .pods()
        .await?
        .into_iter()
        .filter(|p| p.name.as_deref() == Some(name) && !p.id.is_empty() && not_terminated(p))
        .collect();
    matches.sort_by(|a, b| recency(b).cmp(&recency(a)));
    if matches.len() > 1 {
        notice(MultiplePods {
            name: name.to_string(),
            ids: matches.iter().map(|p| p.id.clone()).collect(),
        });
    }
    Ok(matches.into_iter().next())
}

/// The network volume a pod mounts: `networkVolumeId`, else
/// `networkVolume.id`.
pub fn pod_volume_id(pod: &Value) -> Option<&str> {
    nonempty(pod, "networkVolumeId")
        .or_else(|| pod.get("networkVolume").and_then(|v| text(v, "id")))
}

/// The endpoint's base URL, or `None` without a pod. The cached id needs
/// no request; without one, and with `discover` and a RUNPOD_API_KEY,
/// RunPod is asked, and a failed call is no pod. In tunnel mode the URL is
/// the same for every pod, but a pod still has to exist, so "no pod" stays
/// apart from "the tunnel is down".
pub async fn base_url(
    paths: &Paths,
    cfg: &Config,
    http: Arc<dyn HttpClient>,
    discover: bool,
    notice: impl FnMut(MultiplePods),
) -> io::Result<Option<String>> {
    let mut state = state::load(paths)?;
    if state.pod_id().is_none()
        && discover
        && let Some(api_key) = cfg.get("RUNPOD_API_KEY").filter(|k| !k.is_empty())
    {
        let seam = RunPodProvider::new(RunPod::configured(api_key, http, cfg), cfg);
        match resolve(&seam, paths, cfg, notice).await {
            // resolve cached the pod it found.
            Ok(Some(_)) => state = state::load(paths)?,
            Ok(None) | Err(ResolveError::Provider(_)) => {}
            Err(ResolveError::State(e)) => return Err(e),
        }
    }
    let Some(id) = state.pod_id() else {
        return Ok(None);
    };
    Ok(Some(if reached_by_tunnel(&state, cfg) {
        local_url(cfg)
    } else {
        proxy_url(id, cfg.get("VLLM_PORT").unwrap_or(""))
    }))
}

/// Whether the tracked pod is reachable only through the SSH tunnel. Its
/// ports and vLLM's bind address are fixed when it is created, so what
/// `pod.json` recorded then wins over TUNNEL; only a pod that recorded
/// nothing goes by TUNNEL, the intent for the pod `up` is about to make.
pub fn tunnel_only(paths: &Paths, cfg: &Config) -> io::Result<bool> {
    Ok(reached_by_tunnel(&state::load(paths)?, cfg))
}

/// The tunnel's local end, the same for every pod.
pub fn local_url(cfg: &Config) -> String {
    format!("http://127.0.0.1:{}", tunnel_local_port(cfg))
}

/// How the tracked pod is reached, in words.
pub fn transport_label(paths: &Paths, cfg: &Config) -> io::Result<&'static str> {
    let state = state::load(paths)?;
    Ok(
        match (state.tunnel_only(), reached_by_tunnel(&state, cfg)) {
            (_, false) => PROXY_LABEL,
            (Some(_), true) => TUNNEL_LABEL,
            (None, true) => PREDATES_TUNNEL_LABEL,
        },
    )
}

fn reached_by_tunnel(state: &PodState, cfg: &Config) -> bool {
    state.tunnel_only().unwrap_or_else(|| tunnel_enabled(cfg))
}

/// What `up` recorded about the one pod it created. A different pod
/// adopted by name was not made by that `up`, so none of it holds for that
/// pod; the tunnel keys describe the local ssh, not the pod, and stay.
const CREATED_KEYS: [&str; 3] = [key::CREATED_AT, key::TUNNEL_ONLY, key::LAST_STARTUP_SECONDS];

/// Remembers the pod in `pod.json`, keeping the keys it does not set,
/// except [`CREATED_KEYS`] when it is another pod than the cached one: the
/// old pod's `tunnel_only` would make the new one's URL and label claim a
/// tunnel-only route it may not have (rpctl b642713).
fn cache(paths: &Paths, pod: &Pod) -> io::Result<()> {
    let id = pod.id.as_str();
    state::update(paths, |map| {
        if map.get(key::POD_ID).and_then(Value::as_str) != Some(id) {
            for k in CREATED_KEYS {
                map.shift_remove(k);
            }
        }
        map.insert(key::POD_ID.into(), id.into());
        map.insert(key::NAME.into(), pod.name.clone().into());
        map.insert(key::COST_PER_HR.into(), pod.cost_per_hour.into());
        map.insert(key::VOLUME_ID.into(), pod.volume.clone().into());
    })?;
    Ok(())
}

fn not_terminated(pod: &Pod) -> bool {
    pod.state != Some(Run::Terminated)
}

/// Running first, then the latest start.
fn recency(pod: &Pod) -> (bool, &str) {
    let started = pod
        .started_at
        .as_deref()
        .or(pod.created_at.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    (pod.state == Some(Run::Running), started)
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn nonempty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    text(value, key).filter(|s| !s.is_empty())
}

#[cfg(test)]
#[path = "tracked_tests.rs"]
mod tests;
