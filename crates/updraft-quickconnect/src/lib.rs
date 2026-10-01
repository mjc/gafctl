use std::{fmt, sync::Arc, time::Duration, time::SystemTime};

use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::BytesMut;
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, StatusCode, Url, header::RETRY_AFTER, redirect::Policy};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::{sync::Mutex, time::sleep};

const USER_POOL_ID: &str = "us-east-2_F6aHzg32w";
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_READ_RETRIES: u8 = 2;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(2);

/// Account role accepted by the QuickConnect login endpoint.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountRole {
    /// Default role used by the reference integration.
    Contractor,
    /// Consumer role accepted by the reference integration.
    Consumer,
}

/// Account credentials. Debug output redacts both username and password.
pub struct Credentials {
    username: String,
    password: String,
    role: AccountRole,
}

impl Credentials {
    /// Create credentials for one account and role.
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
        role: AccountRole,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            role,
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("username", &"[redacted]")
            .field("password", &"[redacted]")
            .field("role", &self.role)
            .finish()
    }
}

/// Endpoint and resource limits for a QuickConnect account client.
#[derive(Clone)]
pub struct QuickConnectConfig {
    auth_base_url: Url,
    device_base_url: Url,
    timeout: Duration,
    max_response_bytes: usize,
}

impl QuickConnectConfig {
    /// Create a config for the public QuickConnect service endpoints.
    pub fn production() -> Result<Self, ClientError> {
        Ok(Self::new(
            Url::parse("https://gaf-coreservices.aurai.io/cognito/")
                .map_err(|_| ClientError::InvalidEndpoint)?,
            Url::parse("https://gaf.keenhome.io/gaf/").map_err(|_| ClientError::InvalidEndpoint)?,
        ))
    }

    /// Create a config with injected API roots. Each root should end in `/`.
    pub fn new(auth_base_url: Url, device_base_url: Url) -> Self {
        Self {
            auth_base_url,
            device_base_url,
            timeout: Duration::from_secs(20),
            max_response_bytes: MAX_RESPONSE_BYTES,
        }
    }

    /// Set a total timeout for each individual request.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the maximum number of response bytes retained in memory.
    pub fn with_max_response_bytes(mut self, limit: usize) -> Self {
        self.max_response_bytes = limit;
        self
    }
}

/// Sanitized QuickConnect transport, authentication, and response errors.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ClientError {
    /// A request could not be sent or its response stream failed.
    #[error("QuickConnect transport failed")]
    Transport,
    /// Login failed, returned no token, or the service rejected the account.
    #[error("QuickConnect authentication failed")]
    Authentication,
    /// The service rejected an operation with this HTTP status.
    #[error("QuickConnect returned HTTP status {0}")]
    HttpStatus(u16),
    /// The service returned a non-success application status.
    #[error("QuickConnect rejected the request with service status {0}")]
    ServiceStatus(u64),
    /// The response exceeded the configured body limit.
    #[error("QuickConnect response exceeded the {0}-byte limit")]
    ResponseTooLarge(usize),
    /// The response body was not valid JSON.
    #[error("QuickConnect returned malformed JSON")]
    InvalidJson,
    /// A successful response did not have the expected envelope or fields.
    #[error("QuickConnect returned an invalid response envelope")]
    InvalidEnvelope,
    /// The configured URL or path could not be used safely.
    #[error("QuickConnect endpoint configuration is invalid")]
    InvalidEndpoint,
}

/// Authenticated client for one QuickConnect account.
#[derive(Clone)]
pub struct QuickConnectClient {
    credentials: Arc<Credentials>,
    config: Arc<QuickConnectConfig>,
    http: Client,
    token: Arc<Mutex<TokenState>>,
}

impl QuickConnectClient {
    /// Build a client with TLS verification enabled and redirects disabled.
    pub fn new(credentials: Credentials, config: QuickConnectConfig) -> Result<Self, ClientError> {
        let http = Client::builder()
            .redirect(Policy::none())
            .timeout(config.timeout)
            .build()
            .map_err(|_| ClientError::InvalidEndpoint)?;
        Ok(Self {
            credentials: Arc::new(credentials),
            config: Arc::new(config),
            http,
            token: Arc::new(Mutex::new(TokenState::default())),
        })
    }

    /// Read the device list response without interpreting provider-specific records.
    pub async fn list_devices(&self) -> Result<Value, ClientError> {
        validate_inventory_response(self.get_json("device/deviceList").await?)
    }

    /// Read one device detail record using its provider ID as an encoded query value.
    pub async fn device_detail(&self, provider_id: &str) -> Result<Value, ClientError> {
        let mut url = endpoint(&self.config.device_base_url, "device")?;
        url.query_pairs_mut().append_pair("deviceId", provider_id);
        validate_device_response(self.get_url_with_retry(url).await?)
    }

    /// POST device settings once. Writes are never retried or reauthenticated automatically.
    pub async fn save_device_settings<T: Serialize>(
        &self,
        provider_id: &str,
        body: &T,
    ) -> Result<Value, ClientError> {
        let url = settings_endpoint(&self.config.device_base_url, provider_id)?;
        let token = self.current_token().await?;
        let response = self
            .http
            .post(url)
            .header(reqwest::header::AUTHORIZATION, token.value)
            .json(body)
            .send()
            .await
            .map_err(|_| ClientError::Transport)?;
        if is_unauthorized(response.status()) {
            return Err(ClientError::Authentication);
        }
        response_json(response, self.config.max_response_bytes).await
    }

    /// GET a JSON response from a safe path below the configured device API root.
    pub async fn get_json(&self, path: &str) -> Result<Value, ClientError> {
        self.get_url_with_retry(endpoint(&self.config.device_base_url, path)?)
            .await
    }

    async fn get_url_with_retry(&self, url: Url) -> Result<Value, ClientError> {
        stream::iter(0..=MAX_READ_RETRIES)
            .fold(ReadRetryState::default(), |state, attempt| {
                let url = url.clone();
                async move {
                    if state.result.is_some() {
                        return state;
                    }
                    match self.get_once(&url, !state.reauthenticated).await {
                        Ok(value) => ReadRetryState {
                            result: Some(Ok(value)),
                            ..state
                        },
                        Err(failure) if failure.retryable && attempt < MAX_READ_RETRIES => {
                            sleep(retry_delay(attempt, failure.retry_after)).await;
                            ReadRetryState {
                                reauthenticated: state.reauthenticated || failure.reauthenticated,
                                ..state
                            }
                        }
                        Err(failure) => ReadRetryState {
                            result: Some(Err(failure.error)),
                            ..state
                        },
                    }
                }
            })
            .await
            .result
            .unwrap_or(Err(ClientError::Transport))
    }

    async fn get_once(
        &self,
        url: &Url,
        allow_reauthentication: bool,
    ) -> Result<Value, RequestFailure> {
        let mut token = self
            .current_token()
            .await
            .map_err(RequestFailure::final_error)?;
        let mut response = self.send_get(url, &token.value).await?;
        let mut reauthenticated = false;
        if is_unauthorized(response.status()) {
            if !allow_reauthentication {
                return Err(RequestFailure::final_error(ClientError::Authentication));
            }
            token = self
                .refresh_token(token.generation)
                .await
                .map_err(RequestFailure::after_reauthentication)?;
            reauthenticated = true;
            response =
                self.send_get(url, &token.value)
                    .await
                    .map_err(|failure| RequestFailure {
                        reauthenticated: true,
                        ..failure
                    })?;
            if is_unauthorized(response.status()) {
                return Err(RequestFailure {
                    reauthenticated: true,
                    ..RequestFailure::final_error(ClientError::Authentication)
                });
            }
        }
        let retry_after = retry_after(response.headers());
        response_json(response, self.config.max_response_bytes)
            .await
            .map_err(|error| {
                let mut failure = RequestFailure::from_response_error(error, retry_after);
                failure.reauthenticated = reauthenticated;
                failure
            })
    }

    async fn send_get(&self, url: &Url, token: &str) -> Result<reqwest::Response, RequestFailure> {
        self.http
            .get(url.clone())
            .header(reqwest::header::AUTHORIZATION, token)
            .send()
            .await
            .map_err(|_| RequestFailure::retryable(ClientError::Transport, None))
    }

    async fn current_token(&self) -> Result<TokenSnapshot, ClientError> {
        let mut state = self.token.lock().await;
        if state.value.is_none() {
            let value = self
                .login_with_retry()
                .await
                .map_err(|failure| failure.error)?;
            state.generation = state.generation.saturating_add(1);
            state.value = Some(value);
        }
        state.snapshot().ok_or(ClientError::Authentication)
    }

    async fn refresh_token(
        &self,
        observed_generation: u64,
    ) -> Result<TokenSnapshot, RequestFailure> {
        let mut state = self.token.lock().await;
        if state.generation == observed_generation {
            let value = self.login_with_retry().await?;
            state.generation = state.generation.saturating_add(1);
            state.value = Some(value);
        }
        state
            .snapshot()
            .ok_or_else(|| RequestFailure::final_error(ClientError::Authentication))
    }

    async fn login_with_retry(&self) -> Result<String, RequestFailure> {
        stream::iter(0..=MAX_READ_RETRIES)
            .fold(None, |previous, attempt| async move {
                if previous.is_some() {
                    return previous;
                }
                match self.login_once().await {
                    Ok(token) => Some(Ok(token)),
                    Err(failure) if failure.retryable && attempt < MAX_READ_RETRIES => {
                        sleep(retry_delay(attempt, failure.retry_after)).await;
                        None
                    }
                    Err(failure) => Some(Err(failure)),
                }
            })
            .await
            .unwrap_or_else(|| Err(RequestFailure::final_error(ClientError::Authentication)))
    }

    async fn login_once(&self) -> Result<String, RequestFailure> {
        let url =
            endpoint(&self.config.auth_base_url, "login").map_err(RequestFailure::final_error)?;
        let body = LoginRequest {
            user_name: self.credentials.username.trim(),
            password: STANDARD.encode(self.credentials.password.as_bytes()),
            user_pool_id: USER_POOL_ID,
            user_role: self.credentials.role,
        };
        let response = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|_| RequestFailure::retryable(ClientError::Transport, None))?;
        if response.status().is_client_error() && response.status() != StatusCode::TOO_MANY_REQUESTS
        {
            return Err(RequestFailure::final_error(ClientError::Authentication));
        }
        let retry_after = retry_after(response.headers());
        let payload = response_json(response, self.config.max_response_bytes)
            .await
            .map_err(|error| RequestFailure::from_response_error(error, retry_after))?;
        payload
            .get("responseData")
            .and_then(|data| data.get("idToken"))
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| RequestFailure::final_error(ClientError::Authentication))
    }
}

#[derive(Default)]
struct TokenState {
    generation: u64,
    value: Option<String>,
}

impl TokenState {
    fn snapshot(&self) -> Option<TokenSnapshot> {
        self.value.as_ref().map(|value| TokenSnapshot {
            generation: self.generation,
            value: value.clone(),
        })
    }
}

struct TokenSnapshot {
    generation: u64,
    value: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginRequest<'a> {
    user_name: &'a str,
    password: String,
    user_pool_id: &'static str,
    user_role: AccountRole,
}

struct RequestFailure {
    error: ClientError,
    retryable: bool,
    retry_after: Option<Duration>,
    reauthenticated: bool,
}

#[derive(Default)]
struct ReadRetryState {
    result: Option<Result<Value, ClientError>>,
    reauthenticated: bool,
}

impl RequestFailure {
    fn final_error(error: ClientError) -> Self {
        Self {
            error,
            retryable: false,
            retry_after: None,
            reauthenticated: false,
        }
    }

    fn retryable(error: ClientError, retry_after: Option<Duration>) -> Self {
        Self {
            error,
            retryable: true,
            retry_after,
            reauthenticated: false,
        }
    }

    fn after_reauthentication(mut failure: Self) -> Self {
        failure.retryable = false;
        failure.reauthenticated = true;
        failure
    }

    fn from_response_error(error: ClientError, retry_after: Option<Duration>) -> Self {
        let retryable = is_transient_error(&error);
        Self {
            error,
            retryable,
            retry_after,
            reauthenticated: false,
        }
    }
}

fn is_transient_error(error: &ClientError) -> bool {
    match error {
        ClientError::Transport | ClientError::HttpStatus(429) => true,
        ClientError::HttpStatus(status) => (500..=599).contains(status),
        _ => false,
    }
}

async fn response_json(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Value, ClientError> {
    let status = response.status();
    let bytes = response
        .bytes_stream()
        .map_err(|_| ClientError::Transport)
        .try_fold(BytesMut::new(), |mut body, chunk| async move {
            if body.len().saturating_add(chunk.len()) > max_bytes {
                return Err(ClientError::ResponseTooLarge(max_bytes));
            }
            body.extend_from_slice(&chunk);
            Ok(body)
        })
        .await?;
    let payload = serde_json::from_slice::<Value>(&bytes).map_err(|_| {
        if status.is_success() {
            ClientError::InvalidJson
        } else {
            ClientError::HttpStatus(status.as_u16())
        }
    })?;
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Err(ClientError::HttpStatus(status.as_u16()));
    }
    if let Some(service_status @ 1..) = payload.get("statusCode").and_then(Value::as_u64)
        && service_status != u64::from(status.as_u16())
    {
        return Err(ClientError::ServiceStatus(service_status));
    }
    if !status.is_success() {
        return Err(ClientError::HttpStatus(status.as_u16()));
    }
    if !payload.is_object() {
        return Err(ClientError::InvalidEnvelope);
    }
    Ok(payload)
}

fn endpoint(base: &Url, path: &str) -> Result<Url, ClientError> {
    if path.starts_with('/')
        || path.chars().next().is_some_and(char::is_whitespace)
        || path.contains('\\')
        || path
            .chars()
            .any(|character| character == '%' || character == '?' || character == '#')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|segment| segment == ".." || segment.contains(':'))
    {
        return Err(ClientError::InvalidEndpoint);
    }
    let joined = base.join(path).map_err(|_| ClientError::InvalidEndpoint)?;
    let root = base.path();
    if joined.origin() != base.origin() || !joined.path().starts_with(root) {
        return Err(ClientError::InvalidEndpoint);
    }
    Ok(joined)
}

fn settings_endpoint(base: &Url, provider_id: &str) -> Result<Url, ClientError> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|_| ClientError::InvalidEndpoint)?
        .pop_if_empty()
        .push("deviceMode")
        .push(provider_id);
    Ok(url)
}

fn validate_inventory_response(payload: Value) -> Result<Value, ClientError> {
    let inventory = payload
        .get("responseData")
        .ok_or(ClientError::InvalidEnvelope)?;
    if inventory.is_array() || inventory.get("devices").is_some_and(Value::is_array) {
        Ok(payload)
    } else {
        Err(ClientError::InvalidEnvelope)
    }
}

fn validate_device_response(payload: Value) -> Result<Value, ClientError> {
    if payload.get("responseData").is_some_and(Value::is_object) {
        Ok(payload)
    } else {
        Err(ClientError::InvalidEnvelope)
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, SystemTime::now()))
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    value
        .parse::<u64>()
        .map(Duration::from_secs)
        .ok()
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()?
                .duration_since(now)
                .ok()
        })
}

fn retry_delay(attempt: u8, retry_after: Option<Duration>) -> Duration {
    retry_after.map_or_else(
        || Duration::from_millis(100 * 2_u64.pow(u32::from(attempt))),
        |delay| delay.min(MAX_RETRY_AFTER),
    )
}

fn is_unauthorized(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_bounded_and_invalid_values_use_backoff() {
        assert_eq!(
            retry_delay(0, Some(Duration::from_secs(100))),
            MAX_RETRY_AFTER
        );
        assert_eq!(retry_delay(1, None), Duration::from_millis(200));
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
    }

    #[test]
    fn retry_after_accepts_http_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let retry_at = now + Duration::from_secs(100);
        let value = httpdate::fmt_http_date(retry_at);
        assert_eq!(
            parse_retry_after(&value, now),
            Some(Duration::from_secs(100))
        );
    }

    #[test]
    fn supported_inventory_and_detail_envelopes_are_accepted() {
        assert!(validate_inventory_response(serde_json::json!({"responseData": []})).is_ok());
        assert!(
            validate_inventory_response(serde_json::json!({"responseData": {"devices": []}}))
                .is_ok()
        );
        assert!(validate_device_response(serde_json::json!({"responseData": {}})).is_ok());
        assert_eq!(
            validate_device_response(serde_json::json!({"responseData": []})).unwrap_err(),
            ClientError::InvalidEnvelope
        );
    }

    #[test]
    fn production_endpoints_match_the_pinned_reference() {
        let config = QuickConnectConfig::production().unwrap();
        assert_eq!(
            config.auth_base_url.as_str(),
            "https://gaf-coreservices.aurai.io/cognito/"
        );
        assert_eq!(
            config.device_base_url.as_str(),
            "https://gaf.keenhome.io/gaf/"
        );
        assert_eq!(config.timeout, Duration::from_secs(20));
        assert_eq!(config.max_response_bytes, 2 * 1024 * 1024);
    }
}
