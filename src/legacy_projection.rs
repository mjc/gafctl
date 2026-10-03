use std::time::SystemTime;

use gafctl_api::{
    DeviceBackend, DeviceDiagnostics, DeviceSettings, DeviceState, LegacyMode, StateProvenance,
    unix_millis,
};
use gafctl_protocol::{DeviceSnapshot, FanState, OperatingMode};

pub(crate) fn project_snapshot(snapshot: &DeviceSnapshot) -> DeviceState {
    let mode = snapshot.mode.decoded().ok();
    let sensors = snapshot.sensors.decoded().ok();
    let thresholds = snapshot.thresholds.decoded().ok();
    let timer = snapshot.timer.decoded().ok();
    let firmware = snapshot.identity.decoded().ok().map(|identity| {
        let version = identity.firmware_version;
        format!("{}.{}.{}", version.major, version.minor, version.patch)
    });
    DeviceState {
        temperature_f: sensors.map(|sensors| f64::from(sensors.temperature.value()) / 10.0),
        humidity_percent: sensors.map(|sensors| f64::from(sensors.humidity.value()) / 10.0),
        settings: DeviceSettings::LegacyBle {
            mode: mode.map(|mode| match mode.mode {
                OperatingMode::Automatic => LegacyMode::Automatic,
                OperatingMode::Timer => LegacyMode::Timer,
                OperatingMode::Ota => LegacyMode::Ota,
            }),
            controller_fan_on: mode.map(|mode| mode.fan == FanState::On),
            automatic_temperature_tenths_f: thresholds.map(|value| value.temperature.value()),
            automatic_humidity_tenths_percent: thresholds.map(|value| value.humidity.value()),
            timer_remaining_minutes: timer.map(|value| value.remaining.value()),
            timer_original_minutes: timer.map(|value| value.original.value()),
        },
        estimated_running: None,
        diagnostics: Some(DeviceDiagnostics {
            firmware_version: firmware,
            signal_strength_raw: None,
            verified_raw: None,
            ota_in_progress: None,
        }),
        provenance: StateProvenance {
            backend: DeviceBackend::LegacyBle,
            fetched_at_unix_ms: unix_millis(SystemTime::now()),
            observed_at_unix_ms: unix_millis(snapshot.observed_at),
        },
    }
}
