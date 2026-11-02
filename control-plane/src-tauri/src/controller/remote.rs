//! Opt-in remote validators. Profiles live in ignored control-plane/.local.
//! SSH stages the validator and relays market feeds. An optional Tailscale
//! profile carries validator peer traffic directly between tailnet addresses.
use super::*;
use crate::types::RemoteChoice;
use serde::Deserialize;
use std::net::Ipv4Addr;
use std::path::Path;

#[derive(Clone, Deserialize)]
pub(super) struct RemoteProfile {
    pub id: String,
    pub label: String,
    pub ssh_alias: String,
    pub remote_base_dir: String,
    pub peer_port: u16,
    pub bootstrap_port: u16,
    pub feed_port_start: u16,
    #[serde(default)]
    pub tailnet_ip: Option<Ipv4Addr>,
}

#[derive(Deserialize)]
struct RemoteFile {
    profiles: Vec<RemoteProfile>,
}

pub(super) struct PlacementPlan {
    pub local_count: u64,
    pub hosts: BTreeMap<String, (RemoteProfile, Vec<NodeId>)>,
    pub nodes: BTreeMap<NodeId, (String, u16)>,
}

pub(super) fn placement_plan(project: &Path, config: &LaunchConfig) -> io::Result<PlacementPlan> {
    let mut requested = BTreeMap::new();
    let mut remote_total = 0u64;
    for placement in &config.remote_placements {
        if placement.count == 0
            || requested
                .insert(placement.profile_id.clone(), placement.count)
                .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote host counts must be positive and unique",
            ));
        }
        remote_total = remote_total.checked_add(placement.count).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "validator count overflow")
        })?;
    }
    if remote_total > 0 && config.node_count < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote voting validators require at least four total nodes",
        ));
    }
    let local_count = config
        .node_count
        .checked_sub(remote_total)
        .filter(|count| *count > 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one local validator is required",
            )
        })?;
    let mut hosts = BTreeMap::new();
    let mut nodes = BTreeMap::new();
    let mut ip_set = HashSet::new();
    let mut alias_set = HashSet::new();
    let mut next_id = local_count;
    for (id, count) in requested {
        let profile = selected_profile(project, &id)?;
        if !alias_set.insert(profile.ssh_alias.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a remote SSH host may appear only once",
            ));
        }
        if remote_total > 1 && profile.tailnet_ip.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "multiple remote validators require Tailscale on every remote host",
            ));
        }
        if let Some(ip) = profile.tailnet_ip {
            if !ip_set.insert(ip) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "duplicate remote Tailscale address",
                ));
            }
        }
        let last_port = u64::from(profile.peer_port)
            .checked_add(count - 1)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "peer port range overflow")
            })?;
        if last_port > u64::from(u16::MAX)
            || (u64::from(profile.peer_port)..=last_port)
                .contains(&u64::from(profile.bootstrap_port))
            || (u64::from(profile.peer_port)..=last_port).any(|port| {
                (u64::from(profile.feed_port_start)..u64::from(profile.feed_port_start) + 15)
                    .contains(&port)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "peer port range overlaps reserved ports on {}",
                    profile.label
                ),
            ));
        }
        let mut ids = Vec::new();
        for index in 0..count {
            let node_id = next_id + index;
            nodes.insert(
                node_id,
                (id.clone(), (u64::from(profile.peer_port) + index) as u16),
            );
            ids.push(node_id);
        }
        next_id += count;
        hosts.insert(id, (profile, ids));
    }
    Ok(PlacementPlan {
        local_count,
        hosts,
        nodes,
    })
}

#[derive(Clone)]
pub(super) struct RemoteRuntime {
    pub profile: RemoteProfile,
    pub run_dir: String,
    pub node_id: NodeId,
}

#[derive(Clone)]
pub(super) struct RemoteHostInfo {
    pub profile: RemoteProfile,
    pub run_dir: String,
    pub node_ids: Vec<NodeId>,
}

pub(super) struct RemoteHostRuntime {
    pub info: RemoteHostInfo,
    pub bootstrap_tunnel: Child,
    pub feed_tunnel: Child,
}

fn valid_alias(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b))
        && !value.starts_with('-')
}

fn valid_absolute_path(value: &str) -> bool {
    value.starts_with('/')
        && value.split('/').all(|part| part != "..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
}

pub(super) fn profiles(project: &Path) -> io::Result<Vec<RemoteProfile>> {
    let file = project.join("control-plane/.local/remotes.json");
    if !file.exists() {
        return Ok(Vec::new());
    }
    let parsed: RemoteFile = serde_json::from_slice(&std::fs::read(file)?)?;
    let mut ids = HashSet::new();
    for profile in &parsed.profiles {
        if !valid_alias(&profile.id)
            || !valid_alias(&profile.ssh_alias)
            || !valid_absolute_path(&profile.remote_base_dir)
            || !ids.insert(profile.id.clone())
            || profile.peer_port == 0
            || profile.bootstrap_port == 0
            || profile.feed_port_start == 0
            || profile.peer_port == profile.bootstrap_port
            || (profile.feed_port_start as u32 + 14) > u16::MAX as u32
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid remote profile",
            ));
        }
    }
    Ok(parsed.profiles)
}

pub fn available_profiles(project: &Path) -> io::Result<Vec<RemoteChoice>> {
    Ok(profiles(project)?
        .into_iter()
        .map(|p| RemoteChoice {
            id: p.id,
            label: p.label,
        })
        .collect())
}

pub(super) fn selected_profile(project: &Path, id: &str) -> io::Result<RemoteProfile> {
    profiles(project)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("remote profile {id:?} missing from control-plane/.local/remotes.json"),
            )
        })
}

fn ssh(profile: &RemoteProfile) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ControlMaster=no",
        "-S",
        "none",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=3",
        &profile.ssh_alias,
    ]);
    cmd
}

fn run_remote(profile: &RemoteProfile, script: &str) -> io::Result<String> {
    let output = ssh(profile).arg(script).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "SSH {}: {}",
            profile.label,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

fn scp_to(profile: &RemoteProfile, source: &Path, destination: &str) -> io::Result<()> {
    let output = Command::new("scp")
        .args(["-q", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
        .arg(source)
        .arg(format!("{}:{destination}", profile.ssh_alias))
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "SCP {}: {}",
            profile.label,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

pub(super) fn preflight(profile: &RemoteProfile) -> io::Result<String> {
    let script = "free -m | awk '/^Mem:/ {print $7}'; df -Pm / | awk 'NR==2 {print $4}'; uname -m";
    let output = run_remote(profile, &script)?;
    let lines: Vec<_> = output.lines().collect();
    if lines.len() != 3 {
        return Err(io::Error::other("remote resource probe failed"));
    }
    let memory = lines[0].parse::<u64>().map_err(io::Error::other)?;
    let disk = lines[1].parse::<u64>().map_err(io::Error::other)?;
    if memory < 300 || disk < 1024 {
        return Err(io::Error::other(format!(
            "remote headroom too low: {memory} MiB available RAM, {disk} MiB disk"
        )));
    }
    if lines[2] != std::env::consts::ARCH {
        return Err(io::Error::other(format!(
            "remote architecture {} differs from local {}",
            lines[2],
            std::env::consts::ARCH
        )));
    }
    if let Some(expected) = profile.tailnet_ip {
        let actual = run_remote(profile, "tailscale ip -4")?;
        if actual != expected.to_string() {
            return Err(io::Error::other(format!(
                "remote Tailscale IP mismatch: configured {expected}, host reports {actual}"
            )));
        }
    }
    Ok(format!(
        "{}: {memory} MiB available RAM, {disk} MiB free disk",
        profile.label
    ))
}

pub(super) fn local_tailnet_ip(profile: &RemoteProfile) -> io::Result<Option<Ipv4Addr>> {
    if profile.tailnet_ip.is_none() {
        return Ok(None);
    }
    let output = Command::new("tailscale").args(["ip", "-4"]).output()?;
    if !output.status.success() {
        return Err(io::Error::other("local Tailscale is not connected"));
    }
    let ip = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<Ipv4Addr>()
        .map_err(|_| io::Error::other("invalid local Tailscale IPv4 address"))?;
    Ok(Some(ip))
}

fn allocate_local_port() -> io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

pub(super) fn stage_host(
    profile: RemoteProfile,
    binary: &Path,
    data_dir: &Path,
    auth_key: &[u8; 32],
    node_ids: Vec<NodeId>,
    parent: SocketAddr,
    local_feeds: &[(u64, String, String)],
    local_peer_ports: &[u16],
) -> io::Result<RemoteHostRuntime> {
    let run_name = data_dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| io::Error::other("invalid session path"))?;
    if !valid_alias(run_name) {
        return Err(io::Error::other("invalid session name"));
    }
    let run_dir = format!(
        "{}/{run_name}",
        profile.remote_base_dir.trim_end_matches('/')
    );
    let dir = format!("{run_dir}/data");
    run_remote(
        &profile,
        &format!("umask 077; test ! -e {run_dir}; mkdir -p {dir}; chmod 700 {run_dir} {dir}"),
    )?;
    let auth_path = data_dir.join(format!("remote-auth-{}.key", profile.id));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    std::io::Write::write_all(
        &mut options.open(&auth_path)?,
        hex_encode(auth_key).as_bytes(),
    )?;
    let staging = (|| -> io::Result<()> {
        for (source, destination) in [
            (binary.to_path_buf(), format!("{run_dir}/node")),
            (
                data_dir.join("manifest.json"),
                format!("{dir}/manifest.json"),
            ),
            (auth_path.clone(), format!("{run_dir}/auth.key")),
        ] {
            scp_to(&profile, &source, &destination)?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(auth_path);
    if let Err(error) = staging {
        let _ = cleanup_host(&RemoteHostInfo {
            profile: profile.clone(),
            run_dir: run_dir.clone(),
            node_ids: node_ids.clone(),
        });
        return Err(error);
    }
    let mut bootstrap_command = ssh(&profile);
    bootstrap_command.args(["-o", "ExitOnForwardFailure=yes", "-N"]);
    bootstrap_command.arg(format!(
        "-R127.0.0.1:{}:127.0.0.1:{}",
        profile.bootstrap_port,
        parent.port()
    ));
    if profile.tailnet_ip.is_none() {
        for port in local_peer_ports {
            bootstrap_command.arg(format!("-R127.0.0.1:{port}:127.0.0.1:{port}"));
        }
    }
    let host_info = RemoteHostInfo {
        profile: profile.clone(),
        run_dir: run_dir.clone(),
        node_ids: node_ids.clone(),
    };
    let mut bootstrap_tunnel = match bootstrap_command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = cleanup_host(&host_info);
            return Err(error);
        }
    };
    let mut feed_command = ssh(&profile);
    feed_command.args(["-o", "ExitOnForwardFailure=yes", "-N"]);
    for (index, (_, _, url)) in local_feeds.iter().enumerate() {
        let port = url
            .rsplit(':')
            .next()
            .ok_or_else(|| io::Error::other("invalid local feed URL"))?
            .parse::<u16>()
            .map_err(io::Error::other)?;
        feed_command.arg(format!(
            "-R127.0.0.1:{}:127.0.0.1:{port}",
            profile.feed_port_start as usize + index
        ));
    }
    let mut feed_tunnel = match feed_command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = bootstrap_tunnel.kill();
            let _ = bootstrap_tunnel.wait();
            let _ = cleanup_host(&host_info);
            return Err(error);
        }
    };
    thread::sleep(Duration::from_millis(250));
    if bootstrap_tunnel.try_wait()?.is_some() || feed_tunnel.try_wait()?.is_some() {
        let _ = bootstrap_tunnel.kill();
        let _ = bootstrap_tunnel.wait();
        let _ = feed_tunnel.kill();
        let _ = feed_tunnel.wait();
        let _ = cleanup_host(&host_info);
        return Err(io::Error::other(
            "remote bootstrap or feed tunnel exited during startup",
        ));
    }
    Ok(RemoteHostRuntime {
        info: RemoteHostInfo {
            profile,
            run_dir,
            node_ids,
        },
        bootstrap_tunnel,
        feed_tunnel,
    })
}

pub(super) fn stage_node(
    host: &RemoteHostInfo,
    binary_data_dir: &Path,
    node_id: NodeId,
    peer_port: u16,
    local_feeds: &[(u64, String, String)],
    config: &LaunchConfig,
) -> io::Result<(RemoteRuntime, Child, SocketAddr)> {
    let profile = &host.profile;
    let run_dir = &host.run_dir;
    let dir = format!("{run_dir}/data");
    let remote = RemoteRuntime {
        profile: profile.clone(),
        run_dir: run_dir.clone(),
        node_id,
    };
    scp_to(
        profile,
        &binary_data_dir
            .join("keys")
            .join(format!("validator-{node_id}.key")),
        &format!("{dir}/validator-{node_id}.key"),
    )?;
    let local_port = if profile.tailnet_ip.is_none() {
        Some(allocate_local_port()?)
    } else {
        None
    };
    let mut ssh_command = ssh(profile);
    ssh_command.args(["-o", "ExitOnForwardFailure=yes"]);
    if let Some(port) = local_port {
        ssh_command.arg(format!("-L127.0.0.1:{port}:127.0.0.1:{peer_port}"));
    }
    let remote_peer_address = profile
        .tailnet_ip
        .map(|ip| SocketAddr::from((ip, peer_port)))
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], local_port.unwrap())));
    let remote_listen_address = profile
        .tailnet_ip
        .map(|ip| SocketAddr::from((ip, peer_port)))
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], peer_port)));
    let mut args = vec![
        "--node".to_string(),
        node_id.to_string(),
        "--parent".into(),
        format!("127.0.0.1:{}", profile.bootstrap_port),
        "--listen".into(),
        remote_listen_address.to_string(),
        "--advertise".into(),
        remote_peer_address.to_string(),
        "--data-dir".into(),
        dir.clone(),
        "--chain-config".into(),
        format!("{dir}/manifest.json"),
        "--validator-key".into(),
        format!("{dir}/validator-{node_id}.key"),
        "--block-time-ms".into(),
        config.block_time_ms.to_string(),
        "--tx-batch-window-ms".into(),
        config.tx_batch_window_ms.to_string(),
        "--timeout-ms".into(),
        config.timeout_ms.to_string(),
        "--sync-timeout-ms".into(),
        "10000".into(),
        "--primary-probe-timeout-ms".into(),
        "1500".into(),
        "--consensus".into(),
        config.consensus.as_str().into(),
    ];
    for (index, (base, venue, _)) in local_feeds.iter().enumerate() {
        let url = format!(
            "ws://127.0.0.1:{}",
            profile.feed_port_start as usize + index
        );
        if *base == light_dex::exchange::market::BTC {
            let flag = match venue.as_str() {
                "binance" => "--binance-feed-url",
                "okx" => "--okx-feed-url",
                "coinbase" => "--coinbase-feed-url",
                _ => continue,
            };
            args.extend([flag.into(), url]);
        } else {
            args.extend([
                "--oracle-feed".into(),
                format!("{base}:{}:{venue}:{url}", light_dex::exchange::market::USD),
            ]);
        }
    }
    // The remote wrapper persists its own PID before exec. Only session-scoped
    // files are copied; source code and account private keys never leave here.
    let trace_env = if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some() {
        "LIGHT_DEX_HANDOFF_TRACE=1 "
    } else {
        ""
    };
    let fast_collector_env = if std::env::var_os("LIGHT_DEX_FAST_LEADER_COLLECTOR").is_some() {
        "LIGHT_DEX_FAST_LEADER_COLLECTOR=1 "
    } else {
        ""
    };
    let fast_cert_handoff_env = if std::env::var_os("LIGHT_DEX_FAST_CERT_HANDOFF").is_some() {
        "LIGHT_DEX_FAST_CERT_HANDOFF=1 "
    } else {
        ""
    };
    let profile_env = if std::env::var("LIGHT_DEX_PROFILE_STAGES").as_deref() == Ok("1") {
        "LIGHT_DEX_PROFILE_STAGES=1 "
    } else {
        ""
    };
    let compression_env = if std::env::var_os("LIGHT_DEX_DISABLE_CONSENSUS_COMPRESSION").is_some() {
        "LIGHT_DEX_DISABLE_CONSENSUS_COMPRESSION=1 "
    } else {
        ""
    };
    let batch_env = if std::env::var_os("LIGHT_DEX_DISABLE_TX_BATCH_FORWARD").is_some() {
        "LIGHT_DEX_DISABLE_TX_BATCH_FORWARD=1 "
    } else {
        ""
    };
    let compact_env = if std::env::var_os("LIGHT_DEX_COMPACT_PROPOSALS").is_some() {
        "LIGHT_DEX_COMPACT_PROPOSALS=1 "
    } else {
        ""
    };
    let ready_env = std::env::var("LIGHT_DEX_PROPOSAL_READY_TXS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| format!("LIGHT_DEX_PROPOSAL_READY_TXS={} ", value.clamp(32, 512)))
        .unwrap_or_default();
    let verify_workers_env = std::env::var("LIGHT_DEX_VERIFY_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| format!("LIGHT_DEX_VERIFY_WORKERS={} ", value.clamp(1, 8)))
        .unwrap_or_default();
    let script = format!(
        "cd {run_dir} && umask 077 && printf '%s\\n' $$ > validator-{node_id}.pid && {trace_env}{fast_collector_env}{fast_cert_handoff_env}{profile_env}{compression_env}{batch_env}{compact_env}{ready_env}{verify_workers_env}LIGHT_DEX_AUTH_KEY=\"$(cat auth.key)\" exec ./node {}",
        args.join(" ")
    );
    let child = match ssh_command
        .arg(format!("sh -c '{}'", script.replace('\'', "'\\''")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return Err(error),
    };
    Ok((remote, child, remote_peer_address))
}

pub(super) fn terminate(remote: &RemoteRuntime) -> io::Result<()> {
    let script = format!(
        "p=$(cat {}/validator-{}.pid 2>/dev/null) || exit 0; case $p in *[!0-9]*|'') exit 1;; esac; if [ \"$(readlink -f /proc/$p/exe)\" = '{}/node' ]; then kill -TERM $p; i=0; while [ $i -lt 20 ] && [ \"$(readlink -f /proc/$p/exe)\" = '{}/node' ]; do sleep 0.5; i=$((i+1)); done; [ \"$(readlink -f /proc/$p/exe)\" != '{}/node' ] || exit 1; fi",
        remote.run_dir, remote.node_id, remote.run_dir, remote.run_dir, remote.run_dir
    );
    run_remote(&remote.profile, &script).map(|_| ())
}

pub(super) fn retrieve_journal(remote: &RemoteRuntime, local_dir: &Path) -> io::Result<()> {
    let name = format!("node-{}.json.journal", remote.node_id);
    let destination = local_dir.join(&name);
    let temporary = local_dir.join(format!("{name}.partial-{:016x}", rand::random::<u64>()));
    let remote_path = format!("{}/data/{name}", remote.run_dir);
    let remote_compressed = format!("{remote_path}.zst");
    let compressed = temporary.with_extension("partial.zst");
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    // Use an explicit zstd archive when both hosts provide it. OpenSSH's
    // streaming -C does not always keep up with multi-minute JSON journals.
    // Fall back to the original transfer for other public remote profiles.
    let use_zstd = Command::new("zstd").arg("--version").output().is_ok()
        && run_remote(
            &remote.profile,
            &format!(
                "command -v zstd >/dev/null && zstd -q -1 -f -o {} {}",
                quote(&remote_compressed),
                quote(&remote_path)
            ),
        )
        .is_ok();
    // Journals are JSON and compress well. Bound archival so a stalled remote
    // cannot leave a cleanup worker and its SSH connection alive indefinitely.
    let mut child = Command::new("scp")
        .args([
            "-q",
            "-C",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
        ])
        .arg(format!(
            "{}:{}",
            remote.profile.ssh_alias,
            if use_zstd {
                &remote_compressed
            } else {
                &remote_path
            }
        ))
        .arg(if use_zstd { &compressed } else { &temporary })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if let Some(status) = child.try_wait()? {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                pipe.read_to_string(&mut stderr)?;
            }
            if use_zstd {
                let _ = run_remote(
                    &remote.profile,
                    &format!("rm -f -- {}", quote(&remote_compressed)),
                );
            }
            if status.success() {
                if use_zstd {
                    let output = Command::new("zstd")
                        .args(["-q", "-d", "-f", "-o"])
                        .arg(&temporary)
                        .arg(&compressed)
                        .output()?;
                    let _ = std::fs::remove_file(&compressed);
                    if !output.status.success() {
                        return Err(io::Error::other(format!(
                            "journal decompression failed: {}",
                            String::from_utf8_lossy(&output.stderr)
                        )));
                    }
                }
                return std::fs::rename(&temporary, &destination);
            }
            let _ = std::fs::remove_file(&temporary);
            let _ = std::fs::remove_file(&compressed);
            return Err(io::Error::other(format!(
                "remote journal retrieval failed: {stderr}"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&temporary);
            let _ = std::fs::remove_file(&compressed);
            if use_zstd {
                let _ = run_remote(
                    &remote.profile,
                    &format!("rm -f -- {}", quote(&remote_compressed)),
                );
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "remote journal retrieval timed out after 300 seconds",
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Run the same signed-journal replay on the remote host and retain its small
/// digest summary locally. Opt-in for long experiments where journal transfer
/// would dominate the benchmark's cleanup phase.
pub(super) fn retrieve_journal_audit(remote: &RemoteRuntime, local_dir: &Path) -> io::Result<()> {
    let dir = &remote.run_dir;
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let script = format!(
        "cd {} && ./node --audit-journal --node {} --data-dir {} --chain-config {}",
        quote(dir),
        remote.node_id,
        quote(&format!("{dir}/data")),
        quote(&format!("{dir}/data/manifest.json")),
    );
    let output = run_remote(&remote.profile, &script)?;
    let audit = output
        .strip_prefix("JOURNAL_AUDIT ")
        .ok_or_else(|| io::Error::other("remote node did not return a journal audit"))?;
    let parsed: serde_json::Value = serde_json::from_str(audit).map_err(io::Error::other)?;
    if parsed["node_id"].as_u64() != Some(remote.node_id)
        || !parsed["block_digests"].is_array()
        || !parsed["state_root"].is_string()
    {
        return Err(io::Error::other("invalid remote journal audit summary"));
    }
    std::fs::write(
        local_dir.join(format!("node-{}.audit.json", remote.node_id)),
        audit,
    )
}

pub(super) fn cleanup_host(host: &RemoteHostInfo) -> io::Result<()> {
    // This path is generated locally and validated above. Never touch the
    // profile's base directory or any unrelated remote process.
    run_remote(&host.profile, &format!("rm -rf -- {}", host.run_dir)).map(|_| ())
}

pub(super) struct ResourceSample {
    pub rss_mib: u64,
    pub available_mib: u64,
    pub disk_free_mib: u64,
    pub host_cpu_percent: f64,
}

pub(super) fn sample_resources(host: &RemoteHostInfo) -> io::Result<ResourceSample> {
    let pids = host
        .node_ids
        .iter()
        .map(|id| format!("{}/validator-{id}.pid", host.run_dir))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "awk 'NR==1 {{t=0;for(i=2;i<=NF;i++)t+=$i;print $5,t}}' /proc/stat; sleep 1; awk 'NR==1 {{t=0;for(i=2;i<=NF;i++)t+=$i;print $5,t}}' /proc/stat; free -m | awk '/^Mem:/ {{print $7}}'; df -Pm / | awk 'NR==2 {{print $4}}'; for f in {pids}; do p=$(cat \"$f\" 2>/dev/null); if [ -n \"$p\" ]; then ps -o rss= -p \"$p\"; fi; done | awk '{{s+=$1}} END {{print int(s/1024)}}'"
    );
    let output = run_remote(&host.profile, &script)?;
    let lines: Vec<_> = output.lines().collect();
    if lines.len() != 5 {
        return Err(io::Error::other("invalid remote resource sample"));
    }
    let pair = |line: &str| -> io::Result<(u64, u64)> {
        let mut fields = line.split_whitespace();
        Ok((
            fields
                .next()
                .ok_or_else(|| io::Error::other("missing CPU idle"))?
                .parse()
                .map_err(io::Error::other)?,
            fields
                .next()
                .ok_or_else(|| io::Error::other("missing CPU total"))?
                .parse()
                .map_err(io::Error::other)?,
        ))
    };
    let (idle0, total0) = pair(lines[0])?;
    let (idle1, total1) = pair(lines[1])?;
    let total = total1.saturating_sub(total0).max(1);
    Ok(ResourceSample {
        rss_mib: lines[4].trim().parse().unwrap_or(0),
        available_mib: lines[2].trim().parse().map_err(io::Error::other)?,
        disk_free_mib: lines[3].trim().parse().map_err(io::Error::other)?,
        host_cpu_percent: 100.0 * (total.saturating_sub(idle1.saturating_sub(idle0))) as f64
            / total as f64,
    })
}

pub(super) fn spawn_resource_guard(shared: Shared, host: RemoteHostInfo, run_id: u64) {
    thread::spawn(move || {
        let mut warned_cpu = false;
        loop {
            thread::sleep(Duration::from_secs(9));
            let tunnel_exited = {
                let mut state = shared.lock().unwrap();
                if !state.running || state.run_id != run_id {
                    break;
                }
                state
                    .remote_hosts
                    .get_mut(&host.profile.id)
                    .is_some_and(|runtime| {
                        matches!(runtime.bootstrap_tunnel.try_wait(), Ok(Some(_)))
                            || matches!(runtime.feed_tunnel.try_wait(), Ok(Some(_)))
                    })
            };
            if tunnel_exited {
                log(
                    &shared,
                    format!("Remote tunnel exited on {}", host.profile.label),
                );
                stop_cluster(shared, false);
                break;
            }
            match sample_resources(&host) {
                Ok(sample) => {
                    if sample.host_cpu_percent > 80.0 && !warned_cpu {
                        log(
                            &shared,
                            format!(
                                "Remote host {} CPU at {:.0}%",
                                host.profile.label, sample.host_cpu_percent
                            ),
                        );
                        warned_cpu = true;
                    } else if sample.host_cpu_percent < 70.0 {
                        warned_cpu = false;
                    }
                    if sample.rss_mib > 500 * host.node_ids.len() as u64
                        || sample.available_mib < 300
                        || sample.disk_free_mib < 1024
                    {
                        log(
                            &shared,
                            format!(
                                "Remote resource guard stopped cluster on {}: validators {} MiB RSS, {} MiB available, {} MiB disk",
                                host.profile.label,
                                sample.rss_mib,
                                sample.available_mib,
                                sample.disk_free_mib
                            ),
                        );
                        stop_cluster(shared, false);
                        break;
                    }
                }
                Err(error) => {
                    log(&shared, format!("Remote resource probe failed: {error}"));
                }
            }
        }
    });
}

#[cfg(test)]
mod placement_tests {
    use super::*;
    use crate::types::RemotePlacement;

    #[test]
    fn distributes_remote_nodes_and_rejects_unsafe_placements() {
        let root =
            std::env::temp_dir().join(format!("light-dex-placement-{}", rand::random::<u64>()));
        let dir = root.join("control-plane/.local");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("remotes.json"), r#"{"profiles":[
            {"id":"tokyo","label":"Tokyo","ssh_alias":"tokyo-ls","remote_base_dir":"/tmp/light-dex",
             "peer_port":44000,"bootstrap_port":44050,"feed_port_start":45000,"tailnet_ip":"100.64.0.1"},
            {"id":"ideapad","label":"IdeaPad","ssh_alias":"ideapad-home","remote_base_dir":"/tmp/light-dex",
             "peer_port":44100,"bootstrap_port":44150,"feed_port_start":45100,"tailnet_ip":"100.64.0.2"}
        ]}"#).unwrap();
        let mut config = crate::default_config();
        config.node_count = 6;
        config.remote_placements = vec![
            RemotePlacement {
                profile_id: "tokyo".into(),
                count: 2,
            },
            RemotePlacement {
                profile_id: "ideapad".into(),
                count: 1,
            },
        ];
        let plan = placement_plan(&root, &config).unwrap();
        assert_eq!(plan.local_count, 3);
        assert_eq!(plan.nodes.get(&3), Some(&("ideapad".into(), 44100)));
        assert_eq!(plan.nodes.get(&4), Some(&("tokyo".into(), 44000)));
        assert_eq!(plan.nodes.get(&5), Some(&("tokyo".into(), 44001)));
        config.node_count = 3;
        assert!(placement_plan(&root, &config).is_err());
        config.node_count = 60;
        config.remote_placements[1].count = 51;
        assert!(placement_plan(&root, &config).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
