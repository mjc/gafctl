use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode, Uri, header},
    response::IntoResponse,
    routing::get,
    routing::post,
};
use serde_json::{Value, json};
use updraft_quickconnect::{
    AccountRole, Credentials, DeviceModeStatus, QuickConnectClient, QuickConnectCommand,
    QuickConnectCommandMode, QuickConnectConfig, QuickConnectSettings, QuickConnectSettingsBody,
    build_settings_body,
};

type LoginCapture = Arc<tokio::sync::Mutex<Option<Value>>>;
type AuthorizationCapture = Arc<tokio::sync::Mutex<Option<String>>>;
type Captures = (LoginCapture, AuthorizationCapture);

fn typed_settings_body() -> QuickConnectSettingsBody {
    let current = QuickConnectSettings {
        mode: DeviceModeStatus::Automatic,
        automatic_temperature_f: Some(105),
        automatic_humidity_percent: Some(40),
        timer_duration_minutes: Some(60),
        humidity_monitor: Some(true),
    };
    build_settings_body(
        &QuickConnectCommand::SetMode {
            mode: QuickConnectCommandMode::Automatic,
        },
        &current,
    )
    .unwrap()
}

#[tokio::test]
async fn login_trims_username_encodes_utf8_password_and_sends_literal_token_header() {
    let captured_login = Arc::new(tokio::sync::Mutex::new(None));
    let captured_authorization = Arc::new(tokio::sync::Mutex::new(None));
    let app = Router::new()
        .route("/cognito/login", post(capture_login))
        .route("/gaf/device/deviceList", get(capture_authorization))
        .with_state((
            Arc::clone(&captured_login),
            Arc::clone(&captured_authorization),
        ));
    let (base_url, server) = start_server(app).await;
    let client = QuickConnectClient::new(
        Credentials::new("  fan@example.invalid  ", "café", AccountRole::Consumer),
        QuickConnectConfig::new(
            base_url.join("cognito/").unwrap(),
            base_url.join("gaf/").unwrap(),
        ),
    )
    .unwrap();

    client.list_devices().await.unwrap();

    assert_eq!(
        *captured_login.lock().await,
        Some(json!({
            "userName": "fan@example.invalid",
            "password": "Y2Fmw6k=",
            "userPoolId": "us-east-2_F6aHzg32w",
            "userRole": "consumer"
        }))
    );
    assert_eq!(
        captured_authorization.lock().await.as_deref(),
        Some("SYNTHETIC_TOKEN_DO_NOT_USE")
    );
    server.abort();
}

#[test]
fn api_roots_require_tls_except_for_loopback_http() {
    let credentials = || Credentials::new("user", "password", AccountRole::Contractor);
    let https = reqwest::Url::parse("https://api.example.invalid/root/").unwrap();
    let loopback = reqwest::Url::parse("http://127.0.0.1:8080/root/").unwrap();
    let ipv6_loopback = reqwest::Url::parse("http://[::1]:8080/root/").unwrap();
    let localhost = reqwest::Url::parse("http://localhost:8080/root/").unwrap();
    let public_http = reqwest::Url::parse("http://api.example.invalid/root/").unwrap();
    let other_scheme = reqwest::Url::parse("ftp://api.example.invalid/root/").unwrap();

    assert!(
        QuickConnectClient::new(
            credentials(),
            QuickConnectConfig::new(https.clone(), https.clone()),
        )
        .is_ok()
    );
    assert!(
        QuickConnectClient::new(
            credentials(),
            QuickConnectConfig::new(loopback.clone(), localhost),
        )
        .is_ok()
    );
    assert!(
        QuickConnectClient::new(
            credentials(),
            QuickConnectConfig::new(ipv6_loopback, loopback.clone()),
        )
        .is_ok()
    );
    assert!(
        QuickConnectClient::new(
            credentials(),
            QuickConnectConfig::new(public_http.clone(), https.clone()),
        )
        .is_err()
    );
    assert!(
        QuickConnectClient::new(credentials(), QuickConnectConfig::new(https, other_scheme),)
            .is_err()
    );
}

#[tokio::test]
async fn read_only_poll_keeps_inventory_when_one_detail_fetch_fails() {
    let failed_detail_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(successful_login))
        .route("/gaf/device/deviceList", get(two_device_inventory))
        .route("/gaf/device", get(detail_by_id))
        .with_state(Arc::clone(&failed_detail_requests));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    let inventory = client.read_inventory().await.unwrap();
    let failed = client.read_device_state(inventory[0].provider_id()).await;
    let successful = client
        .read_device_state(inventory[1].provider_id())
        .await
        .unwrap();

    assert_eq!(inventory.len(), 2);
    assert_eq!(inventory[0].provider_id(), "synthetic-failed-device");
    assert_eq!(inventory[0].name(), Some("Failed detail"));
    assert_eq!(
        failed,
        Err(updraft_quickconnect::ClientError::HttpStatus(503))
    );
    assert_eq!(inventory[1].provider_id(), "synthetic-live-device");
    assert_eq!(successful.temperature_f, Some(78.0));
    assert_eq!(successful.humidity_percent, Some(44.0));
    assert_eq!(successful.settings.automatic_temperature_f, Some(105));
    assert_eq!(successful.settings.automatic_humidity_percent, Some(40));
    assert_eq!(successful.settings.timer_duration_minutes, Some(60));
    assert!(successful.fetched_at_unix_ms.is_some());
    assert_eq!(successful.observed_at_unix_ms, None);
    assert_eq!(failed_detail_requests.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn concurrent_unauthorized_reads_share_one_replacement_login() {
    let login_count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(count_login))
        .route("/gaf/device/deviceList", get(authorize_by_token))
        .with_state(Arc::clone(&login_count));
    let (base_url, server) = start_server(app).await;
    let client = QuickConnectClient::new(
        Credentials::new(
            "fan@example.invalid",
            "synthetic-password",
            AccountRole::Contractor,
        ),
        QuickConnectConfig::new(
            base_url.join("cognito/").unwrap(),
            base_url.join("gaf/").unwrap(),
        ),
    )
    .unwrap();

    let (first, second) = tokio::join!(client.list_devices(), client.list_devices());

    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(login_count.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn repeated_auth_rejection_stops_after_one_retry() {
    let login_count = Arc::new(AtomicUsize::new(0));
    let device_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(count_login_pair))
        .route("/gaf/device/deviceList", get(reject_every_read))
        .with_state((Arc::clone(&login_count), Arc::clone(&device_requests)));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client.list_devices().await.unwrap_err(),
        updraft_quickconnect::ClientError::Authentication
    );
    assert_eq!(login_count.load(Ordering::SeqCst), 2);
    assert_eq!(device_requests.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn transient_failures_do_not_reset_the_single_reauthentication_allowance() {
    let login_count = Arc::new(AtomicUsize::new(0));
    let device_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(count_login_pair))
        .route("/gaf/device/deviceList", get(auth_then_transient_then_auth))
        .with_state((Arc::clone(&login_count), Arc::clone(&device_requests)));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client.list_devices().await.unwrap_err(),
        updraft_quickconnect::ClientError::Authentication
    );
    assert_eq!(login_count.load(Ordering::SeqCst), 2);
    assert_eq!(device_requests.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn malformed_oversized_and_application_error_responses_are_distinct() {
    let app = Router::new()
        .route("/cognito/login", post(successful_login))
        .route("/gaf/device/deviceList", get(valid_empty_inventory))
        .route("/gaf/device/bad-json", get(malformed_json))
        .route("/gaf/device/large", get(oversized_json))
        .route("/gaf/device/service-error", get(service_error));
    let (base_url, server) = start_server(app).await;
    let credentials = Credentials::new("user", "password", AccountRole::Contractor);
    let client = QuickConnectClient::new(
        credentials,
        QuickConnectConfig::new(
            base_url.join("cognito/").unwrap(),
            base_url.join("gaf/").unwrap(),
        )
        .with_max_response_bytes(128),
    )
    .unwrap();

    assert_eq!(
        client.get_json("device/bad-json").await.unwrap_err(),
        updraft_quickconnect::ClientError::InvalidJson
    );
    assert_eq!(
        client.get_json("device/large").await.unwrap_err(),
        updraft_quickconnect::ClientError::ResponseTooLarge(128)
    );
    let service_error = client.get_json("device/service-error").await.unwrap_err();
    assert_eq!(
        service_error,
        updraft_quickconnect::ClientError::ServiceStatus(4444)
    );
    assert!(
        !service_error
            .to_string()
            .contains("private service response")
    );
    server.abort();
}

#[tokio::test]
async fn settings_rejection_keeps_the_provider_service_status() {
    let app = Router::new()
        .route("/cognito/login", post(successful_login))
        .route("/gaf/deviceMode/fan", post(reject_settings));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client
            .save_device_settings("fan", &typed_settings_body())
            .await
            .unwrap_err(),
        updraft_quickconnect::ClientError::ServiceStatus(4444)
    );
    server.abort();
}

#[tokio::test]
async fn untrusted_paths_cannot_change_the_api_origin() {
    let (base_url, server) = start_server(Router::new()).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client
            .get_json(r"\\attacker.example/collect")
            .await
            .unwrap_err(),
        updraft_quickconnect::ClientError::InvalidEndpoint
    );
    assert_eq!(
        client
            .get_json("//attacker.example/collect")
            .await
            .unwrap_err(),
        updraft_quickconnect::ClientError::InvalidEndpoint
    );
    assert_eq!(
        client.get_json(" /device/deviceList").await.unwrap_err(),
        updraft_quickconnect::ClientError::InvalidEndpoint
    );
    assert_eq!(
        client.get_json("device/%2e%2e/collect").await.unwrap_err(),
        updraft_quickconnect::ClientError::InvalidEndpoint
    );
    server.abort();
}

#[tokio::test]
async fn transient_refresh_login_failure_is_retried_within_its_budget() {
    let login_count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(transient_refresh_login))
        .route("/gaf/device/deviceList", get(reject_then_accept))
        .with_state(Arc::clone(&login_count));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert!(client.list_devices().await.is_ok());
    assert_eq!(login_count.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn exhausted_refresh_retries_return_the_original_transient_failure() {
    let login_count = Arc::new(AtomicUsize::new(0));
    let device_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(failing_refresh_login))
        .route("/gaf/device/deviceList", get(reject_initial_token))
        .with_state((Arc::clone(&login_count), Arc::clone(&device_requests)));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client.list_devices().await.unwrap_err(),
        updraft_quickconnect::ClientError::HttpStatus(503)
    );
    assert_eq!(login_count.load(Ordering::SeqCst), 4);
    assert_eq!(device_requests.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn exhausted_initial_login_retries_do_not_restart_the_login_budget() {
    let login_count = Arc::new(AtomicUsize::new(0));
    let device_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(failing_initial_login))
        .route("/gaf/device/deviceList", get(count_device_request))
        .with_state((Arc::clone(&login_count), Arc::clone(&device_requests)));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client.list_devices().await.unwrap_err(),
        updraft_quickconnect::ClientError::HttpStatus(503)
    );
    assert_eq!(login_count.load(Ordering::SeqCst), 3);
    assert_eq!(device_requests.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn successful_http_response_with_unknown_inventory_shape_is_a_schema_error() {
    let app = Router::new()
        .route("/cognito/login", post(successful_login))
        .route("/gaf/device/deviceList", get(invalid_inventory));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client.list_devices().await.unwrap_err(),
        updraft_quickconnect::ClientError::InvalidEnvelope
    );
    server.abort();
}

#[tokio::test]
async fn device_detail_encodes_provider_identifier_as_a_query_value() {
    let captured_query = Arc::new(tokio::sync::Mutex::new(None));
    let app = Router::new()
        .route("/cognito/login", post(successful_login))
        .route("/gaf/device", get(capture_detail_query))
        .with_state(Arc::clone(&captured_query));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    client.device_detail("fan&other=secret").await.unwrap();

    assert_eq!(
        captured_query.lock().await.as_deref(),
        Some("deviceId=fan%26other%3Dsecret")
    );
    server.abort();
}

#[tokio::test]
async fn missing_login_token_is_an_authentication_error() {
    let app = Router::new().route("/cognito/login", post(login_without_token));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client.list_devices().await.unwrap_err(),
        updraft_quickconnect::ClientError::Authentication
    );
    server.abort();
}

#[tokio::test]
async fn login_client_error_status_is_an_authentication_error() {
    let app = Router::new().route("/cognito/login", post(reject_login));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client.list_devices().await.unwrap_err(),
        updraft_quickconnect::ClientError::Authentication
    );
    server.abort();
}

#[tokio::test]
async fn reads_retry_bounded_server_errors_and_writes_are_never_retried() {
    let read_count = Arc::new(AtomicUsize::new(0));
    let write_count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(successful_login))
        .route("/gaf/device/deviceList", get(retry_read))
        .route("/gaf/deviceMode/fan", post(fail_write))
        .with_state((Arc::clone(&read_count), Arc::clone(&write_count)));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert!(client.list_devices().await.is_ok());
    assert_eq!(read_count.load(Ordering::SeqCst), 3);
    assert_eq!(
        client
            .save_device_settings("fan", &typed_settings_body())
            .await
            .unwrap_err(),
        updraft_quickconnect::ClientError::HttpStatus(500)
    );
    assert_eq!(write_count.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn timed_out_settings_write_is_not_replayed() {
    let write_count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(successful_login))
        .route("/gaf/deviceMode/fan", post(slow_write))
        .with_state(Arc::clone(&write_count));
    let (base_url, server) = start_server(app).await;
    let credentials = Credentials::new("user", "password", AccountRole::Contractor);
    let config = QuickConnectConfig::new(
        base_url.join("cognito/").unwrap(),
        base_url.join("gaf/").unwrap(),
    )
    .with_timeout(std::time::Duration::from_millis(50));
    let client = QuickConnectClient::new(credentials, config).unwrap();

    assert_eq!(
        client
            .save_device_settings("fan", &typed_settings_body())
            .await
            .unwrap_err(),
        updraft_quickconnect::ClientError::Transport
    );
    assert_eq!(write_count.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn timed_out_reads_use_the_finite_retry_budget() {
    let read_count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(successful_login))
        .route("/gaf/device/deviceList", get(slow_read))
        .with_state(Arc::clone(&read_count));
    let (base_url, server) = start_server(app).await;
    let credentials = Credentials::new("user", "password", AccountRole::Contractor);
    let config = QuickConnectConfig::new(
        base_url.join("cognito/").unwrap(),
        base_url.join("gaf/").unwrap(),
    )
    .with_timeout(std::time::Duration::from_millis(50));
    let client = QuickConnectClient::new(credentials, config).unwrap();

    assert_eq!(
        client.list_devices().await.unwrap_err(),
        updraft_quickconnect::ClientError::Transport
    );
    assert_eq!(read_count.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn redirects_are_not_followed_with_account_authorization() {
    let redirected_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/cognito/login", post(successful_login))
        .route("/gaf/device/deviceList", get(redirect_read))
        .route("/gaf/collect", get(collect_redirect))
        .with_state(Arc::clone(&redirected_requests));
    let (base_url, server) = start_server(app).await;
    let client = test_client(
        base_url,
        Credentials::new("user", "password", AccountRole::Contractor),
    );

    assert_eq!(
        client.list_devices().await.unwrap_err(),
        updraft_quickconnect::ClientError::HttpStatus(302)
    );
    assert_eq!(redirected_requests.load(Ordering::SeqCst), 0);
    server.abort();
}

#[test]
fn credentials_debug_redacts_username_and_password_and_roles_serialize() {
    let credentials = Credentials::new("private-user", "private-password", AccountRole::Consumer);
    let debug = format!("{credentials:?}");
    assert!(!debug.contains("private-user"));
    assert!(!debug.contains("private-password"));
    assert_eq!(
        serde_json::to_value(AccountRole::Contractor).unwrap(),
        "contractor"
    );
    assert_eq!(
        serde_json::to_value(AccountRole::Consumer).unwrap(),
        "consumer"
    );
}

async fn start_server(app: Router) -> (reqwest::Url, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
        server,
    )
}

async fn capture_login(
    State((captured_login, _)): State<Captures>,
    Json(body): Json<Value>,
) -> Json<Value> {
    *captured_login.lock().await = Some(body);
    Json(json!({"responseData": {"idToken": "SYNTHETIC_TOKEN_DO_NOT_USE"}}))
}

async fn capture_authorization(
    State((_, captured_authorization)): State<Captures>,
    headers: HeaderMap,
) -> Json<Value> {
    *captured_authorization.lock().await = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    Json(json!({"responseData": []}))
}

async fn count_login(State(login_count): State<Arc<AtomicUsize>>) -> Json<Value> {
    let attempt = login_count.fetch_add(1, Ordering::SeqCst);
    Json(
        json!({"responseData": {"idToken": if attempt == 0 { "first-token" } else { "replacement-token" }}}),
    )
}

async fn count_login_pair(
    State((login_count, _)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> Json<Value> {
    let attempt = login_count.fetch_add(1, Ordering::SeqCst);
    Json(
        json!({"responseData": {"idToken": if attempt == 0 { "first-token" } else { "replacement-token" }}}),
    )
}

async fn authorize_by_token(headers: HeaderMap) -> (axum::http::StatusCode, Json<Value>) {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    match token {
        Some("first-token") => (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"message": "expired"})),
        ),
        Some("replacement-token") => (
            axum::http::StatusCode::OK,
            Json(json!({"responseData": []})),
        ),
        _ => (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"message": "missing token"})),
        ),
    }
}

fn test_client(base_url: reqwest::Url, credentials: Credentials) -> QuickConnectClient {
    QuickConnectClient::new(
        credentials,
        QuickConnectConfig::new(
            base_url.join("cognito/").unwrap(),
            base_url.join("gaf/").unwrap(),
        ),
    )
    .unwrap()
}

async fn successful_login() -> Json<Value> {
    Json(json!({"responseData": {"idToken": "SYNTHETIC_TOKEN_DO_NOT_USE"}}))
}

async fn two_device_inventory() -> Json<Value> {
    Json(json!({"responseData": [
        {"deviceId": "synthetic-failed-device", "name": "Failed detail"},
        {"deviceId": "synthetic-live-device", "name": "Live detail"}
    ]}))
}

async fn detail_by_id(
    State(failed_detail_requests): State<Arc<AtomicUsize>>,
    uri: Uri,
) -> Response<Body> {
    match uri.query() {
        Some("deviceId=synthetic-failed-device") => {
            failed_detail_requests.fetch_add(1, Ordering::SeqCst);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"statusCode": 503})),
            )
                .into_response()
        }
        Some("deviceId=synthetic-live-device") => Json(json!({
            "responseData": {
                "deviceConfig": {"setTemperature": 78.0, "setHumidity": 44.0},
                "deviceSettings": {
                    "automaticMode": true,
                    "timerMode": false,
                    "fanMode": false,
                    "setTemperature": 105,
                    "setHumidity": 40,
                    "timerValue": 60,
                    "humidityMonitor": true
                },
                "firmwareVersion": "synthetic-firmware"
            }
        }))
        .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn valid_empty_inventory() -> Json<Value> {
    Json(json!({"responseData": []}))
}

async fn invalid_inventory() -> Json<Value> {
    Json(json!({"message": "unknown service response"}))
}

async fn capture_detail_query(
    State(captured_query): State<Arc<tokio::sync::Mutex<Option<String>>>>,
    uri: Uri,
) -> Json<Value> {
    *captured_query.lock().await = uri.query().map(str::to_owned);
    Json(json!({"responseData": {"deviceId": "synthetic"}}))
}

async fn reject_every_read(
    State((_, device_requests)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> (StatusCode, Json<Value>) {
    device_requests.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"message": "expired"})),
    )
}

async fn auth_then_transient_then_auth(
    State((_, device_requests)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> Response<Body> {
    match device_requests.fetch_add(1, Ordering::SeqCst) {
        0 | 2 => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"message": "expired"})),
        )
            .into_response(),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"statusCode": 503})),
        )
            .into_response(),
    }
}

async fn reject_settings() -> (StatusCode, Json<Value>) {
    (
        StatusCode::EXPECTATION_FAILED,
        Json(json!({"statusCode": 4444, "message": "settings rejected"})),
    )
}

async fn transient_refresh_login(State(login_count): State<Arc<AtomicUsize>>) -> Response<Body> {
    match login_count.fetch_add(1, Ordering::SeqCst) {
        0 => Json(json!({"responseData": {"idToken": "first-token"}})).into_response(),
        1 => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"statusCode": 503})),
        )
            .into_response(),
        _ => Json(json!({"responseData": {"idToken": "replacement-token"}})).into_response(),
    }
}

async fn reject_then_accept(
    State(_login_count): State<Arc<AtomicUsize>>,
    headers: HeaderMap,
) -> Response<Body> {
    match headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        Some("first-token") => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"message": "expired"})),
        )
            .into_response(),
        Some("replacement-token") => Json(json!({"responseData": []})).into_response(),
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"message": "missing"})),
        )
            .into_response(),
    }
}

async fn failing_refresh_login(
    State((login_count, _)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> Response<Body> {
    match login_count.fetch_add(1, Ordering::SeqCst) {
        0 => Json(json!({"responseData": {"idToken": "first-token"}})).into_response(),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"statusCode": 503})),
        )
            .into_response(),
    }
}

async fn reject_initial_token(
    State((_, device_requests)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> Response<Body> {
    device_requests.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"message": "expired"})),
    )
        .into_response()
}

async fn failing_initial_login(
    State((login_count, _)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> Response<Body> {
    login_count.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"statusCode": 503})),
    )
        .into_response()
}

async fn count_device_request(
    State((_, device_requests)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> Json<Value> {
    device_requests.fetch_add(1, Ordering::SeqCst);
    Json(json!({"responseData": []}))
}

async fn malformed_json() -> Response<Body> {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        "{",
    )
        .into_response()
}

async fn oversized_json() -> Json<Value> {
    Json(json!({"data": "x".repeat(256)}))
}

async fn service_error() -> Json<Value> {
    Json(json!({"statusCode": 4444, "message": "private service response"}))
}

async fn login_without_token() -> Json<Value> {
    Json(json!({"responseData": {}}))
}

async fn reject_login() -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"statusInfo": "rejected"})),
    )
}

async fn retry_read(
    State((read_count, _)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> Response<Body> {
    match read_count.fetch_add(1, Ordering::SeqCst) {
        0 => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"statusCode": 4444})),
        )
            .into_response(),
        1 => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "0")],
            Json(json!({"statusCode": 429})),
        )
            .into_response(),
        _ => Json(json!({"responseData": []})).into_response(),
    }
}

async fn fail_write(
    State((_, write_count)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
    Json(_body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    write_count.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"message": "failed"})),
    )
}

async fn slow_read(State(read_count): State<Arc<AtomicUsize>>) -> Json<Value> {
    read_count.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Json(json!({"responseData": []}))
}

async fn slow_write(
    State(write_count): State<Arc<AtomicUsize>>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    write_count.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Json(json!({"responseData": {}}))
}

async fn redirect_read() -> Response<Body> {
    (
        StatusCode::FOUND,
        [(header::LOCATION, "/gaf/collect")],
        Json(json!({"responseData": []})),
    )
        .into_response()
}

async fn collect_redirect(
    State(redirected_requests): State<Arc<AtomicUsize>>,
    _headers: HeaderMap,
) -> Json<Value> {
    redirected_requests.fetch_add(1, Ordering::SeqCst);
    Json(json!({"responseData": []}))
}
