//! Nebius Compute. Account settings come exclusively from the user's global configuration.
mod client;
mod compute;
mod guest;
use std::{collections::BTreeMap, fs, io, path::PathBuf, sync::Arc, time::Duration};

pub use client::ServiceKey;
/// Launch with the client's guest code even when cloud-init ran on an older release.
/// Only fixed program text goes into SSH arguments. Runtime values use stdin.
pub fn engine_launch_command(repair_workspace: bool) -> String {
    let repair = if repair_workspace {
        include_str!("../assets/retry-launch.sh")
    } else {
        ""
    };
    // A JSON string is a Python string literal.
    let watchdog = serde_json::Value::from(include_str!("../assets/watchdog.py"));
    let launcher = format!(
        "WATCHDOG = {watchdog}\n{}",
        include_str!("../assets/launch.py")
    )
    .replace('\'', "'\\''");
    format!("{repair}\nexec sudo -n python3 -c '{launcher}'")
}
/// Prints the last lines of the engine's container on the VM, each after its
/// time. The VM's own log is its serial console, which never carries them.
pub const ENGINE_LOG_COMMAND: &str =
    "sudo -n docker logs --timestamps --tail 200 model-server 2>&1";
use client::{Client, error, field, number};
use serde::Deserialize;
use serde_json::{Value, json};
use viy_config::{Paths, Rental, atomic::write_atomically};
use viy_http::{HttpClient, Method};
use viy_provider::{ProviderError, Result};

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Region {
    pub project: String,
    pub subnet: String,
    pub image: String,
    pub disk_type: String,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub profile: String,
    pub service_key: Option<ServiceKey>,
    pub tenant: String,
    pub region: String,
    pub preset: String,
    pub spot: bool,
    pub workspace_idle_days: u32,
    pub regions: BTreeMap<String, Region>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            profile: "gpu-personal".into(),
            service_key: None,
            tenant: String::new(),
            region: "eu-south1".into(),
            preset: "1gpu-24vcpu-218gb".into(),
            spot: false,
            workspace_idle_days: 0,
            regions: BTreeMap::new(),
        }
    }
}
impl Settings {
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_dir().join("nebius.json");
        match fs::read(&file) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(error(e)),
            Ok(data) => {
                serde_json::from_slice(&data).map_err(|e| error(format!("{}: {e}", file.display())))
            }
        }
    }
}

pub struct Account {
    client: Client,
    pub settings: Settings,
}
impl Account {
    pub fn new(paths: &Paths, http: Arc<dyn HttpClient>) -> Result<Self> {
        let settings = Settings::load(paths)?;
        let client = Client::new(http, settings.profile.clone(), settings.service_key.clone());
        Ok(Self { client, settings })
    }

    /// Allow the native CLI to show its sign-in link on a foreground terminal.
    pub fn with_terminal_login(mut self, enabled: bool) -> Self {
        self.client.terminal_login = enabled;
        self
    }

    async fn tenant(&self) -> Result<String> {
        if !self.settings.tenant.is_empty() {
            return Ok(self.settings.tenant.clone());
        }
        let rows = self.client.list("iam/v2/tenants", None).await?;
        match rows.as_slice() {
            [one] => id(one).map(str::to_owned),
            _ => Err(error(
                "Set tenant in ~/.config/viy/nebius.json to select your Nebius tenant.",
            )),
        }
    }
    pub async fn select(&self, region: &str) -> Result<Rental> {
        let tenant = self.tenant().await?;
        let projects = self.client.list("iam/v2/projects", Some(&tenant)).await?;
        let configured = self
            .settings
            .regions
            .get(region)
            .map(|r| r.project.as_str())
            .unwrap_or("");
        let matching: Vec<_> = projects
            .iter()
            .filter(|p| {
                field(p, "/spec/region") == region
                    && (configured.is_empty() || field(p, "/metadata/id") == configured)
            })
            .collect();
        match matching.as_slice() {
            [one] => Rental::nebius(id(one)?, region).map_err(error),
            _ => Err(error(format!(
                "Select an accessible project for {region} in ~/.config/viy/nebius.json"
            ))),
        }
    }

    pub async fn stock(&self, paths: &Paths) -> Result<String> {
        let tenant = self.tenant().await?;
        let (advice, projects) = tokio::try_join!(
            self.client
                .list("capacity/v1/resource-advice", Some(&tenant)),
            self.client.list("iam/v2/projects", Some(&tenant))
        )?;
        let mut lines = Vec::new();
        for a in advice
            .iter()
            .filter(|a| matches_gpu(a, &self.settings.preset))
        {
            let region = field(a, "/spec/region");
            let configured = self
                .settings
                .regions
                .get(region)
                .map(|r| r.project.as_str())
                .unwrap_or("");
            let candidates: Vec<_> = projects
                .iter()
                .filter(|p| {
                    field(p, "/spec/region") == region
                        && (configured.is_empty() || field(p, "/metadata/id") == configured)
                })
                .collect();
            let placement = if let [project] = candidates.as_slice() {
                let mut scoped = paths.clone();
                scoped.rental = Rental::nebius(id(project)?, region).map_err(error)?;
                if scoped.pod_state_dir().join("workspace.json").exists() {
                    "workspace recorded"
                } else {
                    "project available"
                }
            } else {
                "configure project"
            };
            for (mode, key) in [("on-demand", "onDemand"), ("spot", "preemptible")] {
                let Some(status) = a["status"].get(key) else {
                    continue;
                };
                let count =
                    number(&status["available"]).map_or_else(|| "?".into(), |n| n.to_string());
                lines.push(format!(
                    "{region:<14} {mode:<10} {count:>4}  {}  {}  {}  {placement}",
                    field(status, "/availabilityLevel"),
                    field(status, "/dataState"),
                    field(status, "/effectiveAt")
                ));
            }
        }
        lines.sort();
        Ok(format!(
            "RTX PRO 6000 · {}\nRegion         Mode       Free  Availability / freshness / observed at / workspace\n{}",
            self.settings.preset,
            if lines.is_empty() {
                "No matching capacity advice.".into()
            } else {
                lines.join("\n")
            }
        ))
    }
}
fn matches_gpu(a: &Value, preset: &str) -> bool {
    matches!(
        field(a, "/spec/computeInstance/platform"),
        "gpu-rtx6000" | "gpu-rtx6000-a"
    ) && field(a, "/spec/computeInstance/preset/name") == preset
}
fn id(v: &Value) -> Result<&str> {
    let id = field(v, "/metadata/id");
    validate_id(id)?;
    Ok(id)
}
fn random_id() -> Result<String> {
    use rand::{TryRngCore, rngs::OsRng};
    let mut bytes = [0u8; 16];
    OsRng.try_fill_bytes(&mut bytes).map_err(error)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 160
        || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        Err(error("invalid Nebius resource ID"))
    } else {
        Ok(())
    }
}
fn read_record(file: PathBuf) -> Result<Option<Value>> {
    match fs::read(file) {
        Ok(b) => serde_json::from_slice(&b).map(Some).map_err(error),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(error(e)),
    }
}

pub struct NebiusProvider {
    account: Account,
    project: String,
    region: String,
    owner: String,
    dir: PathBuf,
}
impl NebiusProvider {
    pub fn with_terminal_login(mut self, enabled: bool) -> Self {
        self.account = self.account.with_terminal_login(enabled);
        self
    }

    pub fn new(paths: &Paths, http: Arc<dyn HttpClient>) -> Result<Self> {
        let Rental::Nebius { project, region } = &paths.rental else {
            return Err(error("Nebius project is not selected"));
        };
        let account = Account::new(paths, http)?;
        let dir = paths.pod_state_dir();
        let owner_path = dir.join("owner");
        fs::create_dir_all(&dir).map_err(error)?;
        // Use one file lock for installation identity creation, including concurrent status calls.
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("owner.lock"))
            .map_err(error)?;
        lock.lock().map_err(error)?;
        let owner = match fs::read_to_string(&owner_path) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let value = random_id()?;
                write_atomically(&owner_path, &value).map_err(error)?;
                value
            }
            Err(e) => return Err(error(e)),
        };
        if owner.len() != 32 || !owner.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(error("invalid local Nebius owner record"));
        }
        Ok(Self {
            account,
            project: project.clone(),
            region: region.clone(),
            owner,
            dir,
        })
    }
    fn owned(&self, v: &Value) -> bool {
        field(v, "/metadata/parentId") == self.project
            && field(v, "/metadata/labels/rental-owner") == self.owner
    }
    fn metadata(&self, name: &str) -> Value {
        json!({"parentId":self.project,"name":name,"labels":{"rental-owner":self.owner}})
    }
    async fn list(&self, kind: &str) -> Result<Vec<Value>> {
        self.account.client.list(kind, Some(&self.project)).await
    }
    async fn get(&self, kind: &str, id: &str) -> Result<Option<Value>> {
        validate_id(id)?;
        match self
            .account
            .client
            .request(Method::GET, &format!("{kind}/{id}"), None)
            .await
        {
            Ok(v) if self.owned(&v) => Ok(Some(v)),
            Ok(_) => Err(error(format!("Refusing unowned Nebius resource {id}"))),
            Err(e) if e.status == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// Reconcile a reply that was lost before its operation ID could be saved.
    /// Only accept an observed resource owned by this installation or the confirmed power state.
    async fn observed_intent(&self, kind: &str, intent: &Value) -> Result<bool> {
        let path = field(intent, "/path");
        let target = field(intent, "/target");
        if !target.is_empty() && (path.ends_with("/start") || path.ends_with("/stop")) {
            return Ok(self.get(kind, target).await?.is_some_and(|v| {
                field(&v, "/status/state")
                    == if path.ends_with("/start") {
                        "RUNNING"
                    } else {
                        "STOPPED"
                    }
            }));
        }
        let name = field(intent, "/name");
        if path == kind && !name.is_empty() {
            return Ok(self
                .list(kind)
                .await?
                .iter()
                .any(|v| self.owned(v) && field(v, "/metadata/name") == name));
        }
        Ok(false)
    }

    async fn mutate(
        &self,
        kind: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<String> {
        // One durable operation per service; lifecycle commands serialize on pod.lock.
        let journal = self.dir.join(format!(
            "{}.operation",
            kind.rsplit('/').next().unwrap_or("compute")
        ));
        let fingerprint = ring::digest::digest(
            &ring::digest::SHA256,
            format!("{method} {path} {}", body.unwrap_or(&Value::Null)).as_bytes(),
        );
        let fingerprint: String = fingerprint
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut saved = read_record(journal.clone())?;
        if let Some(prior) = &saved
            && prior["request"] != fingerprint
            && prior["operation"].is_null()
            && self.observed_intent(kind, prior).await?
        {
            fs::remove_file(&journal).map_err(error)?;
            saved = None;
        }
        let mut intent = match saved {
            Some(saved) => saved,
            None => {
                let request = body.unwrap_or(&Value::Null);
                let intent = json!({"request":fingerprint, "key":random_id()?, "path":path,
                    "name":field(request,"/metadata/name"), "target":field(request,"/id")});
                write_atomically(&journal, &intent.to_string()).map_err(error)?;
                intent
            }
        };
        let same = intent["request"] == fingerprint;
        let op = if let Some(oid) = intent["operation"].as_str() {
            validate_id(oid)?;
            self.wait_operation(kind, oid).await?
        } else {
            if !same {
                return Err(error(format!(
                    "A prior Nebius request for {} is unconfirmed. Retry that command first.",
                    intent["path"]
                )));
            }
            let response = self
                .account
                .client
                .request_key(method, path, body, intent["key"].as_str())
                .await;
            let op = match response {
                Err(e) if !e.transient && e.status.is_some() => {
                    fs::remove_file(&journal).map_err(error)?;
                    return Err(e);
                }
                other => other?,
            };
            let oid = field(&op, "/id").to_string();
            validate_id(&oid)?;
            intent["operation"] = oid.clone().into();
            intent["resource"] = op["resourceId"].clone();
            write_atomically(&journal, &intent.to_string()).map_err(error)?;
            if op.get("status").is_some() {
                op
            } else {
                self.wait_operation(kind, &oid).await?
            }
        };
        fs::remove_file(journal).map_err(error)?;
        let result = operation_result(&op)?;
        if !same {
            return Err(error(
                "Recovered the previous Nebius operation. Repeat this command.",
            ));
        }
        Ok(result)
    }
    async fn wait_operation(&self, kind: &str, oid: &str) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
        loop {
            let op = self
                .account
                .client
                .request(Method::GET, &format!("{kind}/operations/{oid}"), None)
                .await?;
            if op.get("status").is_some() {
                return Ok(op);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ProviderError {
                    transient: true,
                    ..error(format!("Nebius operation {oid} is still pending"))
                });
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
fn operation_result(op: &Value) -> Result<String> {
    let status = &op["status"];
    let code = number(&status["code"]).unwrap_or(0);
    if code != 0 {
        return Err(client::api_error(
            status,
            match code {
                7 => 403,
                5 => 404,
                8 => 429,
                16 => 401,
                14 => 503,
                _ => 400,
            },
        ));
    }
    Ok(field(op, "/resourceId").to_owned())
}

fn unix_time() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(error)?
        .as_secs())
}

pub fn workspace_mounted(paths: &Paths) -> Result<()> {
    let file = paths.pod_state_dir().join("workspace.json");
    if let Some(mut record) = read_record(file.clone())? {
        record["last_used"] = unix_time()?.into();
        record["initialize"] = false.into();
        write_atomically(&file, &record.to_string()).map_err(error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
