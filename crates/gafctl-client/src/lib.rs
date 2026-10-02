//! Typed HTTP access to a running Gafctl service.
//!
//! ```no_run
//! use gafctl_api::{ControlPreset, DeviceCommand, DeviceId};
//! use gafctl_client::{Client, ClientOptions, ControlResult};
//!
//! async fn clear_timer() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = Client::new("http://127.0.0.1:8787".parse()?, ClientOptions::default())?;
//!     let device = DeviceId::configured_ble();
//!     let command = DeviceCommand::LegacyPreset { preset: ControlPreset::TimerClear };
//!     let intent = client.prepare_control(&device, command).await?;
//!     let result = intent.submit("example-request".parse()?).await?;
//!     match result {
//!         ControlResult::Confirmed(_) => Ok(()),
//!         ControlResult::Unconfirmed(_) => Err("control was not confirmed".into()),
//!     }
//! }
//! ```

use std::{
    str::FromStr,
    time::{Duration, SystemTime},
};

use gafctl_api::{
    CommandId, ContractError, DeviceCommand, DeviceControlV2Request, DeviceControlV2Response,
    DeviceDescriptor, DeviceId, DeviceListV2Response, DeviceRefreshStatus, DeviceRefreshV2Response,
    DeviceStateV2Response, unix_millis,
};
use reqwest::{RequestBuilder, Url, redirect::Policy};
use serde::de::DeserializeOwned;
use thiserror::Error;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// An HTTP service base URL, including an optional reverse-proxy path prefix.
#[derive(Clone, Debug)]
pub struct ServerUrl(Url);

impl FromStr for ServerUrl {
    type Err = ClientError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut url = Url::parse(value.trim())
            .map_err(|_| ClientError::Configuration("invalid server URL"))?;
        if !["http", "https"].contains(&url.scheme())
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ClientError::Configuration(
                "server URL must be HTTP or HTTPS without credentials, query, or fragment",
            ));
        }
        url.path_segments_mut()
            .map_err(|_| ClientError::Configuration("server URL has no path"))?
            .pop_if_empty()
            .push("");
        Ok(Self(url))
    }
}

impl ServerUrl {
    pub fn as_url(&self) -> &Url {
        &self.0
    }

    fn endpoint(&self, id: Option<&DeviceId>, operation: Option<&str>) -> Url {
        let mut url = self.0.clone();
        // ServerUrl only admits HTTP URLs, whose paths are mutable.
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(["api", "v2", "devices"]);
            if let Some(id) = id {
                path.push(id.as_str());
            }
            if let Some(operation) = operation {
                path.push(operation);
            }
        }
        url
    }
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub read_timeout: Duration,
    pub control_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            read_timeout: Duration::from_secs(10),
            control_timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("{0}")]
    Configuration(&'static str),
    #[error("service request timed out")]
    Timeout,
    #[error("could not complete the service request")]
    Transport(#[source] reqwest::Error),
    #[error("service returned HTTP {0}")]
    Http(u16),
    #[error("service response exceeds the two-MiB limit")]
    ResponseTooLarge,
    #[error("service returned an invalid JSON contract")]
    Decoding(#[source] serde_json::Error),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("service returned a different device or backend")]
    Identity,
    #[error("service HTTP status does not match the refresh outcome")]
    RefreshStatus,
    #[error("service returned a different control request ID")]
    Correlation,
    #[error("device is not in the service inventory")]
    UnknownDevice,
    #[error("device does not advertise this operation")]
    Unsupported,
    #[error("system clock cannot produce a Unix-millisecond timestamp")]
    Clock,
    #[error("service returned HTTP {http_status}: {source}")]
    Response {
        http_status: u16,
        #[source]
        source: Box<ClientError>,
    },
    #[error("control outcome unknown for request {request_id}: {source}")]
    Submission {
        request_id: CommandId,
        #[source]
        source: Box<ClientError>,
    },
}

impl ClientError {
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Http(status)
            | Self::Response {
                http_status: status,
                ..
            } => Some(*status),
            Self::Submission { source, .. } => source.http_status(),
            _ => None,
        }
    }

    pub fn request_id(&self) -> Option<&CommandId> {
        match self {
            Self::Submission { request_id, .. } => Some(request_id),
            _ => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "configuration",
            Self::Timeout => "timeout",
            Self::Transport(_) => "transport",
            Self::Http(_) => "http",
            Self::ResponseTooLarge => "response_too_large",
            Self::Decoding(_) => "decoding",
            Self::Contract(_) | Self::Identity | Self::RefreshStatus => "contract",
            Self::Correlation => "correlation",
            Self::UnknownDevice => "unknown_device",
            Self::Unsupported => "unsupported_command",
            Self::Clock => "clock",
            Self::Submission { source, .. } | Self::Response { source, .. } => source.kind(),
        }
    }
}

fn transport_error(error: reqwest::Error) -> ClientError {
    if error.is_timeout() {
        ClientError::Timeout
    } else {
        ClientError::Transport(error.without_url())
    }
}

/// HTTP client for service discovery, readings, and controls.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    server: ServerUrl,
    options: ClientOptions,
}

impl Client {
    pub fn new(server: ServerUrl, options: ClientOptions) -> Result<Self, ClientError> {
        let now = std::time::Instant::now();
        if options.read_timeout.is_zero()
            || options.control_timeout.is_zero()
            || now.checked_add(options.read_timeout).is_none()
            || now.checked_add(options.control_timeout).is_none()
        {
            return Err(ClientError::Configuration(
                "request timeouts must be positive and representable on this platform",
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .user_agent(concat!("gafctl/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            http,
            server,
            options,
        })
    }

    pub async fn devices(&self) -> Result<DeviceListV2Response, ClientError> {
        let (status, result): (_, DeviceListV2Response) =
            self.get(self.server.endpoint(None, None)).await?;
        result
            .validate()
            .map_err(|error| response_error(status, ClientError::Contract(error)))?;
        Ok(result)
    }

    async fn descriptor(&self, id: &DeviceId) -> Result<DeviceDescriptor, ClientError> {
        self.devices()
            .await?
            .devices
            .into_iter()
            .find(|device| &device.id == id)
            .ok_or(ClientError::UnknownDevice)
    }

    pub async fn state(&self, id: &DeviceId) -> Result<DeviceStateV2Response, ClientError> {
        let descriptor = self.descriptor(id).await?;
        if !descriptor.capabilities.read_state {
            return Err(ClientError::Unsupported);
        }
        let (status, result): (_, DeviceStateV2Response) = self
            .get(self.server.endpoint(Some(id), Some("state")))
            .await?;
        result
            .validate()
            .map_err(|error| response_error(status, ClientError::Contract(error)))?;
        if &result.id != id || result.backend != descriptor.backend {
            return Err(response_error(status, ClientError::Identity));
        }
        Ok(result)
    }

    /// Read through the service's device backend. Concurrent requests share one
    /// worker; failed and superseded reads return their status.
    pub async fn refresh(&self, id: &DeviceId) -> Result<DeviceRefreshV2Response, ClientError> {
        let descriptor = self.descriptor(id).await?;
        if !descriptor.capabilities.read_state {
            return Err(ClientError::Unsupported);
        }
        let (status, body) = bounded_response(
            self.http
                .post(self.server.endpoint(Some(id), Some("refresh")))
                .timeout(self.options.control_timeout),
        )
        .await?;
        if ![200, 409, 502].contains(&status) {
            return Err(ClientError::Http(status));
        }
        let response: DeviceRefreshV2Response = serde_json::from_slice(&body)
            .map_err(|error| response_error(status, ClientError::Decoding(error)))?;
        response
            .validate()
            .map_err(|error| response_error(status, ClientError::Contract(error)))?;
        if &response.device.id != id || response.device.backend != descriptor.backend {
            return Err(response_error(status, ClientError::Identity));
        }
        let expected_status = match response.status {
            DeviceRefreshStatus::Fresh => 200,
            DeviceRefreshStatus::Failed => 502,
            DeviceRefreshStatus::Superseded => 409,
        };
        if status != expected_status {
            return Err(response_error(status, ClientError::RefreshStatus));
        }
        Ok(response)
    }

    /// Resolve inventory and capabilities before a control can be submitted.
    pub async fn prepare_control(
        &self,
        id: &DeviceId,
        command: DeviceCommand,
    ) -> Result<PreparedControl<'_>, ClientError> {
        let descriptor = self.descriptor(id).await?;
        if !descriptor.capabilities.supports(command) {
            return Err(ClientError::Unsupported);
        }
        Ok(PreparedControl {
            client: self,
            id: id.clone(),
            command,
        })
    }

    async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<(u16, T), ClientError> {
        let (status, body) =
            bounded_response(self.http.get(url).timeout(self.options.read_timeout)).await?;
        if !(200..300).contains(&status) {
            return Err(ClientError::Http(status));
        }
        let result = serde_json::from_slice(&body)
            .map_err(|error| response_error(status, ClientError::Decoding(error)))?;
        Ok((status, result))
    }
}

/// A control ready to submit after checking device capabilities.
///
/// ```compile_fail
/// use gafctl_client::PreparedControl;
/// let intent = PreparedControl { command: todo!() };
/// ```
#[must_use]
pub struct PreparedControl<'a> {
    client: &'a Client,
    id: DeviceId,
    command: DeviceCommand,
}

impl PreparedControl<'_> {
    /// Send once, timestamped after checking capabilities. Dropping the future does
    /// not guarantee that the service cancelled the submitted command.
    pub async fn submit(self, request_id: CommandId) -> Result<ControlResult, ClientError> {
        let request = DeviceControlV2Request {
            request_id: request_id.clone(),
            issued_at_unix_ms: unix_millis(SystemTime::now()).ok_or(ClientError::Clock)?,
            command: self.command,
        };
        let builder = self
            .client
            .http
            .post(self.client.server.endpoint(Some(&self.id), Some("control")))
            .timeout(self.client.options.control_timeout)
            .json(&request);
        let result = async {
            let (http_status, body) = bounded_response(builder).await?;
            let response: DeviceControlV2Response = serde_json::from_slice(&body)
                .map_err(|error| response_error(http_status, ClientError::Decoding(error)))?;
            if response.request_id != request_id.as_str() {
                return Err(response_error(http_status, ClientError::Correlation));
            }
            let confirmed = (200..300).contains(&http_status) && response.status.is_confirmed();
            let receipt = ControlReceipt {
                http_status,
                response,
            };
            Ok(if confirmed {
                ControlResult::Confirmed(ConfirmedControl(receipt))
            } else {
                ControlResult::Unconfirmed(receipt)
            })
        }
        .await;
        result.map_err(|source| ClientError::Submission {
            request_id,
            source: Box::new(source),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ControlReceipt {
    http_status: u16,
    response: DeviceControlV2Response,
}

/// A control result with a matching request ID, successful HTTP status, and
/// confirmed backend outcome.
#[derive(Clone, Debug)]
pub struct ConfirmedControl(ControlReceipt);

/// Submission can complete without confirming the requested control.
/// Inspect this outcome even when the outer request result succeeded.
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// use gafctl_api::CommandId;
/// use gafctl_client::{ClientError, PreparedControl};
///
/// async fn control(intent: PreparedControl<'_>, request_id: CommandId) -> Result<(), ClientError> {
///     intent.submit(request_id).await?;
///     Ok(())
/// }
/// ```
#[must_use = "inspect the outcome to determine whether control was confirmed"]
#[derive(Clone, Debug)]
pub enum ControlResult {
    Confirmed(ConfirmedControl),
    Unconfirmed(ControlReceipt),
}

impl ControlResult {
    fn receipt(&self) -> &ControlReceipt {
        match self {
            Self::Confirmed(confirmed) => &confirmed.0,
            Self::Unconfirmed(receipt) => receipt,
        }
    }
    pub fn response(&self) -> &DeviceControlV2Response {
        &self.receipt().response
    }
    pub fn http_status(&self) -> u16 {
        self.receipt().http_status
    }
    #[must_use]
    pub fn is_confirmed(&self) -> bool {
        match self {
            Self::Confirmed(_) => true,
            Self::Unconfirmed(_) => false,
        }
    }
}

async fn bounded_response(builder: RequestBuilder) -> Result<(u16, Vec<u8>), ClientError> {
    let mut response = builder.send().await.map_err(transport_error)?;
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err(response_error(status, ClientError::ResponseTooLarge));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| response_error(status, transport_error(error)))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(response_error(status, ClientError::ResponseTooLarge));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((status, bytes))
}

fn response_error(http_status: u16, source: ClientError) -> ClientError {
    ClientError::Response {
        http_status,
        source: Box::new(source),
    }
}
