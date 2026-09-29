use std::{future::Future, time::Duration};

use anyhow::{Context, Result};
use btleplug::{
    api::{Central as _, Peripheral as _},
    platform::{Adapter, Peripheral},
};
use tokio::{runtime::Handle, time::timeout};

use crate::DisconnectOutcome;

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

pub(super) struct DisconnectCleanup {
    peripheral: Peripheral,
    operation_timeout: Duration,
    runtime: Handle,
    armed: bool,
}

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
        (Err(error), Err(cleanup_error)) => {
            let operation = format!("{error:#}");
            Err(error.context(format!(
                "operation failed ({operation}); disconnect also failed ({cleanup_error:#})"
            )))
        }
    }
}

pub(super) fn fail_with_cleanup<T>(operation: anyhow::Error, cleanup: Result<()>) -> Result<T> {
    match cleanup {
        Ok(()) => Err(operation),
        Err(cleanup_error) => {
            let operation_message = format!("{operation:#}");
            Err(operation.context(format!(
                "operation failed ({operation_message}); cleanup also failed ({cleanup_error:#})"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
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
}
