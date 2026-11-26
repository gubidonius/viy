use super::*;
use std::path::Path;
use viy_exec::{Command, ExecBackend, SystemExec};
use viy_provider::{LogLine, PodRequest};

const PROBE_SIGNATURES: &str = r#"wipefs --no-act --noheadings --output TYPE "$dev""#;

pub fn cloud_init(r: &PodRequest, volume: &str, dir: &Path) -> Result<String> {
    let env = |name: &str| {
        r.env
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let pubkey = env("PUBLIC_KEY").ok_or_else(|| error("Nebius requires an SSH public key"))?;
    let seconds = |k: &str| -> Result<u64> {
        let n = env(k).unwrap_or("").parse::<f64>().map_err(error)?;
        if !(0.0..=31536000.).contains(&n) {
            return Err(error("invalid watchdog duration"));
        }
        Ok(n as u64)
    };
    let idle = seconds("RPCTL_IDLE_TTL_S")?;
    let cap = seconds("RPCTL_MAX_POD_S")?;
    let port = env("NEBIUS_ENGINE_PORT")
        .unwrap_or("8000")
        .parse::<u16>()
        .map_err(error)?;
    // Only a disk freshly created and recorded by this installation may be initialized.
    let initialize = read_record(dir.join("workspace.json"))?
        .is_some_and(|v| v["id"] == volume && v["initialize"] == true);
    let mount = format!(
        r#"#!/bin/bash
set -eu
dev=/dev/disk/by-id/virtio-workspace
test -b "$dev"
if ! blkid "$dev" >/dev/null; then
    test '{initialize}' = true
    test ! -e /var/lib/gpu-workspace-initialized
    signatures=$({PROBE_SIGNATURES})
    test -z "$signatures"
    mkfs.ext4 -F "$dev"
fi
test "$(blkid -s TYPE -o value "$dev")" = ext4
mkdir -p /workspace
mountpoint -q /workspace || mount "$dev" /workspace
test "$(findmnt -nro MAJ:MIN --mountpoint /workspace)" = "$(lsblk -dnro MAJ:MIN "$dev")"
touch /var/lib/gpu-workspace-initialized
"#
    );
    let unit = format!(
        "[Unit]\nAfter=network-online.target\n[Service]\nExecStart=/usr/bin/python3 /usr/local/sbin/gpu-idle.py {port} {idle} {cap}\nRestart=always\nRestartSec=5\n[Install]\nWantedBy=multi-user.target\n"
    );
    let file = |path: &str, permissions: &str, content: &str| json!({"path":path,"permissions":permissions,"content":content});
    let cloud = json!({"users":[{"name":"worker","shell":"/bin/bash","sudo":"ALL=(ALL) NOPASSWD:ALL","ssh_authorized_keys":[pubkey]}],
        "ssh_pwauth":false,"write_files":[file("/usr/local/sbin/gpu-idle.py","0700",include_str!("../assets/watchdog.py")),file("/usr/local/sbin/gpu-launch","0600",include_str!("../assets/launch.py")),file("/usr/local/sbin/gpu-workspace","0700",&mount),file("/etc/systemd/system/gpu-idle.service","0644",&unit)],
        "runcmd":[["systemctl","daemon-reload"],["systemctl","enable","--now","gpu-idle.service"],["sh","-c","{ printf 'sshd host key: '; ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub; } > /dev/ttyS0"]]});
    Ok(format!("#cloud-config\n{cloud}\n"))
}

pub(crate) async fn logs(
    p: &NebiusProvider,
    id: &str,
    tail: u32,
    budget: Duration,
) -> Result<Vec<LogLine>> {
    let token = p.account.client.token(None).await?;
    let config = p.dir.join("serial-profile.yaml");
    // This CLI requires a real profile, even with an access token. Only the pipe path is persisted.
    write_atomically(&config, "default: gpu-runtime\nprofiles:\n  gpu-runtime:\n    endpoint: api.nebius.cloud\n    token-file: /dev/stdin\n").map_err(error)?;
    let cmd = Command::new("nebius")
        .arg("--config")
        .arg(config)
        .args([
            "--profile",
            "gpu-runtime",
            "--no-browser",
            "--no-check-update",
            "--color=false",
            "compute",
            "instance",
            "logs",
            "--project-id",
            &p.project,
            "--id",
            id,
            "--limit",
            &tail.clamp(1, 5000).to_string(),
            "--since",
            "336h",
        ])
        .stdin(token)
        .timeout(budget.max(Duration::from_secs(10)));
    let output = SystemExec.run(cmd).await.map_err(error)?;
    if !output.status.success() {
        return Err(error("Nebius instance serial-log query failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .map(|line| LogLine {
            time: None,
            source: Some("system".into()),
            text: line.into(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::PROBE_SIGNATURES;
    use std::{fs, process::Command};
    use viy_testkit::scratch::ScratchDir;

    #[test]
    fn workspace_probe_accepts_blank_media_and_preserves_existing_signatures() {
        let scratch = ScratchDir::new("workspace-probe").unwrap();
        let image = scratch.path().join("disk.img");
        fs::File::create(&image).unwrap().set_len(8 << 20).unwrap();
        let probe = || {
            let out = Command::new("sh")
                .args(["-c", PROBE_SIGNATURES])
                .env("dev", &image)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        assert!(probe().trim().is_empty());
        assert!(
            Command::new("mkfs.ext4")
                .arg("-q")
                .arg(&image)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(probe().trim(), "ext4");
        assert_eq!(probe().trim(), "ext4");
    }
}
