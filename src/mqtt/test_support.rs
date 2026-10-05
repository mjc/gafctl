use std::{
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime},
};

use crate::service::publication::StateSnapshot;
use futures_util::{StreamExt, future, stream};
use gafctl_api::{
    DeviceInventoryStatus, DeviceSettings, DeviceState, DeviceStateV2Response,
    QuickConnectModeStatus, StateProvenance, unix_millis,
};
use rumqttc_next::{MqttOptions, MqttOptionsBuilder, Publish};
use serde_json::json;
use tokio::{
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::MqttConfig;
pub(super) use super::connection::observed_client;
use gafctl_api::{
    DeviceBackend, DeviceCapabilities, DeviceDescriptor, DeviceId, EntitySource, ProxyId,
};

pub(super) struct NativeBroker {
    process: Child,
    pub(super) port: u16,
}

impl NativeBroker {
    pub(super) async fn restart(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        sleep(Duration::from_millis(1_200)).await;
        self.process = launch_native_broker(self.port);
        wait_for_native_broker(self.port).await;
    }
}

impl Drop for NativeBroker {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

pub(super) async fn start_native_broker() -> NativeBroker {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let broker = NativeBroker {
        process: launch_native_broker(port),
        port,
    };
    wait_for_native_broker(port).await;
    broker
}

fn launch_native_broker(port: u16) -> Child {
    Command::new("mosquitto")
        .args(["-p", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("devenv supplies the native Mosquitto broker")
}

async fn wait_for_native_broker(port: u16) {
    let attempts = stream::iter(0..50)
        .then(|_| async {
            sleep(Duration::from_millis(20)).await;
            TcpStream::connect(("127.0.0.1", port)).await.ok()
        })
        .filter_map(future::ready);
    tokio::pin!(attempts);
    let ready = attempts.next().await;
    assert!(ready.is_some(), "native Mosquitto did not start");
    drop(ready);
}

pub(super) fn test_mqtt_options(client_id: &str, port: u16) -> MqttOptions {
    MqttOptionsBuilder::new(client_id, ("127.0.0.1", port))
        .keep_alive(5)
        .credentials("gafctl-test", "gafctl-test")
        .build()
}

pub(super) async fn receive_topic(
    received: &mut UnboundedReceiverStream<Publish>,
    topic: &str,
) -> Publish {
    let mut messages = received.filter(|message| future::ready(message.topic == topic));
    timeout(Duration::from_secs(15), messages.next())
        .await
        .expect("timed out waiting for MQTT publication")
        .expect("observer stream ended")
}

pub(super) fn mqtt_device(proxy_id: ProxyId, id: &str) -> DeviceDescriptor {
    DeviceDescriptor {
        proxy_id,
        id: DeviceId::parse(id.to_owned()).unwrap(),
        name: format!("Device {id}"),
        backend: DeviceBackend::QuickConnect,
        capabilities: DeviceCapabilities::quickconnect_with_controls(),
        state_source: EntitySource::Mqtt,
        command_source: EntitySource::Mqtt,
    }
}

pub(crate) fn config(port: u16, discovery_enabled: bool) -> MqttConfig {
    MqttConfig {
        host: "127.0.0.1".to_owned(),
        port,
        username: "test".to_owned(),
        password: "test".to_owned(),
        discovery_enabled,
    }
}

pub(super) fn snapshot(device: DeviceDescriptor) -> StateSnapshot {
    StateSnapshot {
        proxy_id: device.proxy_id,
        discovery_identities: vec![(device.id.clone(), device.backend)],
        publications: vec![DeviceStateV2Response {
            id: device.id.clone(),
            backend: device.backend,
            available: true,
            inventory_status: DeviceInventoryStatus::Present,
            last_error: None,
            state: Some(DeviceState {
                temperature_f: None,
                humidity_percent: None,
                settings: DeviceSettings::QuickConnect {
                    mode: QuickConnectModeStatus::Automatic,
                    automatic_temperature_f: None,
                    automatic_humidity_percent: None,
                    timer_duration_minutes: None,
                    humidity_monitor: None,
                },
                estimated_running: None,
                diagnostics: None,
                provenance: StateProvenance {
                    backend: device.backend,
                    fetched_at_unix_ms: None,
                    observed_at_unix_ms: None,
                },
            }),
        }],
        descriptors: vec![device],
    }
}

pub(super) fn request(id: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"request_id": id, "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(), "command": {"kind": "quick_connect_mode", "mode": "automatic"}})).unwrap()
}
