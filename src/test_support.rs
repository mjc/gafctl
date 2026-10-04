use axum::Router;
use gafctl_quickconnect::{AccountRole, Credentials, QuickConnectClient, QuickConnectConfig};
use tokio_util::task::AbortOnDropHandle;

pub(crate) async fn mock_client(app: Router) -> (QuickConnectClient, AbortOnDropHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    let base = reqwest::Url::parse(&format!("http://{address}/")).unwrap();
    let client = QuickConnectClient::new(
        Credentials::new(
            "synthetic-user",
            "synthetic-password",
            AccountRole::Contractor,
        ),
        QuickConnectConfig::new(base.join("cognito/").unwrap(), base.join("gaf/").unwrap()),
    )
    .unwrap();
    (client, server)
}

#[tokio::test]
async fn dropping_mock_client_stops_its_http_listener() {
    let app = Router::new()
        .route(
            "/cognito/login",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({"responseData":{"idToken":"synthetic-token"}}))
            }),
        )
        .route(
            "/gaf/device/deviceList",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"responseData":[]})) }),
        );
    let (client, server) = mock_client(app).await;
    assert!(client.list_devices().await.is_ok());
    let task = server.abort_handle();
    drop(server);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        std::future::poll_fn(|context| {
            if task.is_finished() {
                std::task::Poll::Ready(())
            } else {
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }),
    )
    .await
    .unwrap();
}

pub(crate) fn identity_store_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("gafctl-api-identities-")
        .tempdir_in(root)
        .unwrap();
    let path = directory.path().join("identities.json");
    (directory, path)
}

#[test]
fn identity_store_fixture_stays_under_target_and_cleans_up_on_drop() {
    let (directory, path) = identity_store_fixture();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures");
    assert_eq!(directory.path().parent(), Some(root.as_path()));
    assert_eq!(path.parent(), Some(directory.path()));
    std::fs::write(&path, b"synthetic fixture").unwrap();
    assert!(path.is_file());
    drop(directory);
    assert!(!path.parent().unwrap().exists());
}

#[test]
fn identity_store_fixture_cleans_up_when_test_scope_unwinds() {
    let (directory, path) = identity_store_fixture();
    std::fs::write(&path, b"synthetic fixture").unwrap();
    let failure = std::panic::catch_unwind(|| {
        let _directory = directory;
        std::panic::resume_unwind(Box::new(()));
    });
    assert!(failure.is_err());
    assert!(!path.parent().unwrap().exists());
}
