use std::{future, sync::OnceLock, time::Duration};

use anyhow::{Context, Result, bail};
use bluez_async::{
    BluetoothEvent, BluetoothSession, CharacteristicEvent, CharacteristicFlags, CharacteristicInfo,
    DeviceId, WriteOptions, WriteType,
};
use btleplug::{api::Peripheral as _, platform::Peripheral};
use futures_util::StreamExt;
use tokio::runtime::Handle;
use updraft_protocol::ControlCommand;

use super::RuntimeSessionCache;
use super::request_session::{GattTransport, RequestSession};
use crate::{
    GAF_CHARACTERISTIC_UUID, GAF_SERVICE_UUID, QueryResult,
    lifecycle::{complete_before, fail_with_cleanup, finish_with_cleanup},
};

struct ConnectedDevice {
    session: BluetoothSession,
    device: DeviceId,
    operation_timeout: Duration,
    runtime: Handle,
    cleanup_armed: bool,
}

static BLUEZ_SESSIONS: OnceLock<RuntimeSessionCache<BluetoothSession>> = OnceLock::new();

struct BluezTransport<'a> {
    connected: &'a ConnectedDevice,
    characteristic: CharacteristicInfo,
    write_type: WriteType,
}

pub async fn query_peripheral(
    peripheral: &Peripheral,
    response_timeout: Duration,
    control_command: Option<ControlCommand>,
) -> Result<QueryResult> {
    let mut connected = ConnectedDevice::connect(peripheral, response_timeout).await?;
    let query = async {
        let request_session = connected.request_session(response_timeout).await?;
        request_session.query(control_command).await
    }
    .await;
    let (mut result, disconnect) = finish_with_cleanup(query, connected.disconnect().await)?;
    result.disconnect = disconnect;
    Ok(result)
}

impl ConnectedDevice {
    async fn connect(peripheral: &Peripheral, operation_timeout: Duration) -> Result<Self> {
        let path = format!("/org/bluez/{}", peripheral.id());
        let device = serde_json::from_value(serde_json::json!({ "object_path": path }))
            .context("parse BlueZ device path")?;
        let runtime = Handle::current();
        let session = BLUEZ_SESSIONS
            .get_or_init(RuntimeSessionCache::default)
            .get_or_init(runtime.id(), || async {
                let (dbus_task, session) = BluetoothSession::new()
                    .await
                    .context("create BlueZ D-Bus session")?;
                let driver = tokio::spawn(async move {
                    if let Err(error) = dbus_task.await {
                        tracing::warn!(%error, "BlueZ D-Bus connection ended");
                    }
                });
                Ok((session, driver))
            })
            .await?;

        let mut connected = Self {
            session,
            device,
            operation_timeout,
            runtime,
            cleanup_armed: true,
        };
        let result = complete_before(operation_timeout, "connect to GAF BLE peripheral", async {
            connected
                .session
                .connect_with_timeout(&connected.device, operation_timeout)
                .await
                .context("connect to GAF BLE peripheral")
        })
        .await;
        if let Err(error) = result {
            return fail_with_cleanup(error, connected.disconnect().await);
        }
        Ok(connected)
    }

    async fn request_session(
        &self,
        response_timeout: Duration,
    ) -> Result<RequestSession<BluezTransport<'_>>> {
        let service = complete_before(response_timeout, "discover GAF BLE service", async {
            self.session
                .get_service_by_uuid(&self.device, GAF_SERVICE_UUID)
                .await
                .context("discover GAF BLE service 00FF")
        })
        .await?;
        let characteristic =
            complete_before(response_timeout, "discover GAF BLE characteristic", async {
                self.session
                    .get_characteristic_by_uuid(&service.id, GAF_CHARACTERISTIC_UUID)
                    .await
                    .context("discover GAF BLE characteristic FF01")
            })
            .await?;
        let write_type = write_type(&characteristic)?;

        let events = complete_before(response_timeout, "subscribe to BLE notifications", async {
            self.session
                .device_event_stream(&self.device)
                .await
                .context("subscribe to BLE notifications")
        })
        .await?;
        let characteristic_id = characteristic.id.clone();
        let notifications = events
            .filter_map(move |event| {
                future::ready(match event {
                    BluetoothEvent::Characteristic {
                        id,
                        event: CharacteristicEvent::Value { value },
                    } if id == characteristic_id => Some(value),
                    _ => None,
                })
            })
            .boxed();

        complete_before(response_timeout, "enable GAF characteristic FF01", async {
            self.session
                .start_notify(&characteristic.id)
                .await
                .context("enable responses on GAF characteristic FF01")
        })
        .await?;

        Ok(RequestSession::new(
            BluezTransport {
                connected: self,
                characteristic,
                write_type,
            },
            notifications,
            response_timeout,
        ))
    }

    async fn disconnect(&mut self) -> Result<()> {
        let result = complete_before(
            self.operation_timeout,
            "disconnect from GAF BLE peripheral",
            async {
                self.session
                    .disconnect(&self.device)
                    .await
                    .context("disconnect from GAF BLE peripheral")
            },
        )
        .await;
        if result.is_ok() {
            self.cleanup_armed = false;
        }
        result
    }
}

impl Drop for ConnectedDevice {
    fn drop(&mut self) {
        if self.cleanup_armed {
            let session = self.session.clone();
            let device = self.device.clone();
            let operation_timeout = self.operation_timeout;
            self.runtime.spawn(async move {
                let result = complete_before(
                    operation_timeout,
                    "disconnect from GAF BLE peripheral",
                    async {
                        session
                            .disconnect(&device)
                            .await
                            .context("disconnect from GAF BLE peripheral")
                    },
                )
                .await;
                if let Err(error) = result {
                    tracing::warn!(%error, "best-effort BLE disconnect failed");
                }
            });
        }
    }
}

impl GattTransport for BluezTransport<'_> {
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.connected
            .session
            .write_characteristic_value_with_options(
                &self.characteristic.id,
                bytes.to_vec(),
                WriteOptions {
                    write_type: Some(self.write_type),
                    ..WriteOptions::default()
                },
            )
            .await
            .context("write GAF BLE characteristic")
    }
}

fn write_type(characteristic: &CharacteristicInfo) -> Result<WriteType> {
    match (
        characteristic.flags.contains(CharacteristicFlags::WRITE),
        characteristic
            .flags
            .contains(CharacteristicFlags::WRITE_WITHOUT_RESPONSE),
    ) {
        (true, _) => Ok(WriteType::WithResponse),
        (false, true) => Ok(WriteType::WithoutResponse),
        (false, false) => bail!("GAF characteristic FF01 does not permit writes"),
    }
}
