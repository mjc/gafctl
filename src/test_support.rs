use axum::Router;
use gafctl_quickconnect::{AccountRole, Credentials, QuickConnectClient, QuickConnectConfig};

pub(crate) async fn mock_client(app: Router) -> (QuickConnectClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
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
