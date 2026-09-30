use std::{future, time::Duration};

use anyhow::{Context, Result, bail};
use btleplug::api::{CharPropFlags, Characteristic, Peripheral as _, ValueNotification, WriteType};
use btleplug::platform::Peripheral;
use futures_util::StreamExt;
use updraft_protocol::ControlCommand;

use super::request_session::{GattTransport, RequestSession};
use crate::{
    GAF_CHARACTERISTIC_UUID, GAF_SERVICE_UUID, QueryResult,
    lifecycle::{
        DisconnectCleanup, complete_before, fail_with_cleanup, finish_with_cleanup,
        retry_connection,
    },
};

struct ConnectedPeripheral<'a> {
    peripheral: &'a Peripheral,
    cleanup: DisconnectCleanup,
}

struct BtleplugTransport<'a> {
    peripheral: &'a Peripheral,
    characteristic: Characteristic,
    write_type: WriteType,
}

pub async fn query_peripheral(
    peripheral: &Peripheral,
    response_timeout: Duration,
    control_command: Option<ControlCommand>,
) -> Result<QueryResult> {
    let mut connected =
        retry_connection(|| ConnectedPeripheral::connect(peripheral, response_timeout)).await?;
    let query = async {
        request_session(&connected, response_timeout)
            .await?
            .query(control_command)
            .await
    }
    .await;
    let (mut result, disconnect) = finish_with_cleanup(query, connected.disconnect().await)?;
    result.disconnect = disconnect;
    Ok(result)
}

impl<'a> ConnectedPeripheral<'a> {
    async fn connect(peripheral: &'a Peripheral, operation_timeout: Duration) -> Result<Self> {
        let mut cleanup = DisconnectCleanup::new(peripheral.clone(), operation_timeout);
        let connection =
            complete_before(operation_timeout, "connect to GAF BLE peripheral", async {
                peripheral
                    .connect()
                    .await
                    .context("connect to GAF BLE peripheral")
            })
            .await;
        match connection {
            Ok(()) => Ok(Self {
                peripheral,
                cleanup,
            }),
            Err(error) => fail_with_cleanup(error, cleanup.run().await),
        }
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.cleanup.run().await
    }
}

async fn request_session<'a, 'device>(
    connected: &'a ConnectedPeripheral<'device>,
    response_timeout: Duration,
) -> Result<RequestSession<BtleplugTransport<'a>>> {
    let (characteristic, write_type) = writable_characteristic(connected, response_timeout).await?;
    let notifications =
        complete_before(response_timeout, "subscribe to BLE notifications", async {
            connected
                .peripheral
                .notifications()
                .await
                .context("subscribe to BLE notifications")
        })
        .await?;
    complete_before(response_timeout, "enable GAF characteristic FF01", async {
        connected
            .peripheral
            .subscribe(&characteristic)
            .await
            .context("enable responses on GAF characteristic FF01")
    })
    .await?;

    let notifications = notifications
        .filter_map(|notification: ValueNotification| {
            future::ready(
                (notification.uuid == GAF_CHARACTERISTIC_UUID).then_some(notification.value),
            )
        })
        .boxed();
    Ok(RequestSession::new(
        BtleplugTransport {
            peripheral: connected.peripheral,
            characteristic,
            write_type,
        },
        notifications,
        response_timeout,
    ))
}

async fn writable_characteristic(
    connected: &ConnectedPeripheral<'_>,
    operation_timeout: Duration,
) -> Result<(Characteristic, WriteType)> {
    complete_before(operation_timeout, "discover GAF BLE services", async {
        connected
            .peripheral
            .discover_services()
            .await
            .context("discover GAF BLE services")
    })
    .await?;
    let characteristic = connected
        .peripheral
        .services()
        .into_iter()
        .flat_map(|service| service.characteristics)
        .find(|characteristic| {
            characteristic.service_uuid == GAF_SERVICE_UUID
                && characteristic.uuid == GAF_CHARACTERISTIC_UUID
        })
        .context("GAF service 00FF has no characteristic FF01")?;

    let write_type = match (
        characteristic.properties.contains(CharPropFlags::WRITE),
        characteristic
            .properties
            .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE),
    ) {
        (true, _) => WriteType::WithResponse,
        (false, true) => WriteType::WithoutResponse,
        (false, false) => bail!("GAF characteristic FF01 does not permit writes"),
    };
    Ok((characteristic, write_type))
}

impl GattTransport for BtleplugTransport<'_> {
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.peripheral
            .write(&self.characteristic, bytes, self.write_type)
            .await
            .context("write GAF BLE characteristic")
    }
}
