use bytes::Bytes;
use gafctl_protocol::{
    Acknowledgement, AutomaticThresholds, ControlCommand, ControlOutcome, ControlReadback,
    DeviceMode, DeviceSnapshot, FanState, FirmwareVersion, Frame, HumidityTenthsPercent, Identity,
    Minutes, ModeReadback, OperatingMode, PayloadError, ReadCommand, Readback, ReadbackError,
    ReadbackMatch, Request, SensorReadings, StateFreshness, StateReconciler, TemperatureTenthsF,
    TimerState, UnexpectedResponse,
};
use std::time::{Duration, Instant, SystemTime};

fn frame(wire: &'static [u8]) -> Frame<'static> {
    Frame::from_bytes(Bytes::from_static(wire)).unwrap()
}

fn snapshot_with_mode(
    mode: &'static [u8],
    thresholds: &'static [u8],
    timer: &'static [u8],
) -> DeviceSnapshot {
    DeviceSnapshot::from_frames(
        frame(b"#idr030000example-suffix\n"),
        frame(mode),
        frame(b"#sdr03ca00aa\n"),
        frame(thresholds),
        frame(timer),
    )
    .unwrap()
}

fn snapshot(thresholds: &'static [u8], timer: &'static [u8]) -> DeviceSnapshot {
    snapshot_with_mode(b"#dmraf\n", thresholds, timer)
}

fn snapshot_at(
    mode: &'static [u8],
    observed_at: SystemTime,
    freshness_started_at: Instant,
) -> DeviceSnapshot {
    DeviceSnapshot::from_frames_at(
        frame(b"#idr030000example-suffix\n"),
        frame(mode),
        frame(b"#sdr03ca00aa\n"),
        frame(b"#atr041a012c\n"),
        frame(b"#ttr00000000\n"),
        observed_at,
        freshness_started_at,
    )
    .unwrap()
}

#[test]
fn reconciler_exposes_only_fresh_snapshots_and_keeps_failures_separate() {
    let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let freshness_started_at = Instant::now();
    let first = snapshot_at(b"#dmraf\n", observed_at, freshness_started_at);
    let mut state = StateReconciler::default();
    let first_poll = state.begin_poll();

    assert_eq!(
        state.freshness_at(freshness_started_at, Duration::from_secs(5)),
        StateFreshness::Unknown
    );
    state.apply_success(first_poll, first);
    assert_eq!(
        state.freshness_at(
            freshness_started_at + Duration::from_secs(4),
            Duration::from_secs(5)
        ),
        StateFreshness::Fresh
    );
    assert!(
        state
            .current_snapshot_at(
                freshness_started_at + Duration::from_secs(5),
                Duration::from_secs(5)
            )
            .is_some()
    );

    let failed_poll = state.begin_poll();
    state.apply_failure(failed_poll, "BLE poll timed out");
    assert_eq!(state.last_error(), Some("BLE poll timed out"));
    assert!(
        state
            .current_snapshot_at(
                freshness_started_at + Duration::from_secs(5),
                Duration::from_secs(5)
            )
            .is_some()
    );
    assert!(
        state
            .current_snapshot_at(
                freshness_started_at + Duration::from_secs(6),
                Duration::from_secs(5)
            )
            .is_none()
    );
    assert_eq!(
        state.freshness_at(
            freshness_started_at + Duration::from_secs(6),
            Duration::from_secs(5)
        ),
        StateFreshness::Stale
    );
    let future_wall_clock = snapshot_at(
        b"#dmraf\n",
        SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000_000),
        freshness_started_at,
    );
    assert!(!future_wall_clock.is_fresh_at(
        freshness_started_at + Duration::from_secs(6),
        Duration::from_secs(5)
    ));
}

#[test]
fn reconciler_applies_newer_app_side_state_and_ignores_late_older_polls() {
    let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let freshness_started_at = Instant::now();
    let mut state = StateReconciler::default();
    let older_poll = state.begin_poll();
    let newer_poll = state.begin_poll();

    state.apply_success(
        newer_poll,
        snapshot_at(
            b"#dmrtf\n",
            observed_at + Duration::from_secs(1),
            freshness_started_at + Duration::from_secs(1),
        ),
    );
    assert_eq!(
        state
            .latest_snapshot()
            .unwrap()
            .mode
            .decoded()
            .unwrap()
            .mode,
        OperatingMode::Timer
    );

    assert!(!state.apply_success(
        older_poll,
        snapshot_at(
            b"#dmraf\n",
            observed_at + Duration::from_secs(2),
            freshness_started_at + Duration::from_secs(2),
        ),
    ));
    assert_eq!(
        state
            .latest_snapshot()
            .unwrap()
            .mode
            .decoded()
            .unwrap()
            .mode,
        OperatingMode::Timer
    );
}

#[test]
fn reconciler_rejects_undecodable_state_and_keeps_last_confirmed_snapshot() {
    let observed_at = SystemTime::now();
    let monotonic_at = Instant::now();
    let mut state = StateReconciler::default();
    let confirmed_poll = state.begin_poll();
    state.apply_success(
        confirmed_poll,
        snapshot_at(b"#dmraf\n", observed_at, monotonic_at),
    );

    let invalid_poll = state.begin_poll();
    assert!(!state.apply_success(
        invalid_poll,
        snapshot_at(b"#dmrax\n", observed_at, monotonic_at),
    ));
    assert_eq!(
        state.last_error(),
        Some(
            "poll snapshot contains invalid payload: mode does not begin with a known mode and fan flag"
        )
    );
    assert_eq!(
        state
            .latest_snapshot()
            .unwrap()
            .mode
            .decoded()
            .unwrap()
            .mode,
        OperatingMode::Automatic
    );
}

#[test]
fn request_carries_frame_response_and_operation_together() {
    [
        (ReadCommand::Identity, b"#idg\n".as_slice(), *b"idr"),
        (ReadCommand::Mode, b"#dmg\n".as_slice(), *b"dmr"),
        (ReadCommand::Sensors, b"#sdg\n".as_slice(), *b"sdr"),
        (ReadCommand::AutoThresholds, b"#atg\n".as_slice(), *b"atr"),
        (ReadCommand::Timer, b"#ttg\n".as_slice(), *b"ttr"),
    ]
    .into_iter()
    .for_each(|(command, wire, response)| {
        assert_eq!(command.frame(), wire);
        assert_eq!(command.response_id(), response);
        let request = Request::from(command);
        assert!(match request.frame() {
            gafctl_protocol::RequestFrame::Read(_) => true,
            gafctl_protocol::RequestFrame::Control(_) => false,
        });
        assert_eq!(request.frame().as_ref(), wire);
        assert_eq!(request.response_id(), response);
        assert_eq!(request.operation(), "state query");
    });

    let command = ControlCommand::SetTimer(Minutes::new(1));
    let request = Request::from(command);
    assert!(match request.frame() {
        gafctl_protocol::RequestFrame::Read(_) => false,
        gafctl_protocol::RequestFrame::Control(_) => true,
    });
    assert_eq!(request.frame().as_ref(), b"#tms0001\n");
    assert_eq!(request.response_id(), *b"tmr");
    assert_eq!(request.operation(), "ordinary control command");

    let automatic = Request::from(ControlCommand::SetAutomaticThresholds(
        AutomaticThresholds {
            temperature: TemperatureTenthsF::new(1050),
            humidity: HumidityTenthsPercent::new(300),
        },
    ));
    assert!(match automatic.frame() {
        gafctl_protocol::RequestFrame::Read(_) => false,
        gafctl_protocol::RequestFrame::Control(_) => true,
    });
    assert_eq!(automatic.frame().as_ref(), b"#ams041A012C\n");
    assert_eq!(automatic.response_id(), *b"amr");
    assert_eq!(automatic.operation(), "ordinary control command");
}

#[test]
fn snapshot_decodes_known_values_and_retains_raw_frames_in_request_order() {
    let identity_wire = Bytes::from_static(b"#idr030000example-suffix\n");
    let identity_pointer = identity_wire.as_ptr();
    let snapshot = DeviceSnapshot::from_frames(
        Frame::from_bytes(identity_wire).unwrap(),
        frame(b"#dmraf\n"),
        frame(b"#sdr03ca00aa\n"),
        frame(b"#atr041a012c\n"),
        frame(b"#ttr00000000\n"),
    )
    .unwrap();

    assert_eq!(
        snapshot.identity.decoded(),
        Ok(&Identity {
            firmware_version: FirmwareVersion {
                major: 3,
                minor: 0,
                patch: 0,
            },
        })
    );
    assert_eq!(snapshot.identity.frame().payload(), b"030000example-suffix");
    assert_eq!(
        snapshot.identity.frame().as_bytes().as_ptr(),
        identity_pointer
    );
    assert_eq!(
        snapshot.mode.decoded(),
        Ok(&DeviceMode {
            mode: OperatingMode::Automatic,
            fan: FanState::Off,
        })
    );
    assert_eq!(
        snapshot.sensors.decoded(),
        Ok(&SensorReadings {
            temperature: TemperatureTenthsF::new(970),
            humidity: HumidityTenthsPercent::new(170),
        })
    );
    assert_eq!(
        snapshot.thresholds.decoded(),
        Ok(&AutomaticThresholds {
            temperature: TemperatureTenthsF::new(1050),
            humidity: HumidityTenthsPercent::new(300),
        })
    );
    assert_eq!(
        snapshot.timer.decoded(),
        Ok(&TimerState {
            remaining: Minutes::new(0),
            original: Minutes::new(0),
        })
    );
    assert_eq!(
        snapshot
            .frames()
            .map(|(request, frame)| (request, frame.command()))
            .collect::<Vec<_>>(),
        [
            (ReadCommand::Identity, *b"idr"),
            (ReadCommand::Mode, *b"dmr"),
            (ReadCommand::Sensors, *b"sdr"),
            (ReadCommand::AutoThresholds, *b"atr"),
            (ReadCommand::Timer, *b"ttr"),
        ]
    );
}

#[test]
fn snapshot_decodes_known_timer_and_ota_mode_flags() {
    [
        (b"#dmrtn\n".as_slice(), OperatingMode::Timer, FanState::On),
        (b"#dmrof\n".as_slice(), OperatingMode::Ota, FanState::Off),
    ]
    .into_iter()
    .for_each(|(mode_wire, mode, fan)| {
        let snapshot = snapshot_with_mode(mode_wire, b"#atr041a012c\n", b"#ttr00000000\n");
        assert_eq!(snapshot.mode.decoded(), Ok(&DeviceMode { mode, fan }));
    });
}

#[test]
fn snapshot_retains_malformed_payloads_but_rejects_wrong_response_id() {
    let malformed = DeviceSnapshot::from_frames(
        frame(b"#idr03x000suffix\n"),
        frame(b"#dmrzz\n"),
        frame(b"#sdr03ca00ag\n"),
        frame(b"#atr041a012\n"),
        frame(b"#ttr0000000z\n"),
    )
    .unwrap();

    assert_eq!(
        malformed.identity.decoded(),
        Err(&PayloadError::InvalidIdentity)
    );
    assert_eq!(malformed.mode.decoded(), Err(&PayloadError::InvalidMode));
    assert_eq!(
        malformed.sensors.decoded(),
        Err(&PayloadError::from(ReadbackError::InvalidHex))
    );
    assert_eq!(
        malformed.thresholds.decoded(),
        Err(&PayloadError::from(ReadbackError::InvalidLength))
    );
    assert_eq!(
        malformed.timer.decoded(),
        Err(&PayloadError::from(ReadbackError::InvalidHex))
    );
    assert_eq!(malformed.identity.frame().payload(), b"03x000suffix");

    let wrong_id = DeviceSnapshot::from_frames(
        frame(b"#idr030000\n"),
        frame(b"#atr041a012c\n"),
        frame(b"#sdr03ca00aa\n"),
        frame(b"#atr041a012c\n"),
        frame(b"#ttr00000000\n"),
    );
    assert_eq!(
        wrong_id.err(),
        Some(UnexpectedResponse {
            expected: *b"dmr",
            actual: *b"atr",
        })
    );
}

#[test]
fn threshold_outcome_uses_exact_ack_and_compares_typed_readback() {
    let requested = AutomaticThresholds {
        temperature: TemperatureTenthsF::new(1050),
        humidity: HumidityTenthsPercent::new(300),
    };
    let command = ControlCommand::SetAutomaticThresholds(requested);
    let matching = snapshot(b"#atr041a012c\n", b"#ttr00000000\n");
    let accepted =
        ControlOutcome::from_response(command, frame(b"#amr0\n"), Some(&matching)).unwrap();

    assert_eq!(accepted.command(), command);
    assert_eq!(accepted.frame().as_bytes(), b"#amr0\n");
    assert_eq!(accepted.acknowledgement(), Acknowledgement::Accepted);
    assert!(accepted.is_confirmed());
    assert_eq!(
        accepted.readback(),
        &ControlReadback::Thresholds(Ok(Readback {
            actual: requested,
            comparison: ReadbackMatch::Matches,
        }))
    );

    let differing = snapshot(b"#atr041b012c\n", b"#ttr00000000\n");
    let unrecognized =
        ControlOutcome::from_response(command, frame(b"#amr1\n"), Some(&differing)).unwrap();
    assert_eq!(
        unrecognized.acknowledgement(),
        Acknowledgement::Unrecognized
    );
    assert!(!unrecognized.is_confirmed());
    assert_eq!(
        unrecognized.readback(),
        &ControlReadback::Thresholds(Ok(Readback {
            actual: AutomaticThresholds {
                temperature: TemperatureTenthsF::new(1051),
                humidity: HumidityTenthsPercent::new(300),
            },
            comparison: ReadbackMatch::Differs,
        }))
    );

    [b"#amr\n".as_slice(), b"#amr00\n".as_slice()]
        .into_iter()
        .for_each(|payload| {
            let outcome =
                ControlOutcome::from_response(command, frame(payload), Some(&matching)).unwrap();
            assert_eq!(outcome.acknowledgement(), Acknowledgement::Unrecognized);
            assert_eq!(outcome.frame().as_bytes(), payload);
        });

    let malformed = snapshot(b"#atrbad\n", b"#ttr00000000\n");
    let outcome =
        ControlOutcome::from_response(command, frame(b"#amr0\n"), Some(&malformed)).unwrap();
    assert_eq!(
        outcome.readback(),
        &ControlReadback::Thresholds(Err(PayloadError::from(ReadbackError::InvalidLength)))
    );
    assert!(!outcome.is_confirmed());
}

#[test]
fn timer_outcome_allows_elapsed_time_and_keeps_readback_errors() {
    let command = ControlCommand::SetTimer(Minutes::new(2));
    let matching = snapshot_with_mode(b"#dmrtn\n", b"#atr041a012c\n", b"#ttr00010002\n");
    let outcome =
        ControlOutcome::from_response(command, frame(b"#tmr0\n"), Some(&matching)).unwrap();
    assert_eq!(outcome.acknowledgement(), Acknowledgement::Accepted);
    assert!(outcome.is_confirmed());
    assert_eq!(
        outcome.mode_readback(),
        ModeReadback::Matches(OperatingMode::Timer)
    );
    assert_eq!(
        outcome.readback(),
        &ControlReadback::Timer(Ok(Readback {
            actual: TimerState {
                remaining: Minutes::new(1),
                original: Minutes::new(2),
            },
            comparison: ReadbackMatch::Matches,
        }))
    );
    let unrecognized =
        ControlOutcome::from_response(command, frame(b"#tmr1\n"), Some(&matching)).unwrap();
    assert_eq!(
        unrecognized.acknowledgement(),
        Acknowledgement::Unrecognized
    );
    assert!(!unrecognized.is_confirmed());

    let differing = snapshot(b"#atr041a012c\n", b"#ttr00010003\n");
    let outcome =
        ControlOutcome::from_response(command, frame(b"#tmr0\n"), Some(&differing)).unwrap();
    let readback_comparison = match outcome.readback() {
        ControlReadback::Timer(Ok(readback)) => Some(readback.comparison),
        _ => None,
    };
    assert_eq!(readback_comparison, Some(ReadbackMatch::Differs));
    assert!(!outcome.is_confirmed());

    let excessive_remaining = snapshot(b"#atr041a012c\n", b"#ttr00030002\n");
    let outcome =
        ControlOutcome::from_response(command, frame(b"#tmr0\n"), Some(&excessive_remaining))
            .unwrap();
    let readback_comparison = match outcome.readback() {
        ControlReadback::Timer(Ok(readback)) => Some(readback.comparison),
        _ => None,
    };
    assert_eq!(readback_comparison, Some(ReadbackMatch::Differs));

    let malformed = snapshot(b"#atr041a012c\n", b"#ttrbad\n");
    let outcome =
        ControlOutcome::from_response(command, frame(b"#tmr0\n"), Some(&malformed)).unwrap();
    assert_eq!(
        outcome.readback(),
        &ControlReadback::Timer(Err(PayloadError::from(ReadbackError::InvalidLength)))
    );
    assert!(!outcome.is_confirmed());
    let unavailable = ControlOutcome::from_response(command, frame(b"#tmr0\n"), None).unwrap();
    assert_eq!(unavailable.readback(), &ControlReadback::Unavailable);
    assert!(!unavailable.is_confirmed());
    assert_eq!(malformed.timer.frame().payload(), b"bad");
    assert_eq!(
        ControlOutcome::from_response(command, frame(b"#amr0\n"), Some(&matching)).err(),
        Some(UnexpectedResponse {
            expected: *b"tmr",
            actual: *b"amr",
        })
    );
}

#[test]
fn threshold_confirmation_requires_automatic_decodable_mode() {
    let requested = AutomaticThresholds {
        temperature: TemperatureTenthsF::new(1050),
        humidity: HumidityTenthsPercent::new(300),
    };
    for (mode, expected) in [
        (
            b"#dmrtn\n".as_slice(),
            ModeReadback::Differs(OperatingMode::Timer),
        ),
        (
            b"#dmrzz\n".as_slice(),
            ModeReadback::Unrecognized(PayloadError::InvalidMode),
        ),
    ] {
        let snapshot = snapshot_with_mode(mode, b"#atr041a012c\n", b"#ttr00000000\n");
        let outcome = ControlOutcome::from_response(
            ControlCommand::SetAutomaticThresholds(requested),
            frame(b"#amr0\n"),
            Some(&snapshot),
        )
        .unwrap();
        assert_eq!(
            outcome.readback(),
            &ControlReadback::Thresholds(Ok(Readback {
                actual: requested,
                comparison: ReadbackMatch::Matches,
            })),
            "mode={mode:?}"
        );
        assert_eq!(outcome.mode_readback(), expected, "mode={mode:?}");
        assert!(!outcome.is_confirmed(), "mode={mode:?}");
    }
}

#[test]
fn positive_timer_with_zero_remaining_stays_unconfirmed() {
    let outcomes = [b"#dmrtf\n", b"#dmrtn\n", b"#dmraf\n"].map(|mode| {
        let snapshot = snapshot_with_mode(mode, b"#atr041a012c\n", b"#ttr00000001\n");
        let outcome = ControlOutcome::from_response(
            ControlCommand::SetTimer(Minutes::new(1)),
            frame(b"#tmr0\n"),
            Some(&snapshot),
        )
        .unwrap();
        (outcome.mode_readback(), outcome.is_confirmed())
    });

    assert_eq!(
        outcomes,
        [
            (
                ModeReadback::UnverifiedTimerExpiry(OperatingMode::Timer),
                false
            ),
            (
                ModeReadback::UnverifiedTimerExpiry(OperatingMode::Timer),
                false
            ),
            (
                ModeReadback::UnverifiedTimerExpiry(OperatingMode::Automatic),
                false,
            ),
        ]
    );
}

#[test]
fn timer_confirmation_requires_verified_mode_expiry_and_fan_flag() {
    for (minutes, mode, timer, expected, confirmed) in [
        (
            1,
            b"#dmraf\n".as_slice(),
            b"#ttr00010001\n".as_slice(),
            ModeReadback::Differs(OperatingMode::Automatic),
            false,
        ),
        (
            1,
            b"#dmraf\n".as_slice(),
            b"#ttr00000001\n".as_slice(),
            ModeReadback::UnverifiedTimerExpiry(OperatingMode::Automatic),
            false,
        ),
        (
            0,
            b"#dmrtn\n".as_slice(),
            b"#ttr00000000\n".as_slice(),
            ModeReadback::FanFlagDiffers {
                mode: OperatingMode::Timer,
                actual: FanState::On,
            },
            false,
        ),
        (
            0,
            b"#dmrtf\n".as_slice(),
            b"#ttr00000000\n".as_slice(),
            ModeReadback::Matches(OperatingMode::Timer),
            true,
        ),
        (
            0,
            b"#dmraf\n".as_slice(),
            b"#ttr00000000\n".as_slice(),
            ModeReadback::Differs(OperatingMode::Automatic),
            false,
        ),
    ] {
        let snapshot = snapshot_with_mode(mode, b"#atr041a012c\n", timer);
        let outcome = ControlOutcome::from_response(
            ControlCommand::SetTimer(Minutes::new(minutes)),
            frame(b"#tmr0\n"),
            Some(&snapshot),
        )
        .unwrap();
        assert_eq!(
            outcome.mode_readback(),
            expected,
            "minutes={minutes} mode={mode:?} timer={timer:?}"
        );
        assert_eq!(
            outcome.is_confirmed(),
            confirmed,
            "minutes={minutes} mode={mode:?} timer={timer:?}"
        );
    }
}
