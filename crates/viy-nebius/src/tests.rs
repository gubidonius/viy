use super::*;
use viy_provider::{PodRequest, Provider, Volumes};
use viy_testkit::{
    http::{FakeHttp, Reply},
    scratch::ScratchDir,
};

fn reply(v: Value) -> Reply {
    Reply::answer(200, v.to_string())
}
fn fixture(replies: Vec<Reply>) -> (ScratchDir, Paths, Arc<FakeHttp>, NebiusProvider) {
    let scratch = ScratchDir::new("nebius").unwrap();
    let mut paths = Paths::under(scratch.path());
    paths.rental = Rental::nebius("project-test", "test-region").unwrap();
    let http = Arc::new(FakeHttp::new(replies));
    let provider = NebiusProvider::new(&paths, http.clone()).unwrap();
    provider.account.client.synthetic_token();
    (scratch, paths, http, provider)
}

#[tokio::test]
async fn foreign_machines_and_attached_disks_cannot_be_deleted() {
    let (_scratch, _, http, p) = fixture(vec![reply(
        json!({"metadata":{"id":"vm-test","parentId":"project-other","labels":{"rental-owner":"foreign"}}}),
    )]);
    assert!(
        p.terminate("vm-test")
            .await
            .unwrap_err()
            .message
            .contains("unowned")
    );
    assert_eq!(http.sent().len(), 1);
    assert_eq!(http.sent()[0].method, Method::GET);
    let (scratch, paths, _, p) = fixture(vec![]);
    let disk = json!({"metadata":p.metadata("weights"),"status":{"readWriteAttachment":{"instanceId":"vm-test"}}});
    let http = Arc::new(FakeHttp::new([reply(disk)]));
    let p = NebiusProvider::new(&paths, http.clone()).unwrap();
    p.account.client.synthetic_token();
    assert!(
        Volumes::delete(&p, "disk-test")
            .await
            .unwrap_err()
            .message
            .contains("attached")
    );
    assert_eq!(http.sent().len(), 1);
    drop(scratch);
}

#[tokio::test]
async fn interrupted_create_recovers_original_operation_without_a_second_post() {
    let (_scratch, paths, http, p) = fixture(vec![
        reply(json!({"id":"op-test","resourceId":"disk-test"})),
        Reply::answer(503, "{}"),
        reply(json!({"id":"op-test","resourceId":"disk-test","status":{}})),
    ]);
    let path = "compute/v1/disks";
    let spec = json!({"metadata":p.metadata("weights")});
    assert!(
        p.mutate(path, Method::POST, path, Some(&spec))
            .await
            .unwrap_err()
            .transient
    );
    let restarted = NebiusProvider::new(&paths, http.clone()).unwrap();
    restarted.account.client.synthetic_token();
    assert_eq!(
        restarted
            .mutate(path, Method::POST, path, Some(&spec))
            .await
            .unwrap(),
        "disk-test"
    );
    assert_eq!(
        http.sent()
            .iter()
            .filter(|r| r.method == Method::POST)
            .count(),
        1
    );
    assert!(!paths.pod_state_dir().join("disks.operation").exists());
}

#[tokio::test]
async fn lost_response_reuses_idempotency_key_and_checks_different_intent() {
    let (_scratch, _, http, p) = fixture(vec![
        Reply::answer(503, "{}"),
        reply(json!({"id":"op-test","status":{},"resourceId":"vm-test"})),
    ]);
    let path = "compute/v1/instances";
    assert!(p.mutate(path, Method::POST, path, None).await.is_err());
    assert!(
        p.mutate(path, Method::DELETE, &format!("{path}/vm-other"), None)
            .await
            .is_err()
    );
    assert_eq!(
        p.mutate(path, Method::POST, path, None).await.unwrap(),
        "vm-test"
    );
    let sent = http.sent();
    assert_eq!(sent.len(), 2);
    let key = sent[0].header_value("X-Idempotency-Key").unwrap();
    assert_eq!(sent[1].header_value("X-Idempotency-Key"), Some(key));
}

#[tokio::test]
async fn stock_uses_exact_shape_and_reports_zero_separately_from_unknown() {
    let tenant = json!({"metadata":{"id":"tenant-test"}});
    let advice = json!({"spec":{"region":"test-region","computeInstance":{"platform":"gpu-rtx6000","preset":{"name":"1gpu-24vcpu-218gb"}}},"status":{"onDemand":{"available":"0"},"preemptible":{"availabilityLevel":"LOW"}}});
    let mut other = advice.clone();
    other["spec"]["computeInstance"]["preset"]["name"] = "8gpu-other".into();
    let (_scratch, paths, _, p) = fixture(vec![
        reply(json!({"items":[tenant]})),
        reply(json!({"items":[advice,other]})),
        reply(json!({"items":[]})),
    ]);
    let stock = p.account.stock(&paths).await.unwrap();
    assert_eq!(stock.matches("test-region").count(), 2);
    assert!(
        stock
            .lines()
            .any(|line| line.contains("on-demand") && line.split_whitespace().nth(2) == Some("0"))
    );
    assert!(
        stock
            .lines()
            .any(|line| line.contains("spot") && line.split_whitespace().nth(2) == Some("?"))
    );
    assert!(stock.contains("configure project"));
}

#[test]
fn cloud_init_contains_only_public_configuration_and_formats_only_the_recorded_disk() {
    let (_scratch, paths, _, p) = fixture(vec![]);
    let request = PodRequest {
        env: vec![
            ("PUBLIC_KEY".into(), "ssh-ed25519 test".into()),
            ("RPCTL_IDLE_TTL_S".into(), "30".into()),
            ("RPCTL_MAX_POD_S".into(), "120".into()),
            ("HF_TOKEN".into(), "private-placeholder".into()),
        ],
        command: vec!["secret-script".into()],
        ..Default::default()
    };
    write_atomically(
        &p.dir.join("workspace.json"),
        r#"{"id":"new-disk","initialize":true}"#,
    )
    .unwrap();
    for (disk, allowed) in [("new-disk", true), ("old-disk", false)] {
        let cloud = guest::cloud_init(&request, disk, &p.dir).unwrap();
        assert!(!cloud.contains("private-placeholder") && !cloud.contains("secret-script"));
        assert!(cloud.contains(&format!("test '{allowed}' = true")));
        assert!(cloud.contains("findmnt") && cloud.contains("/dev/ttyS0"));
    }
    workspace_mounted(&paths).unwrap();
    assert!(
        guest::cloud_init(&request, "new-disk", &p.dir)
            .unwrap()
            .contains("test 'false' = true")
    );
}

#[tokio::test]
async fn vm_is_created_stopped_and_a_missing_grant_never_starts_compute() {
    use viy_testkit::http::RouteHttp;
    let (_home, paths, _, reference) = fixture(vec![]);
    let owner = reference.owner.clone();
    let identity = format!("idle-{}", reference.watchdog_slot().unwrap());
    let route = Arc::new(RouteHttp::new(move |req| {
        let path = req
            .url
            .strip_prefix("https://api.nebius.cloud/")
            .unwrap()
            .split('?')
            .next()
            .unwrap();
        let metadata = |id: &str, name: &str| json!({"id":id,"name":name,"parentId":"project-test","labels":{"rental-owner":owner}});

        match (req.method.as_str(), path) {
            ("GET", "compute/v1/instances") => reply(json!({})),
            ("GET", "compute/v1/disks/disk-test") => {
                reply(json!({"metadata":metadata("disk-test","weights")}))
            }
            ("GET", "capacity/v1/resource-advice") => reply(
                json!({"items":[{"spec":{"region":"test-region","computeInstance":{"platform":"gpu-rtx6000","preset":{"name":"1gpu-24vcpu-218gb","resources":{"memoryGibibytes":"218","gpuCount":"1"}}}},"status":{"onDemand":{}}}]}),
            ),
            ("GET", "iam/v1/service-accounts") => {
                reply(json!({"items":[{"metadata":metadata("sa-test",&identity)}]}))
            }
            ("GET", "iam/v1/groups") => {
                reply(json!({"items":[{"metadata":metadata("group-test",&identity)}]}))
            }
            ("GET", "iam/v1/group-memberships/list-members") => {
                reply(json!({"memberships":[{"spec":{"memberId":"sa-test"}}]}))
            }
            ("GET", "iam/v1/group-memberships/list-member-of") => {
                reply(json!({"items":[{"metadata":{"id":"group-test"}}]}))
            }
            ("POST", "compute/v1/instances") => {
                let data: Value = serde_json::from_slice(req.body.as_ref().unwrap()).unwrap();
                assert_eq!(data["spec"]["stopped"], true);
                assert_eq!(data["spec"]["serviceAccountId"], "sa-test");
                assert_eq!(
                    data["spec"]["secondaryDisks"][0]["existingDisk"]["id"],
                    "disk-test"
                );
                assert_eq!(
                    data["spec"]["bootDisk"]["managedDisk"]["spec"]["sourceImageId"],
                    "img-test"
                );
                assert!(!data.to_string().contains("private-placeholder"));
                reply(json!({"id":"op-test","resourceId":"vm-test","status":{}}))
            }
            ("GET", "compute/v1/instances/vm-test") => reply(
                json!({"metadata":metadata("vm-test","model"),"status":{"state":"STOPPED"},"spec":{"resources":{"preset":"1gpu-24vcpu-218gb"},"serviceAccountId":"sa-test"}}),
            ),
            ("GET", "iam/v1/access-permits") => reply(json!({})),
            ("POST", "iam/v1/access-permits") => {
                let data: Value = serde_json::from_slice(req.body.as_ref().unwrap()).unwrap();
                assert_eq!(data["metadata"]["parentId"], "group-test");
                assert_eq!(
                    data["spec"],
                    json!({"resourceId":"vm-test","role":"compute.instance-power-operator"})
                );
                Reply::answer(403, "{}")
            }
            other => panic!("unexpected request {other:?}"),
        }
    }));
    let mut p = NebiusProvider::new(&paths, route.clone()).unwrap();
    p.account.client.synthetic_token();
    p.account.settings.tenant = "tenant-test".into();
    p.account.settings.regions.insert(
        "test-region".into(),
        Region {
            subnet: "net-test".into(),
            image: "img-test".into(),
            ..Default::default()
        },
    );
    let request = PodRequest {
        name: "model".into(),
        volume: Some("disk-test".into()),
        gpu_count: 1,
        env: vec![
            ("PUBLIC_KEY".into(), "ssh-ed25519 fixture".into()),
            ("RPCTL_IDLE_TTL_S".into(), "10".into()),
            ("RPCTL_MAX_POD_S".into(), "100".into()),
            ("VLLM_API_KEY".into(), "private-placeholder".into()),
        ],
        ..Default::default()
    };
    let err = Provider::create(&p, &request).await.unwrap_err();
    assert_eq!(err.status, Some(403));
    assert!(route.sent().iter().all(|r| !r.url.ends_with("/start")));
}

#[tokio::test]
async fn stopping_waits_for_confirmation_and_down_deletes_only_the_vm() {
    let (_home, paths, _, p) = fixture(vec![]);
    let mut metadata = p.metadata("model");
    metadata["id"] = "vm-test".into();
    let vm = |state| json!({"metadata":metadata,"status":{"state":state},"spec":{"secondaryDisks":[{"existingDisk":{"id":"disk-test"}}]}});
    let operation = || reply(json!({"id":"op-test","resourceId":"vm-test","status":{}}));
    let http = Arc::new(FakeHttp::new([
        reply(vm("RUNNING")),
        operation(),
        reply(vm("STOPPED")),
        reply(vm("STOPPED")),
        reply(vm("STOPPED")),
        reply(json!({})),
        operation(),
    ]));
    let p = NebiusProvider::new(&paths, http.clone()).unwrap();
    p.account.client.synthetic_token();
    p.stop("vm-test").await.unwrap();
    p.terminate("vm-test").await.unwrap();
    let calls = http.sent();
    let changes: Vec<_> = calls.iter().filter(|r| r.method != Method::GET).collect();
    assert_eq!(changes.len(), 2);
    assert!(changes[0].url.ends_with("/instances/stop"));
    assert_eq!(changes[1].method, Method::DELETE);
    assert!(changes[1].url.ends_with("/instances/vm-test"));
}

#[test]
fn the_launch_command_carries_this_releases_watchdog() {
    // The remote shell unquotes the launcher: print it instead of running it.
    let command = engine_launch_command(false).replace("exec sudo -n python3 -c ", "printf %s ");
    let source = std::process::Command::new("sh")
        .args(["-c", &command])
        .output()
        .unwrap();
    assert!(source.status.success());
    let mut check = std::process::Command::new("python3")
        .args([
            "-I",
            "-c",
            "import ast, sys\n\
             tree = ast.parse(sys.stdin.read())\n\
             first = tree.body[0]\n\
             assert first.targets[0].id == 'WATCHDOG'\n\
             assert ast.literal_eval(first.value) == open(sys.argv[1]).read()",
            "assets/watchdog.py",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    check
        .stdin
        .take()
        .unwrap()
        .write_all(&source.stdout)
        .unwrap();
    assert!(check.wait().unwrap().success());
}

#[test]
fn deployed_guest_programs() {
    let status = std::process::Command::new("python3")
        .args(["-B", "tests/guest.py"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap();
    assert!(status.success());
}
