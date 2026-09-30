use std::time::Duration;

use anyhow::Result;
use updraft_protocol::ControlCommand;

use crate::{
    ProbeMode, ProbeOptions, ProbeResult,
    discovery::{
        Candidate, CandidateSelection, DiscoveryReport, can_query_with_report, discover_candidates,
        incomplete_result, select_candidate, summarize_scan,
    },
    session::query_peripheral,
};

/// Discover GAF BLE peripherals and, when selected unambiguously, issue the
/// read-only queries plus an optional ordinary control-setting write.
pub async fn probe(options: ProbeOptions) -> Result<ProbeResult> {
    let discovery = discover_candidates(options.scan_duration, options.response_timeout).await?;

    match options.mode {
        ProbeMode::Scan => Ok(summarize_scan(discovery)),
        ProbeMode::Query {
            device_id,
            control_command,
        } => {
            query_selected_device(
                discovery,
                device_id.as_deref(),
                control_command,
                options.response_timeout,
            )
            .await
        }
    }
}

async fn query_selected_device(
    discovery: DiscoveryReport<Candidate>,
    device_id: Option<&str>,
    control_command: Option<ControlCommand>,
    response_timeout: Duration,
) -> Result<ProbeResult> {
    if !can_query_with_report(&discovery, device_id) {
        return Ok(incomplete_result(discovery));
    }

    let failures = discovery.failures;
    match select_candidate(discovery.candidates, device_id)? {
        CandidateSelection::NoDevices => Ok(ProbeResult::NoDevices),
        CandidateSelection::Ambiguous(devices) => Ok(ProbeResult::Ambiguous { devices }),
        CandidateSelection::Chosen { device, peripheral } => {
            let mut result =
                query_peripheral(&peripheral, response_timeout, control_command).await?;
            result.discovery_failures = failures;
            Ok(ProbeResult::Queried { device, result })
        }
    }
}
