use std::{future, pin::Pin, time::Duration};

use anyhow::{Context, Result};
use btleplug::api::{CharPropFlags, Characteristic, Peripheral as _, ValueNotification, WriteType};
use btleplug::platform::Peripheral;
use futures_util::{FutureExt, Stream, StreamExt, stream::FilterMap};

use super::request_session::{GattTransport, RequestSession};
use crate::bluetooth::{
    GAF_CHARACTERISTIC_UUID, GAF_SERVICE_UUID,
    lifecycle::{complete_before, disconnect_peripheral, platform_timeout, retry_connection},
};

type Notifications = FilterMap<
    Pin<Box<dyn Stream<Item = ValueNotification> + Send>>,
    future::Ready<Option<Vec<u8>>>,
    fn(ValueNotification) -> future::Ready<Option<Vec<u8>>>,
>;
pub(crate) type BtleplugSession = RequestSession<BtleplugTransport, Notifications>;

pub(crate) struct BtleplugTransport {
    peripheral: Peripheral,
    characteristic: Characteristic,
}

pub(crate) async fn open_session(
    peripheral: &Peripheral,
    response_timeout: Duration,
) -> Result<BtleplugSession> {
    let mut attempted = false;
    retry_connection(|| {
        let retry = std::mem::replace(&mut attempted, true);
        async move {
            if retry {
                disconnect_peripheral(peripheral, platform_timeout(response_timeout))
                    .await
                    .map_err(|cleanup| {
                        anyhow::Error::new(crate::bluetooth::error::CleanupFailed {
                            operation: anyhow::anyhow!(
                                "clean previous failed BLE connection before retry"
                            ),
                            cleanup,
                        })
                    })?;
            }
            connect(peripheral, platform_timeout(response_timeout)).await
        }
    })
    .await?;
    let characteristic = writable_characteristic(peripheral, response_timeout).await?;
    let notifications = complete_before(response_timeout, "register BLE notifications", async {
        peripheral
            .notifications()
            .await
            .context("register BLE notifications")
    })
    .await?;
    complete_before(response_timeout, "enable GAF characteristic FF01", async {
        peripheral
            .subscribe(&characteristic)
            .await
            .context("enable GAF responses")
    })
    .await?;
    complete_before(
        response_timeout,
        "read initial GAF characteristic FF01",
        async {
            peripheral
                .read(&characteristic)
                .await
                .context("read initial GAF characteristic FF01")
        },
    )
    .await?;
    let mut notifications = notifications.filter_map(gaf_notification as fn(_) -> _);
    discard_startup_notifications(&mut notifications)?;
    Ok(RequestSession::new(
        BtleplugTransport {
            peripheral: peripheral.clone(),
            characteristic,
        },
        notifications,
        response_timeout,
    ))
}

async fn connect(peripheral: &Peripheral, operation_timeout: Duration) -> Result<()> {
    complete_before(operation_timeout, "connect to GAF BLE peripheral", async {
        peripheral
            .connect()
            .await
            .context("connect to GAF BLE peripheral")
    })
    .await
}

fn discard_startup_notifications(notifications: &mut Notifications) -> Result<()> {
    for _ in 0..64 {
        match notifications.next().now_or_never() {
            None => return Ok(()),
            Some(None) => anyhow::bail!("BLE notification stream ended during initialization"),
            Some(Some(_)) => {}
        }
    }
    anyhow::bail!("BLE notification stream flooded during initialization")
}

fn gaf_notification(notification: ValueNotification) -> future::Ready<Option<Vec<u8>>> {
    future::ready(
        notification_matches(notification.service_uuid, notification.uuid)
            .then_some(notification.value),
    )
}

fn notification_matches(service_uuid: uuid::Uuid, characteristic_uuid: uuid::Uuid) -> bool {
    service_uuid == GAF_SERVICE_UUID && characteristic_uuid == GAF_CHARACTERISTIC_UUID
}

async fn writable_characteristic(
    peripheral: &Peripheral,
    operation_timeout: Duration,
) -> Result<Characteristic> {
    complete_before(operation_timeout, "discover GAF BLE services", async {
        peripheral
            .discover_services()
            .await
            .context("discover GAF BLE services")
    })
    .await?;
    let characteristic = peripheral
        .services()
        .into_iter()
        .flat_map(|service| service.characteristics)
        .find(|characteristic| {
            characteristic.service_uuid == GAF_SERVICE_UUID
                && characteristic.uuid == GAF_CHARACTERISTIC_UUID
        })
        .context("GAF service 00FF has no characteristic FF01")?;
    let required = CharPropFlags::READ | CharPropFlags::WRITE | CharPropFlags::NOTIFY;
    if !characteristic.properties.contains(required) {
        anyhow::bail!(
            "GAF characteristic FF01 must support reads, writes with response, and notifications"
        );
    }
    Ok(characteristic)
}

impl GattTransport for BtleplugTransport {
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.peripheral
            .write(&self.characteristic, bytes, WriteType::WithResponse)
            .await
            .context("write GAF BLE characteristic")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_matching_requires_gaf_service_and_characteristic() {
        assert!(notification_matches(
            GAF_SERVICE_UUID,
            GAF_CHARACTERISTIC_UUID,
        ));
        assert!(!notification_matches(
            uuid::Uuid::nil(),
            GAF_CHARACTERISTIC_UUID,
        ));
        assert!(!notification_matches(GAF_SERVICE_UUID, uuid::Uuid::nil(),));
    }
}
