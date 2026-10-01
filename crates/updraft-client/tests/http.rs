use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use updraft_api::{ControlPreset, DeviceCommand, DeviceControlV2Request, DeviceId};
use updraft_client::{Client, ClientOptions, ServerUrl};

#[derive(Clone)]
struct Service {
    posts: Arc<AtomicUsize>,
    outcome: &'static str,
    status: StatusCode,
    supported: bool,
    mismatched_id: bool,
    read_supported: bool,
    refresh_outcome: &'static str,
    refresh_status: StatusCode,
}

impl Default for Service {
    fn default() -> Self {
        Self {
            posts: Arc::default(),
            outcome: "confirmed",
            status: StatusCode::OK,
            supported: true,
            mismatched_id: false,
            read_supported: true,
            refresh_outcome: "failed",
            refresh_status: StatusCode::BAD_GATEWAY,
        }
    }
}

async fn devices(State(service): State<Service>) -> Json<Value> {
    Json(
        json!({"devices":[{"id":"configured","name":"Attic fan","backend":"legacy_ble",
        "proxy_id":"550e8400-e29b-41d4-a716-446655440000","capabilities":{"read_state":service.read_supported,"commands":if service.supported { json!([{"kind":"legacy_preset","value":"timer_clear"}]) } else { json!([]) }},
        "state_source":"mqtt","command_source":"mqtt"}]}),
    )
}

async fn state(Path(id): Path<String>) -> Json<Value> {
    Json(
        json!({"id":id,"backend":"legacy_ble","available":false,"inventory_status":"unknown","last_error":null,"state":null}),
    )
}

async fn refresh(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    service.posts.fetch_add(1, Ordering::SeqCst);
    (
        service.refresh_status,
        Json(
            json!({"status":service.refresh_outcome, "id":if service.mismatched_id { "other" } else { &id },
        "backend":"legacy_ble","available":false,"inventory_status":"unavailable",
        "last_error":"read failed","state":null}),
        ),
    )
}

async fn control(
    State(service): State<Service>,
    Json(request): Json<DeviceControlV2Request>,
) -> (StatusCode, Json<Value>) {
    service.posts.fetch_add(1, Ordering::SeqCst);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    assert!(now.abs_diff(u128::from(request.issued_at_unix_ms)) < 1_000);
    assert_eq!(request.command, preset());
    let id = if service.mismatched_id {
        "some-other-request"
    } else {
        request.request_id.as_str()
    };
    (
        service.status,
        Json(json!({"request_id":id,"status":service.outcome})),
    )
}

fn preset() -> DeviceCommand {
    DeviceCommand::LegacyPreset {
        preset: ControlPreset::TimerClear,
    }
}
fn device_id() -> DeviceId {
    "configured".parse().unwrap()
}

struct Running {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start(service: Service) -> Running {
    let app = Router::new()
        .route("/prefix/api/v2/devices", get(devices))
        .route("/prefix/api/v2/devices/{id}/state", get(state))
        .route("/prefix/api/v2/devices/{id}/refresh", post(refresh))
        .route("/prefix/api/v2/devices/{id}/control", post(control))
        .with_state(service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/prefix", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Running { url, task }
}

#[test]
fn server_url_rejects_credentials_queries_fragments_and_non_http_schemes() {
    for invalid in [
        "ftp://localhost",
        "http://user:secret@localhost",
        "http://localhost?token=secret",
        "http://localhost/#fragment",
        "not a URL",
    ] {
        assert!(invalid.parse::<ServerUrl>().is_err(), "{invalid}");
    }
    assert_eq!(
        "http://localhost/prefix"
            .parse::<ServerUrl>()
            .unwrap()
            .as_url()
            .as_str(),
        "http://localhost/prefix/"
    );
}

#[tokio::test]
async fn client_preserves_path_prefix_and_reads_unavailable_state() {
    let server = start(Service::default()).await;
    let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
    let devices = client.devices().await.unwrap();
    assert_eq!(devices.devices.len(), 1);
    let state = client.state(&device_id()).await.unwrap();
    assert!(!state.available);
    assert!(state.state.is_none());
}

#[tokio::test]
async fn refresh_posts_once_and_preserves_failed_read_outcome() {
    let service = Service::default();
    let server = start(service.clone()).await;
    let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
    let response = client.refresh(&device_id()).await.unwrap();
    assert_eq!(response.status, updraft_api::DeviceRefreshStatus::Failed);
    assert_eq!(service.posts.load(Ordering::SeqCst), 1);
    assert_eq!(response.device.last_error.as_deref(), Some("read failed"));
}

#[tokio::test]
async fn refresh_validates_identity_outcome_and_read_capability_without_retrying() {
    for (service, expected_kind, posts) in [
        (
            Service {
                mismatched_id: true,
                ..Service::default()
            },
            "contract",
            1,
        ),
        (
            Service {
                refresh_outcome: "fresh",
                refresh_status: StatusCode::OK,
                ..Service::default()
            },
            "contract",
            1,
        ),
        (
            Service {
                refresh_outcome: "future_outcome",
                refresh_status: StatusCode::OK,
                ..Service::default()
            },
            "decoding",
            1,
        ),
        (
            Service {
                refresh_status: StatusCode::OK,
                ..Service::default()
            },
            "contract",
            1,
        ),
        (
            Service {
                read_supported: false,
                ..Service::default()
            },
            "unsupported_command",
            0,
        ),
    ] {
        let server = start(service.clone()).await;
        let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
        assert_eq!(
            client.refresh(&device_id()).await.unwrap_err().kind(),
            expected_kind
        );
        assert_eq!(service.posts.load(Ordering::SeqCst), posts);
    }
}

#[tokio::test]
async fn prepared_control_correlates_one_post_and_ignores_ha_entity_source_ownership() {
    let service = Service::default();
    let server = start(service.clone()).await;
    let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
    let result = client
        .prepare_control(&device_id(), preset())
        .await
        .unwrap()
        .submit("cli-request".parse().unwrap())
        .await
        .unwrap();
    assert!(result.is_confirmed());
    assert_eq!(result.response().request_id, "cli-request");
    assert_eq!(service.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unsupported_capability_prevents_any_post() {
    let service = Service {
        supported: false,
        ..Service::default()
    };
    let server = start(service.clone()).await;
    let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
    assert!(
        client
            .prepare_control(&device_id(), preset())
            .await
            .is_err()
    );
    assert_eq!(service.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn non_success_and_unknown_outcomes_are_preserved_without_retries() {
    for (status, outcome) in [
        (StatusCode::BAD_GATEWAY, "readback_mismatch"),
        (StatusCode::OK, "future_outcome"),
        (StatusCode::BAD_GATEWAY, "confirmed"),
    ] {
        let service = Service {
            status,
            outcome,
            ..Service::default()
        };
        let server = start(service.clone()).await;
        let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
        let result = client
            .prepare_control(&device_id(), preset())
            .await
            .unwrap()
            .submit("cli-request".parse().unwrap())
            .await
            .unwrap();
        assert!(!result.is_confirmed());
        assert_eq!(result.response().status.as_str(), outcome);
        assert_eq!(result.http_status(), status.as_u16());
        assert_eq!(service.posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn mismatched_correlation_reports_unknown_submission_with_its_request_id() {
    let service = Service {
        mismatched_id: true,
        ..Service::default()
    };
    let server = start(service.clone()).await;
    let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
    let error = client
        .prepare_control(&device_id(), preset())
        .await
        .unwrap()
        .submit("cli-request".parse().unwrap())
        .await
        .unwrap_err();
    assert_eq!(error.request_id().unwrap().as_str(), "cli-request");
    assert_eq!(error.kind(), "correlation");
    assert_eq!(service.posts.load(Ordering::SeqCst), 1);
}

#[test]
fn client_rejects_zero_timeouts() {
    let options = ClientOptions {
        read_timeout: Duration::ZERO,
        ..ClientOptions::default()
    };
    let result = Client::new("http://localhost".parse().unwrap(), options);
    assert_eq!(result.err().unwrap().kind(), "configuration");
}

#[tokio::test]
async fn malformed_control_response_preserves_http_status_and_request_id() {
    let app = Router::new()
        .route("/prefix/api/v2/devices", get(devices))
        .route(
            "/prefix/api/v2/devices/configured/control",
            post(|| async { (StatusCode::BAD_GATEWAY, "not JSON") }),
        )
        .with_state(Service::default());
    let server = start_router(app).await;
    let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
    let error = client
        .prepare_control(&device_id(), preset())
        .await
        .unwrap()
        .submit("unknown-outcome".parse().unwrap())
        .await
        .unwrap_err();
    assert_eq!(error.http_status(), Some(502));
    assert_eq!(error.request_id().unwrap().as_str(), "unknown-outcome");
    assert_eq!(error.kind(), "decoding");
}

async fn start_router(app: Router) -> Running {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/prefix", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Running { url, task }
}

#[tokio::test]
async fn redirects_are_never_followed_for_reads_or_controls() {
    let followed = Arc::new(AtomicUsize::new(0));
    let counter = followed.clone();
    let app = Router::new()
        .route(
            "/prefix/api/v2/devices",
            get(|| async { (StatusCode::FOUND, [("location", "/redirected")], "redirect") }),
        )
        .route(
            "/redirected",
            get(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"devices":[]}))
                }
            }),
        );
    let server = start_router(app).await;
    let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
    assert_eq!(client.devices().await.unwrap_err().http_status(), Some(302));
    assert_eq!(followed.load(Ordering::SeqCst), 0);

    let counter = followed.clone();
    let app = Router::new()
        .route("/prefix/api/v2/devices", get(devices))
        .route(
            "/prefix/api/v2/devices/configured/control",
            post(|| async {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [("location", "/redirected")],
                    "redirect",
                )
            }),
        )
        .route(
            "/redirected",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    "should not be called"
                }
            }),
        )
        .with_state(Service::default());
    let server = start_router(app).await;
    let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
    let error = client
        .prepare_control(&device_id(), preset())
        .await
        .unwrap()
        .submit("redirect".parse().unwrap())
        .await
        .unwrap_err();
    assert_eq!(error.http_status(), Some(307));
    assert_eq!(followed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn controls_are_not_retried_after_timeout_or_lost_response() {
    for timeout in [true, false] {
        let posts = Arc::new(AtomicUsize::new(0));
        let counter = posts.clone();
        let app = Router::new()
            .route("/prefix/api/v2/devices", get(devices))
            .route(
                "/prefix/api/v2/devices/configured/control",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if timeout {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        axum::body::Body::from_stream(futures_util::stream::iter([Err::<
                            bytes::Bytes,
                            _,
                        >(
                            std::io::Error::new(
                                std::io::ErrorKind::ConnectionReset,
                                "lost response",
                            ),
                        )]))
                    }
                }),
            )
            .with_state(Service::default());
        let server = start_router(app).await;
        let options = ClientOptions {
            control_timeout: Duration::from_millis(100),
            ..ClientOptions::default()
        };
        let client = Client::new(server.url.parse().unwrap(), options).unwrap();
        let error = client
            .prepare_control(&device_id(), preset())
            .await
            .unwrap()
            .submit("uncertain".parse().unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), if timeout { "timeout" } else { "transport" });
        assert_eq!(error.request_id().unwrap().as_str(), "uncertain");
        assert!(error.to_string().contains("outcome unknown"));
        assert_eq!(posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn response_limit_applies_to_declared_and_chunked_success_and_error_bodies() {
    for declared in [true, false] {
        for status in [StatusCode::OK, StatusCode::BAD_GATEWAY] {
            let app = Router::new().route(
                "/prefix/api/v2/devices",
                get(move || async move {
                    let mut response = axum::response::Response::new(if declared {
                        axum::body::Body::from_stream(futures_util::stream::pending::<
                            Result<bytes::Bytes, std::io::Error>,
                        >())
                    } else {
                        axum::body::Body::from_stream(futures_util::stream::iter((0..3).map(
                            |_| {
                                Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'x'; 1024 * 1024]))
                            },
                        )))
                    });
                    *response.status_mut() = status;
                    if declared {
                        response
                            .headers_mut()
                            .insert("content-length", "2097153".parse().unwrap());
                    }
                    response
                }),
            );
            let server = start_router(app).await;
            let client =
                Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
            let error = client.devices().await.unwrap_err();
            assert_eq!(error.kind(), "response_too_large");
            assert_eq!(error.http_status(), Some(status.as_u16()));
        }
    }
}

#[tokio::test]
async fn malformed_inventory_and_wrong_state_identity_are_rejected() {
    for inventory in [
        json!({"devices":[{"id":"../bad"}]}),
        json!({"devices":[
        {"id":"configured","name":"one","backend":"legacy_ble","proxy_id":"550e8400-e29b-41d4-a716-446655440000","capabilities":{"read_state":true,"commands":[]},"state_source":"http","command_source":"http"},
        {"id":"configured","name":"two","backend":"legacy_ble","proxy_id":"550e8400-e29b-41d4-a716-446655440000","capabilities":{"read_state":true,"commands":[]},"state_source":"http","command_source":"http"}]} ),
    ] {
        let app = Router::new().route(
            "/prefix/api/v2/devices",
            get(move || {
                let value = inventory.clone();
                async move { Json(value) }
            }),
        );
        let server = start_router(app).await;
        let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
        assert!(client.devices().await.is_err());
    }
    for (id, backend) in [("wrong", "legacy_ble"), ("configured", "quick_connect")] {
        let app=Router::new().route("/prefix/api/v2/devices",get(devices))
            .route("/prefix/api/v2/devices/configured/state",get(move || async move { Json(json!({
                "id":id,"backend":backend,"available":false,"inventory_status":"unknown","last_error":null,"state":null
            })) })).with_state(Service::default());
        let server = start_router(app).await;
        let client = Client::new(server.url.parse().unwrap(), ClientOptions::default()).unwrap();
        let error = client.state(&device_id()).await.unwrap_err();
        assert_eq!(error.kind(), "contract");
        assert_eq!(error.http_status(), Some(200));
    }
}

#[tokio::test]
async fn read_deadline_covers_headers_and_body() {
    for delayed_body in [true, false] {
        let app = Router::new().route(
            "/prefix/api/v2/devices",
            get(move || async move {
                if !delayed_body {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                axum::body::Body::from_stream(futures_util::stream::once(async {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{\"devices\":[]}"))
                }))
            }),
        );
        let server = start_router(app).await;
        let client = Client::new(
            server.url.parse().unwrap(),
            ClientOptions {
                read_timeout: Duration::from_millis(100),
                ..ClientOptions::default()
            },
        )
        .unwrap();
        assert_eq!(client.devices().await.unwrap_err().kind(), "timeout");
    }
}
