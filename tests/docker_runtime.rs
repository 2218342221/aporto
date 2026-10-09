//! Real Docker integration, never a runtime fixture. Run explicitly with:
//! APORTO_DOCKER_TESTS=1 cargo test --test docker_runtime -- --ignored
use aporto::{
    runtime::{DockerConfig, DockerRuntime},
    types::{ExecOptions, ProcessEvent, Runtime},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap, os::unix::fs::PermissionsExt, process::Command, sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const IMAGE: &str = "python:3.12-slim";
struct Cleanup {
    id: String,
    owner: String,
    image: String,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let Ok(output) = Command::new("docker")
            .args([
                "--host",
                "unix:///var/run/docker.sock",
                "container",
                "inspect",
                "--format",
                "{{json .Config.Labels}}",
                &self.id,
            ])
            .output()
        else {
            return;
        };
        let Ok(labels) = serde_json::from_slice::<Value>(&output.stdout) else {
            return;
        };
        if labels["io.aporto.managed"] == "true"
            && labels["io.aporto.owner"] == self.owner
            && labels["io.aporto.image-ref"] == self.image
        {
            let _ = Command::new("docker")
                .args([
                    "--host",
                    "unix:///var/run/docker.sock",
                    "container",
                    "rm",
                    "--force",
                    "--volumes",
                    &self.id,
                ])
                .output();
        }
    }
}
struct ImageCleanup {
    tag: String,
    owner: String,
}
impl Drop for ImageCleanup {
    fn drop(&mut self) {
        let Ok(output) = Command::new("docker")
            .args([
                "--host",
                "unix:///var/run/docker.sock",
                "image",
                "inspect",
                "--format",
                "{{json .Config.Labels}}",
                &self.tag,
            ])
            .output()
        else {
            return;
        };
        let Ok(labels) = serde_json::from_slice::<Value>(&output.stdout) else {
            return;
        };
        if labels["io.aporto.test-image-owner"] == self.owner {
            let _ = Command::new("docker")
                .args([
                    "--host",
                    "unix:///var/run/docker.sock",
                    "image",
                    "rm",
                    &self.tag,
                ])
                .output();
        }
    }
}
fn derived_image(runtime: &DockerRuntime, changes: &[&str]) -> ImageCleanup {
    let owner = uuid::Uuid::new_v4().to_string();
    let cleanup = ImageCleanup {
        tag: format!("aporto-test-derived:{owner}"),
        owner: owner.clone(),
    };
    let label = format!("LABEL io.aporto.test-image-owner={owner}");
    let mut args = vec!["container", "commit", "--change", &label];
    for change in changes {
        args.extend(["--change", change]);
    }
    args.extend([runtime.id(), &cleanup.tag]);
    docker(&args);
    cleanup
}
fn opt_in() {
    assert_eq!(
        std::env::var("APORTO_DOCKER_TESTS").as_deref(),
        Ok("1"),
        "explicit real Docker opt-in is required"
    );
}
async fn runtime() -> (Arc<DockerRuntime>, DockerConfig, Cleanup) {
    opt_in();
    let config = DockerConfig::new(IMAGE, format!("aporto-test-{}", uuid::Uuid::new_v4()));
    let runtime = DockerRuntime::open(config.clone(), None).await.unwrap();
    let cleanup = Cleanup {
        id: runtime.id().into(),
        owner: config.owner.clone(),
        image: config.image.clone(),
    };
    (runtime, config, cleanup)
}
fn docker(args: &[&str]) -> Value {
    let output = Command::new("docker")
        .args(["--host", "unix:///var/run/docker.sock"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "Docker test control failed");
    serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
}
async fn gone(runtime: &DockerRuntime, pid: u32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let output = runtime
                .exec(&format!("test ! -e /proc/{pid}"), ExecOptions::default())
                .await
                .unwrap();
            if output.exit_code == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("guest process survived cleanup");
}

#[test]
fn docker_config_rejects_remote_daemons_and_host_networks() {
    let defaults = DockerConfig::new(IMAGE, "test-owner");
    defaults.validate().unwrap();
    assert_eq!(defaults.host, "unix:///var/run/docker.sock");
    for host in [
        "tcp://localhost:2375",
        "ssh://localhost",
        "unix://remote/run/docker.sock",
        "unix:///tmp/docker.sock?key=x",
    ] {
        let mut invalid = defaults.clone();
        invalid.host = host.into();
        assert!(invalid.validate().is_err());
    }
    for network in ["host", "container:another"] {
        let mut invalid = defaults.clone();
        invalid.network = network.into();
        assert!(invalid.validate().is_err());
    }
    let mut invalid = defaults;
    invalid.memory_mb = 0;
    assert!(invalid.validate().is_err());
}

#[tokio::test]
#[ignore = "requires local Docker and a preloaded python:3.12-slim image"]
async fn binary_files_exec_cwd_environment_and_limits() {
    let (runtime, _, _cleanup) = runtime().await;
    let bytes = (0..=255u8).cycle().take(200_000).collect::<Vec<_>>();
    runtime
        .write_file("/workspace/binary.bin", &bytes)
        .await
        .unwrap();
    assert_eq!(
        runtime.read_file("/workspace/binary.bin").await.unwrap(),
        bytes
    );
    runtime.write_file("/workspace/empty", &[]).await.unwrap();
    assert!(
        runtime
            .read_file("/workspace/empty")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        runtime
            .write_file("/workspace/large", &vec![0; 32 * 1024 * 1024 + 1])
            .await
            .is_err()
    );
    runtime
        .exec("mkdir -p '/workspace/a directory'", ExecOptions::default())
        .await
        .unwrap();
    let output = runtime
        .exec(
            "printf '%s\\n' \"$ONLY_GUEST\"; pwd; printf err >&2; exit 7",
            ExecOptions {
                cwd: Some("/workspace/a directory".into()),
                env: BTreeMap::from([(
                    "ONLY_GUEST".into(),
                    "literal $HOME ; no host expansion".into(),
                )]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(output.exit_code, 7);
    assert_eq!(
        output.stdout,
        "literal $HOME ; no host expansion\n/workspace/a directory\n"
    );
    assert_eq!(output.stderr, "err");
    let overflow = runtime
        .exec(
            "python3 -c 'import sys; sys.stdout.write(\"x\"*9000000)'",
            ExecOptions::default(),
        )
        .await;
    assert!(overflow.is_err());
    runtime.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires local Docker and a preloaded python:3.12-slim image"]
async fn persistent_stdio_and_model_values_are_absent_from_host_docker_argv() {
    let (runtime, _, _cleanup) = runtime().await;
    let source = "import json,os,sys\nsys.stderr.write('ready\\n');sys.stderr.flush()\nfor line in sys.stdin:\n value=json.loads(line);print(json.dumps({'echo':value,'secret':os.environ['ONLY_GUEST']}),flush=True)\n";
    let marker = format!("guest-only-{}", uuid::Uuid::new_v4());
    let mut process = runtime
        .start_process(
            &[
                "python3".into(),
                "-u".into(),
                "-c".into(),
                source.into(),
                marker.clone(),
            ],
            ExecOptions {
                env: BTreeMap::from([("ONLY_GUEST".into(), marker.clone())]),
                timeout_ms: Some(10_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(process.id() > 1);
    // Read only argv of Docker clients addressing this test's exact container.
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let command = String::from_utf8_lossy(&bytes);
        if command.contains("docker") && command.contains(runtime.id()) {
            assert!(!command.contains(&marker));
            assert!(!command.contains(source));
        }
    }
    let mut stderr = Vec::new();
    for index in 0..2 {
        process
            .send(format!("{{\"index\":{index}}}\n").as_bytes())
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(3), async {
            let mut bytes = Vec::new();
            loop {
                match process.next().await.unwrap().unwrap() {
                    ProcessEvent::Stdout(data) => {
                        bytes.extend(data);
                        if bytes.contains(&b'\n') {
                            break serde_json::from_slice::<Value>(&bytes).unwrap();
                        }
                    }
                    ProcessEvent::Stderr(data) => stderr.extend(data),
                    ProcessEvent::Exit(code) => panic!("stdio process exited early: {code}"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(response, json!({"echo":{"index":index},"secret":marker}));
    }
    assert_eq!(stderr, b"ready\n");
    let pid = process.id();
    process.close().await.unwrap();
    gone(&runtime, pid).await;
    runtime.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires local Docker and a preloaded python:3.12-slim image"]
async fn cancellation_drop_and_closed_output_timeouts_kill_guest_process_groups() {
    let (runtime, _, _cleanup) = runtime().await;
    let cancel = CancellationToken::new();
    let task_runtime = runtime.clone();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        task_runtime
            .exec(
                "sleep 300 & echo $! > /workspace/background.pid; wait",
                ExecOptions {
                    cancel: task_cancel,
                    ..Default::default()
                },
            )
            .await
    });
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = runtime.read_file("/workspace/background.pid").await {
                break String::from_utf8(bytes)
                    .unwrap()
                    .trim()
                    .parse::<u32>()
                    .unwrap();
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    gone(&runtime, pid).await;
    let result = runtime
        .exec(
            "exec 1>&- 2>&-; sleep 300",
            ExecOptions {
                timeout_ms: Some(100),
                ..Default::default()
            },
        )
        .await;
    assert!(result.is_err());
    let process = runtime
        .start_process(
            &["/bin/sh".into(), "-c".into(), "sleep 300".into()],
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let pid = process.id();
    drop(process);
    gone(&runtime, pid).await;
    runtime.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires local Docker and a preloaded python:3.12-slim image"]
async fn ownership_resource_limits_stop_resume_running_recovery_and_delete() {
    let (runtime, config, _cleanup) = runtime().await;
    let id = runtime.id().to_owned();
    let inspection = docker(&["container", "inspect", "--format", "{{json .}}", &id]);
    assert_eq!(inspection["Config"]["Labels"]["io.aporto.managed"], "true");
    assert_eq!(
        inspection["Config"]["Labels"]["io.aporto.owner"],
        config.owner
    );
    assert_eq!(inspection["HostConfig"]["NetworkMode"], "none");
    assert_eq!(inspection["HostConfig"]["Privileged"], false);
    assert_eq!(inspection["HostConfig"]["Binds"], Value::Null);
    assert_eq!(inspection["Mounts"], json!([]));
    assert_eq!(inspection["HostConfig"]["Memory"], 512u64 * 1024 * 1024);
    assert_eq!(inspection["HostConfig"]["NanoCpus"], 2_000_000_000u64);
    assert_eq!(inspection["HostConfig"]["PidsLimit"], 128);
    assert!(
        inspection["HostConfig"]["CapDrop"]
            .as_array()
            .unwrap()
            .contains(&json!("ALL"))
    );
    assert!(
        inspection["HostConfig"]["SecurityOpt"]
            .as_array()
            .unwrap()
            .contains(&json!("no-new-privileges:true"))
    );
    let mut wrong = config.clone();
    wrong.owner = "not-the-owner".into();
    assert!(DockerRuntime::open(wrong, Some(&id)).await.is_err());
    assert_eq!(
        docker(&[
            "container",
            "inspect",
            "--format",
            "{{json .State.Running}}",
            &id
        ]),
        true
    );
    runtime
        .write_file("/workspace/persist", b"same workspace")
        .await
        .unwrap();
    runtime.pause().await.unwrap();
    assert_eq!(
        docker(&[
            "container",
            "inspect",
            "--format",
            "{{json .State.Status}}",
            &id
        ]),
        "exited"
    );
    drop(runtime);
    // Simulate a previous Core that crashed while this owned container was running.
    docker(&["container", "start", &id]);
    let marker = format!("crash-leftover-{}", uuid::Uuid::new_v4());
    docker(&[
        "container",
        "exec",
        "-d",
        &id,
        "python3",
        "-c",
        "import time; time.sleep(300)",
        &marker,
    ]);
    let resumed = DockerRuntime::open(config, Some(&id)).await.unwrap();
    assert_eq!(resumed.id(), id);
    assert_eq!(
        resumed.read_file("/workspace/persist").await.unwrap(),
        b"same workspace"
    );
    let code = format!(
        "python3 -c 'import os,pathlib,sys; marker=sys.argv[1].encode(); print(sum(marker in p.read_bytes().split(bytes([0])) for p in pathlib.Path(\"/proc\").glob(\"[0-9]*/cmdline\") if p.parent.name!=str(os.getpid())))' '{marker}'"
    );
    assert_eq!(
        resumed
            .exec(&code, ExecOptions::default())
            .await
            .unwrap()
            .stdout
            .trim(),
        "0"
    );
    resumed.close().await.unwrap();
    resumed.close().await.unwrap();
    let missing = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "container",
            "inspect",
            &id,
        ])
        .output()
        .unwrap();
    assert!(!missing.status.success());
}

#[tokio::test]
#[ignore = "requires local Docker and a preloaded python:3.12-slim image"]
async fn images_with_declared_volumes_are_rejected_before_container_creation() {
    let (base, _, _base_cleanup) = runtime().await;
    let image = derived_image(&base, &["VOLUME /declared"]);
    let config = DockerConfig::new(
        &image.tag,
        format!("aporto-test-rejected-{}", uuid::Uuid::new_v4()),
    );
    let error = match DockerRuntime::open(config.clone(), None).await {
        Ok(unexpected) => {
            let cleanup = Cleanup {
                id: unexpected.id().into(),
                owner: config.owner.clone(),
                image: config.image.clone(),
            };
            // Remove the test-owned container and its anonymous volume even if
            // this regression reappears; production must never create either.
            drop(cleanup);
            panic!("image declaring VOLUME unexpectedly opened");
        }
        Err(error) => error,
    };
    assert!(error.to_string().contains("declaring VOLUME"));
    let output = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "container",
            "ls",
            "--all",
            "--filter",
            &format!("label=io.aporto.owner={}", config.owner),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "rejected image created a container"
    );
    base.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires local Docker and a preloaded python:3.12-slim image"]
async fn startup_waits_for_this_boot_before_clearing_process_registry() {
    let (base, _, _base_cleanup) = runtime().await;
    base.exec("mkdir -p /opt/agent/bin", ExecOptions::default())
        .await
        .unwrap();
    base.write_file(
        "/opt/agent/bin/python3",
        b"#!/bin/sh\ncase \"$4\" in *'Aporto container boot'*) sleep 1;; esac\nexec /usr/local/bin/python3 \"$@\"\n",
    )
    .await
    .unwrap();
    base.exec("chmod +x /opt/agent/bin/python3", ExecOptions::default())
        .await
        .unwrap();
    // The committed image contains an old readiness marker. The wrapper delays
    // only BOOT, allowing the independent readiness probe to run immediately.
    let image = derived_image(
        &base,
        &["ENV PATH=/opt/agent/bin:/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin"],
    );
    let config = DockerConfig::new(
        &image.tag,
        format!("aporto-test-ready-{}", uuid::Uuid::new_v4()),
    );
    let mut current = DockerRuntime::open(config.clone(), None).await.unwrap();
    let _cleanup = Cleanup {
        id: current.id().into(),
        owner: config.owner.clone(),
        image: config.image.clone(),
    };
    let id = current.id().to_owned();
    for index in 0..2 {
        if index == 1 {
            current = DockerRuntime::open(config.clone(), Some(&id))
                .await
                .unwrap();
        }
        let mut process = current
            .start_process(
                &["/bin/sh".into(), "-c".into(), "sleep 300".into()],
                ExecOptions::default(),
            )
            .await
            .unwrap();
        // A late BOOT would erase this process's record after open returned.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let pid = process.id();
        let record = current
            .exec(
                &format!(
                    "python3 -I -c 'import json,pathlib; print(sum(json.loads(p.read_text())[\"pid\"] == {pid} for p in pathlib.Path(\"/tmp/.aporto-processes\").glob(\"*.json\")))'"
                ),
                ExecOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            record.stdout.trim(),
            "1",
            "BOOT erased a live process record"
        );
        process.close().await.unwrap();
        gone(&current, pid).await;
        let marker = current.read_file("/tmp/.aporto-ready").await.unwrap();
        let identity = current
            .exec(
                "python3 -I -c 'import os,pathlib; print(pathlib.Path(\"/proc/1/stat\").read_text().rsplit(\")\",1)[1].split()[19]+\":\"+os.readlink(\"/proc/1/ns/pid\"),end=\"\")'",
                ExecOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(marker, identity.stdout.as_bytes());
        current.pause().await.unwrap();
    }
    current.close().await.unwrap();
    base.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires local Docker and a preloaded python:3.12-slim image"]
async fn normal_exit_close_and_drop_do_not_request_guest_cancellation() {
    opt_in();
    let temporary = tempfile::tempdir().unwrap();
    let wrapper = temporary.path().join("docker");
    let reject_inspect = temporary.path().join("reject-inspect");
    let attempted = temporary.path().join("unexpected-cleanup");
    let source = format!(
        "#!/usr/bin/python3\nimport os,pathlib,shutil,sys\nif sys.argv[3:5]==['container','inspect'] and pathlib.Path({gate}).exists():\n pathlib.Path({attempted}).write_text('unexpected cancellation')\n sys.exit(1)\nos.execv(shutil.which('docker'),['docker']+sys.argv[1:])\n",
        gate = serde_json::to_string(&reject_inspect).unwrap(),
        attempted = serde_json::to_string(&attempted).unwrap(),
    );
    std::fs::write(&wrapper, source).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = DockerConfig::new(IMAGE, format!("aporto-test-exit-{}", uuid::Uuid::new_v4()));
    config.binary = wrapper;
    let runtime = DockerRuntime::open(config.clone(), None).await.unwrap();
    let _cleanup = Cleanup {
        id: runtime.id().into(),
        owner: config.owner.clone(),
        image: config.image.clone(),
    };
    std::fs::write(&reject_inspect, b"1").unwrap();
    for index in 0..12 {
        let mut process = runtime
            .start_process(
                &["/bin/sh".into(), "-c".into(), "exit 0".into()],
                ExecOptions::default(),
            )
            .await
            .unwrap();
        loop {
            if let Some(ProcessEvent::Exit(code)) = process.next().await.unwrap() {
                assert_eq!(code, 0);
                break;
            }
        }
        if index % 2 == 0 {
            process.close().await.unwrap();
        }
        drop(process);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!attempted.exists(), "normal exit requested cancellation");
    std::fs::remove_file(reject_inspect).unwrap();
    runtime.close().await.unwrap();
}
