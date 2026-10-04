use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use assert_cmd::cargo::cargo_bin_cmd;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio_util::task::AbortOnDropHandle;

struct ServerProcess(std::process::Child);

#[test]
fn server_help_and_invalid_arguments_are_forwarded_to_the_server_executable() {
    for args in [vec!["--help"], vec!["--bind", "invalid"]] {
        let direct = cargo_bin_cmd!("gafctl-server")
            .args(&args)
            .output()
            .unwrap();
        let delegated = cargo_bin_cmd!("gafctl")
            .arg("server")
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(delegated.status.code(), direct.status.code());
        assert_eq!(delegated.stdout, direct.stdout);
        assert_eq!(delegated.stderr, direct.stderr);
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn both_server_entrypoints_serve_health_and_inventory_without_device_access() {
    for (binary, prefix) in [("gafctl", Some("server")), ("gafctl-server", None)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin(binary));
        command.args(prefix).args(["--bind", &address.to_string()]);
        for name in [
            "GAFCTL_DEVICE_ID",
            "GAFCTL_IDENTITY_STORE",
            "GAFCTL_MQTT_HOST",
            "GAFCTL_MQTT_USERNAME",
            "GAFCTL_MQTT_PASSWORD",
            "GAFCTL_QUICKCONNECT_USERNAME",
            "GAFCTL_QUICKCONNECT_PASSWORD",
            "GAFCTL_QUICKCONNECT_PASSWORD_FILE",
            "GAFCTL_QUICKCONNECT_WRITES_ENABLED",
        ] {
            command.env_remove(name);
        }
        let mut server =
            ServerProcess(command.stdout(std::process::Stdio::null()).spawn().unwrap());
        let client = reqwest::Client::new();
        let health = format!("http://{address}/health");
        let mut ready = false;
        for _ in 0..100 {
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "{binary} exited before listening"
            );
            if client
                .get(&health)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready, "{binary} did not serve health");
        let inventory: Value = client
            .get(format!("http://{address}/api/v2/devices"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(inventory, json!({"devices": []}));
    }
}

#[test]
fn help_and_completions_work_without_any_transport_configuration() {
    let service_help = cargo_bin_cmd!("gafctl-server")
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let service_help = String::from_utf8(service_help).unwrap();
    for command in ["completions", "probe", "--bind"] {
        assert!(service_help.contains(command), "missing {command}");
    }
    cargo_bin_cmd!("gafctl-server")
        .arg("devices")
        .assert()
        .code(2);

    let output = cargo_bin_cmd!("gafctl")
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(output).unwrap();
    for command in [
        "devices",
        "state",
        "control",
        "ble",
        "server",
        "completions",
    ] {
        assert!(help.contains(command), "missing {command}");
    }
    cargo_bin_cmd!("gafctl")
        .args(["ble", "--format", "json", "scan", "--help"])
        .assert()
        .success();
    cargo_bin_cmd!("gafctl")
        .args(["completions", "zsh"])
        .env("GAFCTL_SERVER_URL", "invalid")
        .env("GAFCTL_QUICKCONNECT_USERNAME", "incomplete")
        .assert()
        .success();
}

#[test]
fn invalid_input_and_missing_control_targets_exit_before_transport_access() {
    for args in [
        vec!["state", "../wrong"],
        vec!["devices", "--server", "http://user:secret@localhost"],
        vec![
            "control",
            "configured",
            "targets",
            "--temperature-f",
            "89",
            "--humidity-percent",
            "40",
        ],
        vec![
            "control",
            "configured",
            "targets",
            "--temperature-f",
            "110",
            "--humidity-percent",
            "80.1",
        ],
        vec!["control", "configured", "timer-duration", "31"],
        vec!["devices", "--timeout-seconds", "0"],
        vec!["ble", "control", "preset", "timer-clear"],
        vec![
            "control",
            "configured",
            "preset",
            "timer-clear",
            "--request-id",
            "bad/id",
        ],
    ] {
        cargo_bin_cmd!("gafctl")
            .args(args)
            .timeout(Duration::from_secs(5))
            .assert()
            .code(2);
    }
}

#[derive(Clone)]
struct Service {
    outcome: &'static str,
    status: StatusCode,
    posts: Arc<AtomicUsize>,
}

async fn devices() -> Json<Value> {
    Json(
        json!({"devices":[{"id":"configured","name":"Attic fan","backend":"legacy_ble",
        "proxy_id":"550e8400-e29b-41d4-a716-446655440000","capabilities":{"read_state":true,"commands":[{"kind":"legacy_preset","value":"timer_clear"}]},
        "state_source":"http","command_source":"http"}]}),
    )
}

async fn state() -> Json<Value> {
    Json(
        json!({"id":"configured","backend":"legacy_ble","available":false,
        "inventory_status":"unknown","last_error":null,"state":null}),
    )
}

async fn control(
    State(service): State<Service>,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    service.posts.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        request["command"],
        json!({"kind":"legacy_preset","preset":"timer_clear"})
    );
    assert!(request["issued_at_unix_ms"].as_u64().unwrap() > 0);
    (
        service.status,
        Json(json!({"request_id":request["request_id"],"status":service.outcome})),
    )
}

struct Running {
    url: String,
    _task: AbortOnDropHandle<()>,
}

async fn start(service: Service) -> Running {
    let app = Router::new()
        .route("/api/v2/devices", get(devices))
        .route("/api/v2/devices/configured/state", get(state))
        .route("/api/v2/devices/configured/control", post(control))
        .with_state(service);
    start_router(app).await
}

async fn start_router(app: Router) -> Running {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Running {
        url,
        _task: AbortOnDropHandle::new(task),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn service_reads_emit_one_json_result_and_explicit_url_overrides_environment() {
    let server = start(Service {
        outcome: "confirmed",
        status: StatusCode::OK,
        posts: Arc::default(),
    })
    .await;
    for args in [vec!["devices"], vec!["state", "configured"]] {
        let url = server.url.clone();
        let output = tokio::task::spawn_blocking(move || {
            cargo_bin_cmd!("gafctl")
                .args(args)
                .args(["--server", &url, "--format", "json"])
                .env("GAFCTL_SERVER_URL", "http://127.0.0.1:1")
                .env("RUST_LOG", "gafctl=debug")
                .timeout(Duration::from_secs(5))
                .assert()
                .success()
                .get_output()
                .stdout
                .clone()
        })
        .await
        .unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();
        assert!(value.get("devices").is_some() || value["available"] == false);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn service_controls_preserve_backend_results_and_exit_only_when_confirmed() {
    for (outcome, status, exit) in [
        ("confirmed", StatusCode::OK, 0),
        ("readback_mismatch", StatusCode::BAD_GATEWAY, 1),
    ] {
        let service = Service {
            outcome,
            status,
            posts: Arc::default(),
        };
        let server = start(service.clone()).await;
        let url = server.url.clone();
        let output = tokio::task::spawn_blocking(move || {
            cargo_bin_cmd!("gafctl")
                .args([
                    "control",
                    "configured",
                    "preset",
                    "timer-clear",
                    "--server",
                    &url,
                    "--format",
                    "json",
                    "--request-id",
                    "cli-test",
                ])
                .timeout(Duration::from_secs(5))
                .assert()
                .code(exit)
                .get_output()
                .stdout
                .clone()
        })
        .await
        .unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["request_id"], "cli-test");
        assert_eq!(value["status"], outcome);
        assert_eq!(service.posts.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn transport_failure_emits_a_json_error_with_a_nonzero_exit() {
    let output = cargo_bin_cmd!("gafctl")
        .args([
            "devices",
            "--server",
            "http://127.0.0.1:1",
            "--format",
            "json",
        ])
        .timeout(Duration::from_secs(5))
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["error"]["kind"], "transport");
}

#[test]
fn closed_stdout_pipe_is_a_successful_completion_exit() {
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("gafctl"))
        .args(["completions", "bash"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unavailable_state_retains_backend_error_in_text_and_json_without_failing() {
    let app = Router::new().route("/api/v2/devices", get(devices)).route(
        "/api/v2/devices/configured/state",
        get(|| async {
            Json(json!({
                "id":"configured", "backend":"legacy_ble", "available":false,
                "inventory_status":"unavailable", "state":null,
                "last_error":"backend read timed out"
            }))
        }),
    );
    let server = start_router(app).await;
    for format in ["text", "json"] {
        let url = server.url.clone();
        let output = tokio::task::spawn_blocking(move || {
            cargo_bin_cmd!("gafctl")
                .args(["state", "configured", "--server", &url, "--format", format])
                .timeout(Duration::from_secs(5))
                .assert()
                .success()
                .get_output()
                .clone()
        })
        .await
        .unwrap();
        assert!(output.stderr.is_empty());
        match format {
            "text" => {
                let text = String::from_utf8(output.stdout).unwrap();
                assert!(text.contains("Available: false"));
                assert!(text.contains("Last error: backend read timed out"));
            }
            "json" => {
                let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["last_error"], "backend read timed out");
                assert_eq!(value["available"], false);
                assert!(value["state"].is_null());
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn json_errors_keep_http_status_and_service_logs_use_stderr() {
    let app = Router::new().route(
        "/api/v2/devices",
        get(|| async { (StatusCode::BAD_GATEWAY, "upstream unavailable") }),
    );
    let Running { url, _task } = start_router(app).await;
    let output = tokio::task::spawn_blocking(move || {
        cargo_bin_cmd!("gafctl")
            .args(["devices", "--format", "json"])
            .env("GAFCTL_SERVER_URL", url)
            .env("RUST_LOG", "gafctl=debug")
            .timeout(Duration::from_secs(5))
            .assert()
            .code(1)
            .get_output()
            .clone()
    })
    .await
    .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["http_status"], 502);
    assert!(String::from_utf8_lossy(&output.stderr).contains("running control CLI command"));
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_inventory_from_environment_is_successful_and_contains_one_newline() {
    let app = Router::new().route(
        "/api/v2/devices",
        get(|| async { Json(json!({"devices":[]})) }),
    );
    let Running { url, _task } = start_router(app).await;
    let output = tokio::task::spawn_blocking(move || {
        cargo_bin_cmd!("gafctl")
            .args(["devices", "--format", "json"])
            .env("GAFCTL_SERVER_URL", url)
            .env("GAFCTL_QUICKCONNECT_USERNAME", "incomplete-account")
            .timeout(Duration::from_secs(5))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    })
    .await
    .unwrap();
    assert_eq!(output, b"{\"devices\":[]}\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn all_cloud_control_shapes_are_posted_through_the_service() {
    for (args, expected) in [
        (
            vec!["mode", "manual"],
            json!({"kind":"quick_connect_mode","mode":"manual"}),
        ),
        (
            vec![
                "targets",
                "--temperature-f",
                "105",
                "--humidity-percent",
                "40",
            ],
            json!({"kind":"quick_connect_targets","temperature_f":105,"humidity_percent":40}),
        ),
        (
            vec!["timer-duration", "60"],
            json!({"kind":"quick_connect_timer_duration","minutes":60}),
        ),
    ] {
        let app=Router::new().route("/api/v2/devices",get(|| async { Json(json!({"devices":[{
            "id":"qc-local","name":"cloud fan","backend":"quick_connect",
            "proxy_id":"550e8400-e29b-41d4-a716-446655440000","capabilities":{"read_state":true,"commands":[{"kind":"quick_connect_mode"},{"kind":"quick_connect_targets"},{"kind":"quick_connect_timer_duration"}]},
            "state_source":"mqtt","command_source":"mqtt"}]})) }))
            .route("/api/v2/devices/qc-local/control",post(move |Json(request):Json<Value>| { let expected=expected.clone(); async move {
                assert_eq!(request["command"],expected);
                assert_eq!(request["request_id"].as_str().unwrap().len(),32);
                Json(json!({"request_id":request["request_id"],"status":"confirmed"}))
            }}));
        let Running { url, _task } = start_router(app).await;
        let output = tokio::task::spawn_blocking(move || {
            cargo_bin_cmd!("gafctl")
                .args(["control", "qc-local"])
                .args(args)
                .args(["--server", &url, "--format", "json"])
                .timeout(Duration::from_secs(5))
                .assert()
                .success()
                .get_output()
                .stdout
                .clone()
        })
        .await
        .unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["status"], "confirmed");
        assert_eq!(value["http_status"], 200);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn timed_out_control_keeps_request_id_and_reports_unknown_outcome_without_retry() {
    let posts = Arc::new(AtomicUsize::new(0));
    let counter = posts.clone();
    let app = Router::new().route("/api/v2/devices", get(devices)).route(
        "/api/v2/devices/configured/control",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(2)).await;
                Json(json!({"request_id":"uncertain-cli","status":"confirmed"}))
            }
        }),
    );
    let Running { url, _task } = start_router(app).await;
    let output = tokio::task::spawn_blocking(move || {
        cargo_bin_cmd!("gafctl")
            .args([
                "control",
                "configured",
                "preset",
                "timer-clear",
                "--server",
                &url,
                "--format",
                "json",
                "--timeout-seconds",
                "1",
                "--request-id",
                "uncertain-cli",
            ])
            .timeout(Duration::from_secs(5))
            .assert()
            .code(1)
            .get_output()
            .stdout
            .clone()
    })
    .await
    .unwrap();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["error"]["kind"], "timeout");
    assert_eq!(value["error"]["request_id"], "uncertain-cli");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outcome unknown")
    );
    assert_eq!(posts.load(Ordering::SeqCst), 1);
}
