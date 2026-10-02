use std::{
    fmt,
    io::{self, Write},
};

use crate::control_display::{ControlReadbackDisplay, ModeReadbackDisplay};
use gafctl_bluetooth::{DiscoveredDevice, ProbeResult};
use gafctl_protocol::{Acknowledgement, ControlOutcome, DeviceSnapshot, ReadCommand};

pub(crate) use crate::stdout::{StdoutError, write_stdout};

pub(crate) fn print_probe_result(result: ProbeResult, show_identity: bool) -> anyhow::Result<()> {
    write_stdout(|output| write_probe_result(output, result, show_identity))
}

fn write_probe_result(
    output: &mut dyn Write,
    result: ProbeResult,
    show_identity: bool,
) -> io::Result<()> {
    match result {
        ProbeResult::NoDevices => {
            writeln!(
                output,
                "No nearby BLE device advertising GAF service 00FF was found."
            )?;
        }
        ProbeResult::Discovered { devices } => {
            print_devices(output, devices.iter().map(|candidate| candidate.device()))?;
            writeln!(
                output,
                "Scan-only mode: no connection or protocol request was made."
            )?;
        }
        ProbeResult::Ambiguous { devices } => {
            print_devices(output, devices.iter().map(|candidate| candidate.device()))?;
            writeln!(
                output,
                "More than one candidate found. Re-run with --device-id <id> to query one fan."
            )?;
        }
        ProbeResult::DiscoveryIncomplete { devices, failures } => {
            print_devices(output, devices.iter().map(|candidate| candidate.device()))?;
            writeln!(
                output,
                "BLE discovery incomplete; automatic selection was skipped."
            )?;
            print_discovery_failures(&failures);
        }
        ProbeResult::Queried { device, result } => {
            writeln!(
                output,
                "Queried GAF BLE device: {}",
                DeviceDescription(&device)
            )?;
            if let Some(control) = &result.control {
                print_control_acknowledgement(output, control)?;
            }
            if let Some(snapshot) = &result.snapshot {
                print_snapshot(output, snapshot, show_identity)?;
            }
            if let Some(control) = &result.control {
                writeln!(output, "{}", ControlReadbackDisplay(control.readback()))?;
                writeln!(output, "{}", ModeReadbackDisplay(control.mode_readback()))?;
            }
            if let Some(error) = &result.state_error {
                eprintln!("state readback unavailable after control acknowledgement: {error}");
            }
            print_discovery_failures(&result.discovery_failures);
            if let gafctl_bluetooth::DisconnectOutcome::Failed(error) = &result.disconnect {
                eprintln!("BLE query succeeded, but disconnect failed: {error}");
            }
        }
    }
    Ok(())
}

fn print_discovery_failures(failures: &[gafctl_bluetooth::DiscoveryFailure]) {
    failures.iter().for_each(|failure| {
        eprintln!(
            "BLE properties unavailable for {}: {}",
            failure.device_id, failure.reason
        );
    });
}

fn print_devices<'a>(
    output: &mut dyn Write,
    devices: impl ExactSizeIterator<Item = &'a DiscoveredDevice>,
) -> io::Result<()> {
    writeln!(output, "Found {} GAF BLE device(s):", devices.len())?;
    devices.enumerate().try_for_each(|(index, device)| {
        writeln!(output, "  [{index}] {}", DeviceDescription(device))
    })
}

fn print_control_acknowledgement(
    output: &mut dyn Write,
    control: &ControlOutcome,
) -> io::Result<()> {
    let response = control.frame();
    let acknowledgement = match control.acknowledgement() {
        Acknowledgement::Accepted => "success",
        Acknowledgement::Unrecognized => "unrecognized/error",
    };
    writeln!(
        output,
        "control acknowledgement: {acknowledgement} ({} payload={})",
        String::from_utf8_lossy(&response.command()),
        String::from_utf8_lossy(response.payload()),
    )
}

fn print_snapshot(
    output: &mut dyn Write,
    snapshot: &DeviceSnapshot,
    show_identity: bool,
) -> io::Result<()> {
    snapshot.frames().try_for_each(|(request, response)| {
        let payload = display_reply_payload(request, response.payload(), show_identity);
        writeln!(
            output,
            "{} -> {} payload_hex={payload}",
            String::from_utf8_lossy(request.frame()).trim_end(),
            String::from_utf8_lossy(&response.command()),
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    use gafctl_protocol::{
        AutomaticThresholds, ControlReadback, HumidityTenthsPercent, Minutes, PayloadError,
        Readback, ReadbackError, ReadbackMatch, TemperatureTenthsF, TimerState,
    };

    #[test]
    fn diagnostic_probe_output_propagates_a_closed_pipe_instead_of_panicking() {
        struct ClosedPipe;
        impl Write for ClosedPipe {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let error = write_probe_result(&mut ClosedPipe, ProbeResult::NoDevices, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

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
