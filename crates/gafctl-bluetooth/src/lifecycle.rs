use std::{future::Future, time::Duration};

use anyhow::{Context, Result};
use backon::{BackoffBuilder, ExponentialBuilder};
use btleplug::{
    api::{Central as _, Peripheral as _},
    platform::{Adapter, Peripheral},
};
use tokio::time::{Instant, sleep, timeout};

use crate::{
    DisconnectOutcome, ProbeError,
    error::{CleanupFailed, cleanup_is_complete},
};

const CONNECTION_RECOVERY_BUDGET: Duration = Duration::from_secs(20);

pub(super) fn platform_timeout(response_timeout: Duration) -> Duration {
    // BlueZ allows 30 seconds for Connect, then five for service resolution.
    response_timeout.max(Duration::from_secs(40))
}

fn retry_fits_recovery_budget(elapsed: Duration, delay: Duration) -> bool {
    elapsed
        .checked_add(delay)
        .is_some_and(|next_attempt| next_attempt < CONNECTION_RECOVERY_BUDGET)
}

pub(super) async fn retry_connection<T, F, Fut>(mut connect: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let started = Instant::now();
    let mut attempts = 1;
    let mut failure = match connect().await {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };

    for delay in ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(500))
        .with_factor(2.0)
        .with_max_times(4)
        .with_jitter()
        .build()
    {
        if !ProbeError::is_transient_connect_failure(&failure)
            || !retry_fits_recovery_budget(started.elapsed(), delay)
        {
            break;
        }
        tracing::info!(
            attempts,
            retry_delay_seconds = delay.as_secs_f64(),
            error = %format_args!("{failure:#}"),
            "Retrying BLE connection after cleanup"
        );
        sleep(delay).await;
        if started.elapsed() >= CONNECTION_RECOVERY_BUDGET {
            break;
        }
        attempts += 1;
        match connect().await {
            Ok(value) => {
                tracing::info!(attempts, "BLE connection recovered");
                return Ok(value);
            }
            Err(error) => failure = error,
        }
    }
    Err(failure).with_context(|| format!("BLE connection failed after {attempts} attempts"))
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
    .or_else(|error| {
        if cleanup_is_complete(&error) {
            Ok(())
        } else {
            Err(error)
        }
    })
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
                .or_else(|error| {
                    if cleanup_is_complete(&error) {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })?;
            if connected_or_absent(peripheral).await? {
                anyhow::bail!("GAF BLE peripheral remains connected after disconnect");
            }
            Ok(())
        },
    )
    .await
}

pub(super) async fn recover_disconnect(
    peripheral: &Peripheral,
    operation_timeout: Duration,
) -> Result<()> {
    complete_before(operation_timeout, "recover BLE disconnect", async {
        if connected_or_absent(peripheral).await? {
            disconnect_peripheral(peripheral, operation_timeout).await?;
        }
        Ok(())
    })
    .await
}

async fn connected_or_absent(peripheral: &Peripheral) -> Result<bool> {
    peripheral
        .is_connected()
        .await
        .context("check BLE connection")
        .or_else(|error| {
            if cleanup_is_complete(&error) {
                Ok(false)
            } else {
                Err(error)
            }
        })
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
    use std::cell::{Cell, RefCell};

    use super::*;
    use futures_util::future;

    #[test]
    fn platform_deadline_allows_connect_and_service_resolution() {
        assert!(platform_timeout(Duration::from_secs(3)) >= Duration::from_secs(35));
        assert_eq!(
            platform_timeout(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

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
        let operation = anyhow::anyhow!("query failed").context("query context");
        let operation_source = operation.as_ref() as *const dyn std::error::Error;
        let error = finish_with_cleanup::<()>(
            Err(operation),
            Err(anyhow::anyhow!("disconnect failed").context("cleanup context")),
        )
        .expect_err("both failures should remain visible");

        assert_eq!(
            error.to_string(),
            "operation failed (query context: query failed); cleanup also failed (cleanup context: disconnect failed)"
        );
        let composite = error.downcast_ref::<CleanupFailed>().unwrap();
        let source = std::error::Error::source(composite).unwrap();
        assert!(std::ptr::eq(source, operation_source));
        assert_eq!(source.to_string(), "query context");
        assert_eq!(source.source().unwrap().to_string(), "query failed");
    }

    #[test]
    fn retry_delay_must_leave_time_to_start_within_the_recovery_budget() {
        assert!(retry_fits_recovery_budget(
            Duration::from_secs(18),
            Duration::from_secs(1)
        ));
        assert!(!retry_fits_recovery_budget(
            Duration::from_secs(19),
            Duration::from_secs(1)
        ));
        assert!(!retry_fits_recovery_budget(
            Duration::MAX,
            Duration::from_secs(1)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn connection_retry_recovers_when_transport_needs_seconds_to_settle() {
        let started = tokio::time::Instant::now();
        let attempts = Cell::new(0);
        let recovered = retry_connection(|| {
            attempts.set(attempts.get() + 1);
            async {
                if started.elapsed() < Duration::from_secs(2) {
                    Err(anyhow::Error::new(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "le-connection-abort-by-local",
                    )))
                } else {
                    Ok("connected")
                }
            }
        })
        .await
        .expect("a transient episode should recover within one poll");

        assert_eq!(recovered, "connected");
        assert!((3..=4).contains(&attempts.get()));
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert!(started.elapsed() <= Duration::from_secs(7));
    }

    #[tokio::test(start_paused = true)]
    async fn connection_retry_stops_starting_attempts_after_recovery_budget() {
        let attempts = Cell::new(0);
        let started = tokio::time::Instant::now();
        let error = retry_connection(|| {
            attempts.set(attempts.get() + 1);
            async {
                sleep(Duration::from_secs(21)).await;
                Err::<(), _>(anyhow::Error::new(btleplug::Error::TimedOut(
                    Duration::from_secs(21),
                )))
            }
        })
        .await
        .expect_err("a slow failure must not start another connection attempt");
        assert!(ProbeError::is_transient_connect_failure(&error));
        assert_eq!(attempts.get(), 1);
        assert_eq!(started.elapsed(), Duration::from_secs(21));
    }

    #[tokio::test(start_paused = true)]
    async fn connection_retry_keeps_success_after_recovery_budget_expires() {
        let attempts = Cell::new(0);
        let started = Instant::now();
        let recovered = retry_connection(|| {
            let attempt = attempts.get() + 1;
            attempts.set(attempt);
            async move {
                if attempt == 1 {
                    Err(anyhow::Error::new(btleplug::Error::TimedOut(
                        Duration::ZERO,
                    )))
                } else {
                    sleep(Duration::from_secs(21)).await;
                    Ok("connected")
                }
            }
        })
        .await
        .expect("an admitted connection is allowed to finish");

        assert_eq!(recovered, "connected");
        assert_eq!(attempts.get(), 2);
        assert!(started.elapsed() > CONNECTION_RECOVERY_BUDGET);
    }

    #[tokio::test(start_paused = true)]
    async fn connection_retry_does_not_retry_authentication_or_protocol_errors() {
        let attempts = Cell::new(0);
        let started = tokio::time::Instant::now();
        let error = retry_connection(|| {
            attempts.set(attempts.get() + 1);
            async { Err::<(), _>(anyhow::Error::new(btleplug::Error::PermissionDenied)) }
        })
        .await
        .expect_err("authentication failure is final");
        assert_eq!(
            ProbeError::classify(error).kind(),
            crate::ProbeErrorKind::Authentication
        );
        assert_eq!(attempts.get(), 1);
        assert_eq!(started.elapsed(), Duration::ZERO);

        let error = retry_connection(|| {
            attempts.set(attempts.get() + 1);
            async { Err::<(), _>(anyhow::Error::new(crate::error::InvalidIdentityResponse)) }
        })
        .await
        .expect_err("protocol failure is final");
        assert_eq!(
            ProbeError::classify(error).kind(),
            crate::ProbeErrorKind::Protocol
        );
        assert_eq!(attempts.get(), 2);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
    async fn connection_retry_jitter_varies_between_recovery_episodes() {
        let mut schedules = Vec::new();
        for _ in 0..2 {
            let started = Instant::now();
            let attempts = RefCell::new(Vec::new());
            let error = retry_connection(|| {
                attempts.borrow_mut().push(started.elapsed());
                async {
                    Err::<(), _>(anyhow::Error::new(btleplug::Error::TimedOut(
                        Duration::ZERO,
                    )))
                }
            })
            .await
            .expect_err("each recovery episode exhausts its transient retries");
            assert!(ProbeError::is_transient_connect_failure(&error));
            let attempts = attempts.into_inner();
            assert_eq!(attempts.len(), 5);
            schedules.push(attempts);
        }
        assert_ne!(schedules[0], schedules[1]);
    }

    #[tokio::test(start_paused = true)]
    async fn connection_retry_stops_after_the_finite_attempt_budget() {
        let attempts = Cell::new(0);
        let started = Instant::now();
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
        assert_eq!(attempts.get(), 5);
        assert!(started.elapsed() >= Duration::from_millis(7_500));
        assert!(started.elapsed() <= Duration::from_secs(15));
        assert!(error.to_string().contains("after 5 attempts"));
    }

    #[tokio::test(start_paused = true)]
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
        assert!(!ProbeError::is_transient_connect_failure(&error));
        assert_eq!(attempts.get(), 1);
    }
}
