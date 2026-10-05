use axum::Router;
use gafctl_quickconnect::{AccountRole, Credentials, QuickConnectClient, QuickConnectConfig};
use tokio_util::task::AbortOnDropHandle;

pub(crate) fn cloud_device(provider_id: &str, name: &str) -> crate::backend::CloudDeviceInput {
    crate::backend::CloudDeviceInput::new(provider_id.to_owned(), name.to_owned())
}

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
fn identity_store_fixture_stays_under_target() {
    let (directory, path) = identity_store_fixture();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures");
    assert_eq!(directory.path().parent(), Some(root.as_path()));
    assert_eq!(path.parent(), Some(directory.path()));
}
