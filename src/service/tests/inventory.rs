use super::*;
use crate::test_support::identity_store_fixture;
use crate::{api::router, backend::DeviceRegistry};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn source_route_persists_owner_and_rejects_split_or_unconfigured_mqtt() {
    let (_directory, path) = identity_store_fixture();
    let state = DeviceService::with_ble_device(
        "no-physical-device".to_owned(),
        DeviceRegistry::load(&path).unwrap(),
    );
    let request = || {
        Request::builder()
            .method("PUT")
            .uri("/api/v2/devices/configured/sources")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"state_source":"http","command_source":"http"}"#,
            ))
            .unwrap()
    };
    let response = router(state.clone()).oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let descriptor: gafctl_api::DeviceDescriptor = serde_json::from_slice(&body).unwrap();
    assert_eq!(descriptor.id, DeviceId::configured_ble());
    for (sources, expected) in [
        (
            r#"{"state_source":"mqtt","command_source":"http"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            r#"{"state_source":"mqtt","command_source":"mqtt"}"#,
            StatusCode::CONFLICT,
        ),
    ] {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v2/devices/configured/sources")
                    .header("content-type", "application/json")
                    .body(Body::from(sources))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}
