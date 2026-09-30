use std::{future::Future, time::Duration};

use anyhow::{Context, Result};
use btleplug::{api::Central as _, platform::Adapter};
#[cfg(not(target_os = "linux"))]
use btleplug::{api::Peripheral as _, platform::Peripheral};
use tokio::{
    runtime::Handle,
    time::{sleep, timeout},
};

use crate::{DisconnectOutcome, ProbeError, error::CleanupFailed};

const CONNECTION_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(300)];

pub(super) const fn connection_retry_delay(retry_index: usize) -> Option<Duration> {
    if retry_index < CONNECTION_RETRY_DELAYS.len() {
        Some(CONNECTION_RETRY_DELAYS[retry_index])
    } else {
        None
    }
}

pub(super) async fn retry_connection<T, F, Fut>(mut connect: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut retry_index = 0;
    loop {
        match connect().await {
            Ok(value) => return Ok(value),
            Err(error)
                if ProbeError::is_transient_connect_failure(&error)
                    && let Some(delay) = connection_retry_delay(retry_index) =>
            {
                sleep(delay).await;
                retry_index += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) struct ScanCleanup {
    adapter: Adapter,
    operation_timeout: Duration,
    runtime: Handle,
    armed: bool,
}

impl ScanCleanup {
    pub(super) fn new(adapter: Adapter, operation_timeout: Duration) -> Self {
        Self {
            adapter,
            operation_timeout,
            runtime: Handle::current(),
            armed: true,
        }
    }

    pub(super) async fn run(&mut self) -> Result<()> {
        stop_ble_scan(&self.adapter, self.operation_timeout).await?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for ScanCleanup {
    fn drop(&mut self) {
        if self.armed {
            let adapter = self.adapter.clone();
            let operation_timeout = self.operation_timeout;
            self.runtime.spawn(async move {
                report_cleanup_failure(
                    "stop BLE scan",
                    stop_ble_scan(&adapter, operation_timeout).await,
                );
            });
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) struct DisconnectCleanup {
    peripheral: Peripheral,
    operation_timeout: Duration,
    runtime: Handle,
    armed: bool,
}

#[cfg(not(target_os = "linux"))]
impl DisconnectCleanup {
    pub(super) fn new(peripheral: Peripheral, operation_timeout: Duration) -> Self {
        Self {
            peripheral,
            operation_timeout,
            runtime: Handle::current(),
            armed: true,
        }
    }

    pub(super) async fn run(&mut self) -> Result<()> {
        disconnect_peripheral(&self.peripheral, self.operation_timeout).await?;
        self.armed = false;
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
impl Drop for DisconnectCleanup {
    fn drop(&mut self) {
        if self.armed {
            let peripheral = self.peripheral.clone();
            let operation_timeout = self.operation_timeout;
            self.runtime.spawn(async move {
                report_cleanup_failure(
                    "disconnect from GAF BLE peripheral",
                    disconnect_peripheral(&peripheral, operation_timeout).await,
                );
            });
        }
    }
}

fn report_cleanup_failure(operation: &'static str, result: Result<()>) {
    if let Err(error) = result {
        tracing::warn!(operation, %error, "best-effort BLE cleanup failed");
    }
}

pub(super) async fn complete_before<T>(
    duration: Duration,
    operation: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    timeout(duration, future)
        .await
        .with_context(|| format!("{operation} timed out"))?
}

pub(super) async fn stop_ble_scan(adapter: &Adapter, operation_timeout: Duration) -> Result<()> {
    complete_before(operation_timeout, "stop BLE scan", async {
        adapter.stop_scan().await.context("stop BLE scan")
    })
    .await
}

#[cfg(not(target_os = "linux"))]
pub(super) async fn disconnect_peripheral(
    peripheral: &Peripheral,
    operation_timeout: Duration,
) -> Result<()> {
    complete_before(
        operation_timeout,
        "disconnect from GAF BLE peripheral",
        async {
            peripheral
                .disconnect()
                .await
                .context("disconnect from GAF BLE peripheral")
        },
    )
    .await
}

pub(super) fn finish_with_cleanup<T>(
    operation: Result<T>,
    cleanup: Result<()>,
) -> Result<(T, DisconnectOutcome)> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok((value, DisconnectOutcome::Disconnected)),
        (Ok(value), Err(error)) => Ok((value, DisconnectOutcome::Failed(format!("{error:#}")))),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(anyhow::Error::new(CleanupFailed {
            operation: error,
            cleanup: cleanup_error,
        })),
    }
}

pub(super) fn fail_with_cleanup<T>(operation: anyhow::Error, cleanup: Result<()>) -> Result<T> {
    match cleanup {
        Ok(()) => Err(operation),
        Err(cleanup_error) => Err(anyhow::Error::new(CleanupFailed {
            operation,
            cleanup: cleanup_error,
        })),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use futures_util::future;

    #[tokio::test]
    async fn platform_operation_deadline_names_the_timed_out_operation() {
        let error = complete_before(
            Duration::ZERO,
            "test BLE operation",
            future::pending::<Result<()>>(),
        )
        .await
        .expect_err("pending operation should time out");

        assert!(format!("{error:#}").contains("test BLE operation timed out"));
    }

    #[test]
    fn successful_operation_keeps_its_value_when_cleanup_fails() {
        let (value, disconnect) = finish_with_cleanup(Ok(42), Err(anyhow::anyhow!("disconnect")))
            .expect("query result is retained");

        assert_eq!(value, 42);
        assert_eq!(
            disconnect,
            DisconnectOutcome::Failed("disconnect".to_owned())
        );
    }

    #[test]
    fn operation_and_cleanup_errors_are_both_reported() {
        let error = finish_with_cleanup::<()>(
            Err(anyhow::anyhow!("query failed")),
            Err(anyhow::anyhow!("disconnect failed")),
        )
        .expect_err("both failures should remain visible");

        assert!(error.to_string().contains("query failed"));
        assert!(error.to_string().contains("disconnect failed"));
    }

    #[test]
    fn connection_retry_delays_are_increasing_and_finite() {
        assert_eq!(connection_retry_delay(0), Some(Duration::from_millis(100)));
        assert_eq!(connection_retry_delay(1), Some(Duration::from_millis(300)));
        assert_eq!(connection_retry_delay(2), None);
    }

    #[tokio::test]
    async fn connection_retry_recovers_after_transient_failures() {
        let attempts = Cell::new(0);
        let connected = retry_connection(|| {
            let attempt = attempts.get() + 1;
            attempts.set(attempt);
            async move {
                if attempt < 3 {
                    Err(anyhow::Error::new(btleplug::Error::TimedOut(
                        Duration::from_secs(3),
                    )))
                } else {
                    Ok(attempt)
                }
            }
        })
        .await
        .expect("connection eventually succeeds");

        assert_eq!(connected, 3);
        assert_eq!(attempts.get(), 3);
    }

    #[tokio::test]
    async fn connection_retry_stops_after_the_finite_attempt_budget() {
        let attempts = Cell::new(0);
        let error = retry_connection(|| {
            attempts.set(attempts.get() + 1);
            async {
                Err::<usize, anyhow::Error>(anyhow::Error::new(btleplug::Error::TimedOut(
                    Duration::from_secs(3),
                )))
            }
        })
        .await
        .expect_err("transient failure remains an error after retries");

        assert!(ProbeError::is_transient_connect_failure(&error));
        assert_eq!(attempts.get(), 3);
    }

    #[tokio::test]
    async fn connection_retry_stops_when_cleanup_fails() {
        let attempts = Cell::new(0);
        let error = retry_connection(|| {
            attempts.set(attempts.get() + 1);
            async {
                fail_with_cleanup::<()>(
                    anyhow::Error::new(btleplug::Error::TimedOut(Duration::from_secs(3))),
                    Err(anyhow::anyhow!("disconnect failed")),
                )
            }
        })
        .await
        .expect_err("cleanup failure prevents reconnecting");

        assert!(format!("{error:#}").contains("disconnect failed"));
        assert_eq!(attempts.get(), 1);
    }
}
