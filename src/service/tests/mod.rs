use super::test_support::*;
use super::*;
use crate::api::router;
use crate::test_support::identity_store_fixture;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
#[cfg(feature = "mqtt")]
use futures_util::{StreamExt, stream};
use gafctl_api::{
    DeviceBackend, DeviceId, DeviceRefreshStatus, DeviceRefreshV2Response, DeviceSettings,
    DeviceState, StateProvenance, unix_millis,
};
use http_body_util::BodyExt;
use std::time::{Duration, SystemTime};
#[cfg(feature = "mqtt")]
use tokio::sync::watch;
use tower::ServiceExt;
mod quickconnect;
mod refresh;
