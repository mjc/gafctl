use std::fmt;

use updraft_bluetooth::{DiscoveredDevice, ProbeResult};
use updraft_protocol::{
    Acknowledgement, ControlOutcome, ControlReadback, DeviceSnapshot, ReadCommand, ReadbackMatch,
};

pub(crate) fn print_probe_result(result: ProbeResult, show_identity: bool) {
    match result {
        ProbeResult::NoDevices => {
            println!("No nearby BLE device advertising GAF service 00FF was found.");
        }
        ProbeResult::Discovered { devices } => {
            print_devices(devices.iter().map(|candidate| candidate.device()));
            println!("Scan-only mode: no connection or protocol request was made.");
        }
        ProbeResult::Ambiguous { devices } => {
            print_devices(devices.iter().map(|candidate| candidate.device()));
            println!(
                "More than one candidate found. Re-run with --device-id <id> to query one fan."
            );
        }
        ProbeResult::Queried { device, result } => {
            println!("Queried GAF BLE device: {}", DeviceDescription(&device));
            if let Some(control) = &result.control {
                print_control_acknowledgement(control);
            }
            print_snapshot(&result.snapshot, show_identity);
            if let Some(control) = &result.control {
                println!("{}", ControlReadbackDisplay(control.readback()));
            }
            if let updraft_bluetooth::DisconnectOutcome::Failed(error) = &result.disconnect {
                eprintln!("BLE query succeeded, but disconnect failed: {error}");
            }
        }
    }
}

fn print_devices<'a>(devices: impl ExactSizeIterator<Item = &'a DiscoveredDevice>) {
    println!("Found {} GAF BLE device(s):", devices.len());
    devices.enumerate().for_each(|(index, device)| {
        println!("  [{index}] {}", DeviceDescription(device));
    });
}

fn print_control_acknowledgement(control: &ControlOutcome) {
    let response = control.frame();
    let acknowledgement = match control.acknowledgement() {
        Acknowledgement::Accepted => "success",
        Acknowledgement::Unrecognized => "unrecognized/error",
    };
    println!(
        "control acknowledgement: {acknowledgement} ({} payload={})",
        String::from_utf8_lossy(&response.command()),
        String::from_utf8_lossy(response.payload()),
    );
}

fn print_snapshot(snapshot: &DeviceSnapshot, show_identity: bool) {
    snapshot.frames().for_each(|(request, response)| {
        let payload = display_reply_payload(request, response.payload(), show_identity);
        println!(
            "{} -> {} payload_hex={payload}",
            String::from_utf8_lossy(request.frame()).trim_end(),
            String::from_utf8_lossy(&response.command()),
        );
    });
}

fn display_reply_payload(
    request: ReadCommand,
    payload: &[u8],
    show_identity: bool,
) -> ReplyPayload<'_> {
    match (request, show_identity) {
        (ReadCommand::Identity, false) => ReplyPayload::Redacted(payload.len()),
        _ => ReplyPayload::Hex(payload),
    }
}

struct DeviceDescription<'a>(&'a DiscoveredDevice);

impl fmt::Display for DeviceDescription<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        let device = self.0;
        write!(
            output,
            "id={} name={} rssi={}",
            device.id,
            device.name.as_deref().unwrap_or("(not advertised)"),
            SignalStrength(device.rssi),
        )
    }
}

struct SignalStrength(Option<i16>);

impl fmt::Display for SignalStrength {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(rssi) => write!(output, "{rssi} dBm"),
            None => output.write_str("(unknown)"),
        }
    }
}

enum ReplyPayload<'a> {
    Redacted(usize),
    Hex(&'a [u8]),
}

impl fmt::Display for ReplyPayload<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redacted(length) => write!(output, "<redacted; {length} bytes>"),
            Self::Hex(bytes) => bytes
                .iter()
                .try_for_each(|byte| write!(output, "{byte:02X}")),
        }
    }
}

struct ControlReadbackDisplay<'a>(&'a ControlReadback);

impl fmt::Display for ControlReadbackDisplay<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            ControlReadback::Thresholds(Ok(readback)) => write!(
                output,
                "automatic threshold readback: {}",
                describe_readback_match(readback.comparison),
            ),
            ControlReadback::Thresholds(Err(_)) => {
                output.write_str("automatic threshold readback: unrecognized payload")
            }
            ControlReadback::Timer(Ok(readback)) => write!(
                output,
                "timer readback: remaining={} minute(s), original={} minute(s); {}",
                readback.actual.remaining.value(),
                readback.actual.original.value(),
                describe_readback_match(readback.comparison),
            ),
            ControlReadback::Timer(Err(_)) => {
                output.write_str("timer readback: unrecognized payload")
            }
        }
    }
}

fn describe_readback_match(comparison: ReadbackMatch) -> &'static str {
    match comparison {
        ReadbackMatch::Matches => "matches request",
        ReadbackMatch::Differs => "differs from request",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use updraft_protocol::{
        AutomaticThresholds, HumidityTenthsPercent, Minutes, PayloadError, Readback, ReadbackError,
        TemperatureTenthsF, TimerState,
    };

    #[test]
    fn borrowed_payload_display_preserves_hex_and_identity_redaction() {
        assert_eq!(
            display_reply_payload(ReadCommand::Identity, b"AB", false).to_string(),
            "<redacted; 2 bytes>",
        );
        assert_eq!(
            display_reply_payload(ReadCommand::Identity, b"AB", true).to_string(),
            "4142",
        );
        assert_eq!(
            display_reply_payload(ReadCommand::Sensors, &[0x00, 0xAF], false).to_string(),
            "00AF",
        );
    }

    #[test]
    fn threshold_report_formats_typed_outcomes() {
        let actual = AutomaticThresholds {
            temperature: TemperatureTenthsF::new(1050),
            humidity: HumidityTenthsPercent::new(300),
        };
        [
            (ReadbackMatch::Matches, "matches request"),
            (ReadbackMatch::Differs, "differs from request"),
        ]
        .into_iter()
        .for_each(|(comparison, status)| {
            let outcome = ControlReadback::Thresholds(Ok(Readback { actual, comparison }));
            assert_eq!(
                ControlReadbackDisplay(&outcome).to_string(),
                format!("automatic threshold readback: {status}"),
            );
        });
        let outcome =
            ControlReadback::Thresholds(Err(PayloadError::from(ReadbackError::InvalidHex)));
        assert_eq!(
            ControlReadbackDisplay(&outcome).to_string(),
            "automatic threshold readback: unrecognized payload",
        );
    }

    #[test]
    fn timer_report_formats_typed_outcomes() {
        [
            (ReadbackMatch::Matches, "matches request"),
            (ReadbackMatch::Differs, "differs from request"),
        ]
        .into_iter()
        .for_each(|(comparison, status)| {
            let outcome = ControlReadback::Timer(Ok(Readback {
                actual: TimerState {
                    remaining: Minutes::new(3),
                    original: Minutes::new(5),
                },
                comparison,
            }));
            assert_eq!(
                ControlReadbackDisplay(&outcome).to_string(),
                format!("timer readback: remaining=3 minute(s), original=5 minute(s); {status}"),
            );
        });
        let outcome = ControlReadback::Timer(Err(PayloadError::from(ReadbackError::InvalidHex)));
        assert_eq!(
            ControlReadbackDisplay(&outcome).to_string(),
            "timer readback: unrecognized payload",
        );
    }
}
