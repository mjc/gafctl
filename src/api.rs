use crate::service::{DeviceService, ServiceError};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use gafctl_api::CommandId;
use gafctl_api::{
    ControlStatus as V2ControlStatus, DeviceControlV2Request, DeviceControlV2Response,
    DeviceListV2Response, DeviceStateV2Response,
};
use gafctl_api::{DeviceDescriptor, DeviceId, EntitySources};
use serde::Serialize;
use tokio::net::TcpListener;

pub(crate) async fn serve_http_until_shutdown(
    listener: TcpListener,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server failed")
}

pub(crate) fn router(state: DeviceService) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v2/devices", get(devices_v2))
        .route("/api/v2/devices/{id}/state", get(device_state_v2))
        .route("/api/v2/devices/{id}/refresh", post(refresh_device_v2))
        .route("/api/v2/devices/{id}/control", post(control_device_v2))
        .route("/api/v2/devices/{id}/sources", put(set_device_sources_v2))
        .with_state(state)
}

async fn devices_v2(State(state): State<DeviceService>) -> Json<DeviceListV2Response> {
    Json(state.inventory().await)
}

async fn device_state_v2(
    State(state): State<DeviceService>,
    Path(id): Path<String>,
) -> Result<Json<DeviceStateV2Response>, StatusCode> {
    let id = DeviceId::parse(id).ok_or(StatusCode::NOT_FOUND)?;
    state.state(&id).await.map(Json).map_err(service_status)
}

async fn refresh_device_v2(
    State(state): State<DeviceService>,
    Path(id): Path<String>,
) -> Result<Response, StatusCode> {
    let id = DeviceId::parse(id).ok_or(StatusCode::NOT_FOUND)?;
    let response = state.refresh_device(&id).await.map_err(service_status)?;
    let status = StatusCode::from_u16(response.status.http_status())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((status, Json(response.as_ref())).into_response())
}

async fn control_device_v2(
    State(state): State<DeviceService>,
    Path(id): Path<String>,
    Json(request): Json<DeviceControlV2Request>,
) -> (StatusCode, Json<DeviceControlV2Response>) {
    let Some(id) = DeviceId::parse(id) else {
        return v2_control_rejected(request.request_id, V2ControlStatus::UnknownDevice);
    };
    let response = state.control(id, request).await;
    let status = status_for_v2_outcome(&response.status);
    (status, Json(response))
}

fn v2_control_rejected(
    request_id: CommandId,
    status: V2ControlStatus,
) -> (StatusCode, Json<DeviceControlV2Response>) {
    (
        status_for_v2_outcome(&status),
        Json(DeviceControlV2Response {
            request_id: request_id.as_str().to_owned(),
            status,
        }),
    )
}

fn status_for_v2_outcome(outcome: &V2ControlStatus) -> StatusCode {
    match outcome {
        V2ControlStatus::Confirmed => StatusCode::OK,
        V2ControlStatus::UnknownDevice | V2ControlStatus::DeviceUnavailable => {
            StatusCode::NOT_FOUND
        }
        V2ControlStatus::BackendUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        V2ControlStatus::Busy => StatusCode::TOO_MANY_REQUESTS,
        V2ControlStatus::InvalidRequestId => StatusCode::BAD_REQUEST,
        V2ControlStatus::ControlFailed => StatusCode::INTERNAL_SERVER_ERROR,
        V2ControlStatus::Unconfirmed
        | V2ControlStatus::SubmittedUnconfirmed
        | V2ControlStatus::ReadbackMismatch
        | V2ControlStatus::ReadbackUnavailable => StatusCode::BAD_GATEWAY,
        V2ControlStatus::Rejected
        | V2ControlStatus::UnsupportedCommand
        | V2ControlStatus::StaleRequest
        | V2ControlStatus::RequestIdReused
        | V2ControlStatus::Unknown(_) => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn set_device_sources_v2(
    State(state): State<DeviceService>,
    Path(id): Path<String>,
    Json(sources): Json<EntitySources>,
) -> Result<Json<DeviceDescriptor>, StatusCode> {
    let id = DeviceId::parse(id).ok_or(StatusCode::NOT_FOUND)?;
    state
        .set_sources(&id, sources)
        .await
        .map(Json)
        .map_err(service_status)
}

fn service_status(error: ServiceError) -> StatusCode {
    match error {
        ServiceError::UnknownDevice => StatusCode::NOT_FOUND,
        ServiceError::UnsupportedRead | ServiceError::InvalidSources => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        ServiceError::BackendUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        ServiceError::OwnershipUnavailable => StatusCode::CONFLICT,
        ServiceError::WorkerUnavailable | ServiceError::Persistence => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[cfg(test)]
mod tests;
