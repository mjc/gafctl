use std::time::Duration;

use anyhow::Result;
use updraft_protocol::ControlCommand;

use crate::{
    ProbeMode, ProbeOptions, ProbeResult,
    discovery::{
        Candidate, CandidateSelection, discover_candidates, select_candidate, summarize_scan,
    },
    session::query_peripheral,
};

/// Discover GAF BLE peripherals and, when selected unambiguously, issue the
/// read-only queries plus an optional ordinary control-setting write.
pub async fn probe(options: ProbeOptions) -> Result<ProbeResult> {
    let candidates = discover_candidates(options.scan_duration, options.response_timeout).await?;

    match options.mode {
        ProbeMode::Scan => Ok(summarize_scan(candidates)),
        ProbeMode::Query {
            device_id,
            control_command,
        } => {
            query_selected_device(
                candidates,
                device_id.as_deref(),
                control_command,
                options.response_timeout,
            )
            .await
        }
    }
}

async fn query_selected_device(
    candidates: Vec<Candidate>,
    device_id: Option<&str>,
    control_command: Option<ControlCommand>,
    response_timeout: Duration,
) -> Result<ProbeResult> {
    match select_candidate(candidates, device_id)? {
        CandidateSelection::NoDevices => Ok(ProbeResult::NoDevices),
        CandidateSelection::Ambiguous(devices) => Ok(ProbeResult::Ambiguous { devices }),
        CandidateSelection::Chosen { device, peripheral } => {
            let result = query_peripheral(&peripheral, response_timeout, control_command).await?;
            Ok(ProbeResult::Queried { device, result })
        }
    }
}
