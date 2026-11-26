use super::*;
use std::net::IpAddr;
use viy_provider::{BoxFuture, LogLine, Pod, PodRequest, PodState, Provider, Volume, Volumes};

const VMS: &str = "compute/v1/instances";
const DISKS: &str = "compute/v1/disks";
const ACCOUNTS: &str = "iam/v1/service-accounts";
const GROUPS: &str = "iam/v1/groups";
const MEMBERS: &str = "iam/v1/group-memberships";
const PERMITS: &str = "iam/v1/access-permits";
impl NebiusProvider {
    fn pod_value(&self, v: &Value) -> Result<Pod> {
        let status = field(v, "/status/state");
        let shape = &v["spec"]["resources"];
        // This record identifies the immutable VM configuration for resume and for offline display.
        let machine = json!({"id":id(v)?, "platform":shape["platform"],"preset":shape["preset"],"spot":v["spec"].get("preemptible").is_some(),"region":self.region});
        write_atomically(&self.dir.join("machine.json"), &machine.to_string()).map_err(error)?;
        let endpoint = field(v, "/status/networkInterfaces/0/publicIpAddress/address")
            .split('/')
            .next()
            .and_then(|s| s.parse::<IpAddr>().ok());
        Ok(Pod {
            id: id(v)?.into(),
            name: Some(field(v, "/metadata/name").into()),
            state: Some(match status {
                "RUNNING" => PodState::Running,
                "STOPPED" => PodState::Stopped,
                "CREATING" | "STARTING" => PodState::Starting,
                _ => PodState::Unknown,
            }),
            state_name: Some(status.into()),
            gpu: Some(field(v, "/spec/resources/platform").into()),
            gpu_name: Some("RTX PRO 6000".into()),
            gpu_count: field(v, "/spec/resources/preset")
                .split("gpu")
                .next()
                .and_then(|s| s.parse().ok()),
            region: Some(self.region.clone()),
            ssh: endpoint.map(|ip| (ip.to_string(), 22)),
            volume: v
                .pointer("/spec/secondaryDisks/0/existingDisk/id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            created_at: Some(field(v, "/metadata/createdAt").into()),
            ..Pod::default()
        })
    }
    fn volume_value(&self, v: &Value) -> Result<Volume> {
        Ok(Volume {
            id: id(v)?.into(),
            name: Some(field(v, "/metadata/name").into()),
            region: Some(self.region.clone()),
            size_gb: number(&v["status"]["sizeBytes"])
                .map(|n| n >> 30)
                .or_else(|| number(&v["spec"]["sizeGibibytes"])),
        })
    }
    async fn require_vm(&self, id: &str) -> Result<Value> {
        self.get(VMS, id)
            .await?
            .ok_or_else(|| error(format!("Nebius VM {id} no longer exists")))
    }
    async fn ensure_named(&self, kind: &str, name: &str, spec: Value) -> Result<String> {
        if let Some(existing) = self
            .list(kind)
            .await?
            .iter()
            .find(|v| field(v, "/metadata/name") == name)
        {
            if !self.owned(existing) {
                return Err(error(format!(
                    "Nebius {name} exists with a different owner"
                )));
            }
            return id(existing).map(str::to_owned);
        }
        self.mutate(
            kind,
            Method::POST,
            kind,
            Some(&json!({"metadata":self.metadata(name),"spec":spec})),
        )
        .await
    }
    pub(super) fn watchdog_slot(&self) -> Result<String> {
        let path = self.dir.join("watchdog-slot");
        match fs::read_to_string(&path) {
            Ok(slot) if slot.len() == 32 && slot.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(slot),
            Ok(_) => Err(error("invalid local watchdog identity")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let slot = random_id()?;
                write_atomically(&path, &slot).map_err(error)?;
                Ok(slot)
            }
            Err(e) => Err(error(e)),
        }
    }

    async fn watchdog_identity(&self) -> Result<(String, String)> {
        let name = format!("idle-{}", self.watchdog_slot()?);
        let account = self.ensure_named(ACCOUNTS, &name, json!({})).await?;
        let group = self.ensure_named(GROUPS, &name, json!({})).await?;
        let members = self
            .account
            .client
            .list(&format!("{MEMBERS}/list-members"), Some(&group))
            .await?;
        if members
            .iter()
            .any(|m| field(m, "/spec/memberId") != account)
        {
            return Err(error("unexpected member in Nebius watchdog group"));
        }
        if members.is_empty() {
            self.mutate(
                MEMBERS,
                Method::POST,
                MEMBERS,
                Some(&json!({"metadata":{"parentId":group},"spec":{"memberId":account}})),
            )
            .await?;
        }
        let memberships = self.account.client.list_member_of(&account).await?;
        if memberships.len() != 1 || id(&memberships[0])? != group {
            return Err(error(
                "Watchdog service account has an unexpected group membership.",
            ));
        }
        Ok((account, group))
    }

    async fn grant(&self, vm: &Value) -> Result<()> {
        let (account, group) = self.watchdog_identity().await?;
        if field(vm, "/spec/serviceAccountId") != account {
            return Err(error("Nebius VM watchdog identity does not match"));
        }
        let instance = id(vm)?;
        let permits = self.account.client.list(PERMITS, Some(&group)).await?;
        let expected = |p: &Value| {
            field(p, "/spec/resourceId") == instance
                && field(p, "/spec/role") == "compute.instance-power-operator"
        };
        if permits.iter().any(|p| !expected(p)) {
            return Err(error("watchdog group has access outside this VM"));
        }
        if permits.is_empty() {
            self.mutate(PERMITS,Method::POST,PERMITS,Some(&json!({"metadata":{"parentId":group,"labels":{"rental-owner":self.owner}},"spec":{"resourceId":instance,"role":"compute.instance-power-operator"}}))).await?;
            let verified = self.account.client.list(PERMITS, Some(&group)).await?;
            if verified.len() != 1 || !expected(&verified[0]) {
                return Err(error("Nebius watchdog grant was not confirmed"));
            }
        }
        Ok(())
    }
    async fn change_power(&self, instance: &str, start: bool) -> Result<()> {
        let vm = self.require_vm(instance).await?;
        let state = field(&vm, "/status/state");
        let (action, wanted) = if start {
            ("start", "RUNNING")
        } else {
            ("stop", "STOPPED")
        };
        if state == wanted {
            return Ok(());
        }
        if start {
            if field(&vm, "/spec/resources/preset") != self.account.settings.preset
                || vm["spec"].get("preemptible").is_some() != self.account.settings.spot
            {
                return Err(error(
                    "The stopped VM has another preset or purchase mode. Select its original configuration or use viy down before replacing it.",
                ));
            }
            self.grant(&vm).await?;
        }
        self.mutate(
            VMS,
            Method::POST,
            &format!("{VMS}/{action}"),
            Some(&json!({"id":instance})),
        )
        .await?;
        let vm = self.require_vm(instance).await?;
        if field(&vm, "/status/state") != wanted {
            return Err(error(format!(
                "Nebius did not confirm {wanted} for {instance}"
            )));
        }
        Ok(())
    }
    pub async fn retained_cost(&self, instance: &str) -> Result<String> {
        let vm = self.require_vm(instance).await?;
        let boot = &vm["spec"]["bootDisk"]["managedDisk"]["spec"];
        let disk_id = field(&vm, "/spec/secondaryDisks/0/existingDisk/id");
        let disk = self
            .get(DISKS, disk_id)
            .await?
            .ok_or_else(|| error("workspace not found"))?;
        let boot_size =
            number(&boot["sizeGibibytes"]).ok_or_else(|| error("unknown boot disk size"))?;
        let size = self
            .volume_value(&disk)?
            .size_gb
            .ok_or_else(|| error("unknown workspace size"))?;
        // USD public list prices per GiB per 730 hours, 2026-08-09.
        // https://docs.nebius.com/compute/resources/pricing#disks
        let rate = |kind: &str| match kind {
            "NETWORK_SSD" => Ok(0.071),
            "NETWORK_SSD_NON_REPLICATED" => Ok(0.053),
            "NETWORK_SSD_IO_M3" => Ok(0.118),
            other => Err(error(format!("unknown disk price for {other}"))),
        };
        let cost = boot_size as f64 * rate(field(boot, "/type"))?
            + size as f64 * rate(field(&disk, "/spec/type"))?;
        Ok(format!(
            "Retained storage: {boot_size} GiB boot + {size} GiB workspace, estimated USD {cost:.2} per 730 hours at published list prices."
        ))
    }

    /// Expiry is opt-in. A VM attachment always protects the workspace.
    pub async fn expire_workspace(&self, now: u64) -> Result<Option<String>> {
        let ttl = u64::from(self.account.settings.workspace_idle_days) * 86400;
        let Some(record) = read_record(self.dir.join("workspace.json"))? else {
            return Ok(None);
        };
        let Some(last) = number(&record["last_used"]) else {
            return Ok(None);
        };
        if ttl == 0 || now.saturating_sub(last) < ttl {
            return Ok(None);
        }
        let disk = field(&record, "/id");
        let Some(resource) = self.get(DISKS, disk).await? else {
            return Ok(None);
        };
        if resource["status"].get("readWriteAttachment").is_some()
            || resource["status"]["readOnlyAttachments"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        {
            return Ok(None);
        }
        Volumes::delete(self, disk).await?;
        Ok(Some(disk.to_owned()))
    }
    pub async fn placement(&self) -> Result<(String, u64)> {
        let tenant = self.account.tenant().await?;
        let advice = self
            .account
            .client
            .list("capacity/v1/resource-advice", Some(&tenant))
            .await?;
        let a = advice
            .iter()
            .find(|a| {
                field(a, "/spec/region") == self.region
                    && matches_gpu(a, &self.account.settings.preset)
                    && number(&a["spec"]["computeInstance"]["preset"]["resources"]["gpuCount"])
                        == Some(1)
                    && a["status"]
                        .get(if self.account.settings.spot {
                            "preemptible"
                        } else {
                            "onDemand"
                        })
                        .is_some()
            })
            .ok_or_else(|| {
                error(format!(
                    "Nebius has no RTX PRO 6000 preset {} in {}",
                    self.account.settings.preset, self.region
                ))
            })?;
        Ok((
            field(a, "/spec/computeInstance/platform").into(),
            number(&a["spec"]["computeInstance"]["preset"]["resources"]["memoryGibibytes"])
                .unwrap_or(0),
        ))
    }
    async fn create_vm(&self, r: &PodRequest) -> Result<Pod> {
        if let Some(existing) = self.list(VMS).await?.into_iter().find(|vm| self.owned(vm))
            && field(&existing, "/metadata/name") != r.name
        {
            return Err(error(format!(
                "An owned Nebius VM ({}) already exists in this project. Use viy down before replacing it.",
                id(&existing)?
            )));
        }
        let volume = r
            .volume
            .as_deref()
            .ok_or_else(|| error("Nebius requires a workspace disk"))?;
        let disk = self
            .get(DISKS, volume)
            .await?
            .ok_or_else(|| error("Nebius workspace disk is missing"))?;
        let claim_file = self.dir.join("workspace-create.json");
        if let Some(claim) = read_record(claim_file.clone())?
            && claim["nonce"].is_string()
            && disk["metadata"]["labels"]["workspace-origin"] == claim["nonce"]
            && disk["metadata"]["name"] == claim["name"]
        {
            let record = self.dir.join("workspace.json");
            if !read_record(record.clone())?.is_some_and(|v| v["id"] == volume) {
                write_atomically(
                    &record,
                    &json!({"id":volume,"initialize":true,"name":claim["name"]}).to_string(),
                )
                .map_err(error)?;
            }
            fs::remove_file(claim_file).map_err(error)?;
        }
        let region = self
            .account
            .settings
            .regions
            .get(&self.region)
            .cloned()
            .unwrap_or_default();
        let (platform, ram) = self.placement().await?;
        if r.min_ram_gb.is_some_and(|min| ram < min)
            || r.gpu_count != 1
            || !self.account.settings.preset.starts_with("1gpu-")
        {
            return Err(error(
                "Nebius VM shape does not meet this profile's GPU count or host RAM requirements",
            ));
        }
        let subnet = if region.subnet.is_empty() {
            match self.list("vpc/v1/subnets").await?.as_slice() {
                [one] => id(one)?.to_owned(),
                _ => return Err(error("Set the regional subnet ID in nebius.json.")),
            }
        } else {
            region.subnet
        };
        validate_id(&subnet)?;
        let image = if region.image.is_empty() {
            let code = self
                .project
                .strip_prefix("project-")
                .and_then(|s| s.get(..3))
                .ok_or_else(|| error("invalid regional project id"))?;
            let img=self.account.client.request(Method::GET,&format!("compute/v1/images/get-latest-by-family?parentId=project-{code}public-images&imageFamily=ubuntu24.04-cuda13.0"),None).await?;
            id(&img)?.to_string()
        } else {
            region.image
        };
        validate_id(&image)?;
        let cloud = guest::cloud_init(r, volume, &self.dir)?;
        let (sa, _) = self.watchdog_identity().await?;
        let mut spec = json!({"resources":{"platform":platform,"preset":self.account.settings.preset},"stopped":true,"serviceAccountId":sa,"recoveryPolicy":"FAIL",
            "networkInterfaces":[{"name":"eth0","subnetId":subnet,"ipAddress":{},"publicIpAddress":{}}],
            "bootDisk":{"attachMode":"READ_WRITE","managedDisk":{"name":format!("boot-{}",&self.owner[..20]),"spec":{"type":"NETWORK_SSD","sizeGibibytes":r.disk_gb.max(100).to_string(),"sourceImageId":image}}},
            "secondaryDisks":[{"attachMode":"READ_WRITE","deviceId":"workspace","existingDisk":{"id":volume}}],"cloudInitUserData":cloud});
        if self.account.settings.spot {
            spec["preemptible"] = json!({"onPreemption":"STOP"});
            spec["followsSpotPrice"] = json!({});
        } else {
            spec["onDemand"] = json!({});
        }
        let iid = self.ensure_named(VMS, &r.name, spec).await?;
        self.change_power(&iid, true).await?;
        self.pod_value(&self.require_vm(&iid).await?)
    }
}
impl Provider for NebiusProvider {
    fn lists_owned_pods(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "Nebius"
    }
    fn pods(&self) -> BoxFuture<'_, Result<Vec<Pod>>> {
        Box::pin(async move {
            self.list(VMS)
                .await?
                .iter()
                .filter(|v| self.owned(v))
                .map(|v| self.pod_value(v))
                .collect()
        })
    }
    fn pod<'a>(&'a self, i: &'a str) -> BoxFuture<'a, Result<Option<Pod>>> {
        Box::pin(async move {
            self.get(VMS, i)
                .await?
                .as_ref()
                .map(|v| self.pod_value(v))
                .transpose()
        })
    }
    fn create<'a>(&'a self, r: &'a PodRequest) -> BoxFuture<'a, Result<Pod>> {
        Box::pin(self.create_vm(r))
    }
    fn start<'a>(&'a self, i: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.change_power(i, true))
    }
    fn stop<'a>(&'a self, i: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.change_power(i, false))
    }
    fn terminate<'a>(&'a self, i: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.get(VMS, i).await?.is_none() {
                return Ok(());
            }
            // Stop before removing the watchdog's ability to stop this machine.
            self.change_power(i, false).await?;
            let groups = self.list(GROUPS).await?;
            for group in groups.iter().filter(|g| self.owned(g)) {
                for permit in self.account.client.list(PERMITS, Some(id(group)?)).await? {
                    if field(&permit, "/spec/resourceId") == i
                        && field(&permit, "/metadata/labels/rental-owner") == self.owner
                    {
                        self.mutate(
                            PERMITS,
                            Method::DELETE,
                            &format!("{PERMITS}/{}", id(&permit)?),
                            None,
                        )
                        .await?;
                    }
                }
            }
            self.mutate(VMS, Method::DELETE, &format!("{VMS}/{i}"), None)
                .await?;
            // A cached bearer from an old VM must never acquire access to the next VM.
            let path = self.dir.join("watchdog-slot");
            if let Ok(slot) = fs::read_to_string(&path) {
                fs::remove_file(&path).map_err(error)?;
                let name = format!("idle-{slot}");
                for kind in [GROUPS, ACCOUNTS] {
                    for resource in self.list(kind).await? {
                        if self.owned(&resource) && field(&resource, "/metadata/name") == name {
                            self.mutate(
                                kind,
                                Method::DELETE,
                                &format!("{kind}/{}", id(&resource)?),
                                None,
                            )
                            .await?;
                        }
                    }
                }
            }
            Ok(())
        })
    }
    fn logs<'a>(
        &'a self,
        i: &'a str,
        tail: u32,
        budget: Duration,
    ) -> BoxFuture<'a, Result<Vec<LogLine>>> {
        Box::pin(async move {
            self.require_vm(i).await?;
            guest::logs(self, i, tail, budget).await
        })
    }
    fn volumes(&self) -> Option<&dyn Volumes> {
        Some(self)
    }
}
impl Volumes for NebiusProvider {
    fn list(&self) -> BoxFuture<'_, Result<Vec<Volume>>> {
        Box::pin(async move {
            self.list(DISKS)
                .await?
                .iter()
                .filter(|v| self.owned(v) && v["status"].get("managedBy").is_none())
                .map(|v| self.volume_value(v))
                .collect()
        })
    }
    fn get<'a>(&'a self, i: &'a str) -> BoxFuture<'a, Result<Option<Volume>>> {
        Box::pin(async move {
            self.get(DISKS, i)
                .await?
                .as_ref()
                .map(|v| self.volume_value(v))
                .transpose()
        })
    }
    fn create<'a>(
        &'a self,
        name: &'a str,
        size: u64,
        region: &'a str,
    ) -> BoxFuture<'a, Result<Volume>> {
        Box::pin(async move {
            if region != self.region || size == 0 {
                return Err(error("invalid Nebius workspace region or disk size"));
            }
            let tier = self
                .account
                .settings
                .regions
                .get(region)
                .map(|r| r.disk_type.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("NETWORK_SSD");
            match tier {
                "NETWORK_SSD" if size <= 8192 => {}
                "NETWORK_SSD_NON_REPLICATED" | "NETWORK_SSD_IO_M3"
                    if size.is_multiple_of(93) && size <= 262074 => {}
                _ => {
                    return Err(error(format!(
                        "Invalid {tier} disk size {size} GiB. NETWORK_SSD supports 1–8192 GiB. NRD and IO M3 require multiples of 93 GiB up to 262074."
                    )));
                }
            }
            let claim_file = self.dir.join("workspace-create.json");
            let claim = match read_record(claim_file.clone())? {
                Some(v) if v["name"] == name => v,
                Some(_) => return Err(error("A workspace creation is pending for another name.")),
                None => {
                    let v = json!({"name":name,"nonce":random_id()?});
                    write_atomically(&claim_file, &v.to_string()).map_err(error)?;
                    v
                }
            };
            let mut metadata = self.metadata(name);
            metadata["labels"]["workspace-origin"] = claim["nonce"].clone();
            let i = self
                .mutate(
                    DISKS,
                    Method::POST,
                    DISKS,
                    Some(&json!({"metadata":metadata,
                "spec":{"sizeGibibytes":size.to_string(),"type":tier}})),
                )
                .await?;
            let v = self
                .get(DISKS, &i)
                .await?
                .ok_or_else(|| error("new Nebius disk is not visible"))?;
            if v["metadata"]["labels"]["workspace-origin"] != claim["nonce"] {
                return Err(error(
                    "Nebius workspace creation identity differs from the local request",
                ));
            }
            write_atomically(
                &self.dir.join("workspace.json"),
                &json!({"id":i,"name":name,"size_gib":size,"tier":tier,"initialize":true})
                    .to_string(),
            )
            .map_err(error)?;
            fs::remove_file(claim_file).map_err(error)?;
            self.volume_value(&v)
        })
    }
    fn delete<'a>(&'a self, i: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let v = self
                .get(DISKS, i)
                .await?
                .ok_or_else(|| error("Nebius disk no longer exists"))?;
            if v["status"].get("managedBy").is_some()
                || v["status"].get("readWriteAttachment").is_some()
            {
                return Err(error("Nebius disk is managed or attached"));
            }
            self.mutate(DISKS, Method::DELETE, &format!("{DISKS}/{i}"), None)
                .await?;
            let file = self.dir.join("workspace.json");
            if read_record(file.clone())?.is_some_and(|v| v["id"] == i) {
                fs::remove_file(file).map_err(error)?;
            }
            Ok(())
        })
    }
}
