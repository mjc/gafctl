use updraft_protocol::{ControlCommand, Frame, FrameDecoder, FrameError, ReadCommand};

#[test]
fn read_only_commands_encode_the_observed_line_frames() {
    let cases = [
        (ReadCommand::Identity, b"#idg\n".as_slice(), *b"idr"),
        (ReadCommand::Mode, b"#dmg\n".as_slice(), *b"dmr"),
        (ReadCommand::Sensors, b"#sdg\n".as_slice(), *b"sdr"),
        (ReadCommand::AutoThresholds, b"#atg\n".as_slice(), *b"atr"),
        (ReadCommand::Timer, b"#ttg\n".as_slice(), *b"ttr"),
    ];

    cases.into_iter().for_each(|(command, expected, response)| {
        assert_eq!(command.frame(), expected);
        assert_eq!(command.response_id(), response);
    });
}

#[test]
fn automatic_threshold_write_uses_tenths_and_uppercase_hex() {
    let command = ControlCommand::SetAutomaticThresholds {
        temperature_tenths_f: 1050,
        humidity_tenths_percent: 300,
    };

    assert_eq!(command.frame(), b"#ams041A012C\n");
    assert_eq!(command.response_id(), *b"amr");
}

#[test]
fn automatic_threshold_write_encodes_full_u16_fields() {
    let command = ControlCommand::SetAutomaticThresholds {
        temperature_tenths_f: u16::MAX,
        humidity_tenths_percent: 0,
    };

    assert_eq!(command.frame(), b"#amsFFFF0000\n");
}

#[test]
fn timer_write_encodes_minutes_as_uppercase_hex() {
    let command = ControlCommand::SetTimer {
        duration_minutes: 1,
    };

    assert_eq!(command.frame(), b"#tms0001\n");
    assert_eq!(command.response_id(), *b"tmr");
}

#[test]
fn response_frame_parser_preserves_uninterpreted_payload_bytes() {
    let frame = Frame::parse(b"#sdr00AF7F2A\n").unwrap();

    assert_eq!(frame.command(), *b"sdr");
    assert_eq!(frame.payload(), b"00AF7F2A");
    assert_eq!(frame.encode(), b"#sdr00AF7F2A\n");
}

#[test]
fn sanitized_device_capture_replies_parse_without_losing_payloads() {
    let captures = [
        (b"#dmraf\n".as_slice(), *b"dmr", b"af".as_slice()),
        (
            b"#sdr03ca00aa\n".as_slice(),
            *b"sdr",
            b"03ca00aa".as_slice(),
        ),
        (
            b"#atr041a012c\n".as_slice(),
            *b"atr",
            b"041a012c".as_slice(),
        ),
        (
            b"#ttr00000000\n".as_slice(),
            *b"ttr",
            b"00000000".as_slice(),
        ),
        (b"#amr0\n".as_slice(), *b"amr", b"0".as_slice()),
    ];

    captures.into_iter().for_each(|(bytes, command, payload)| {
        let frame = Frame::parse(bytes).unwrap();
        assert_eq!(frame.command(), command);
        assert_eq!(frame.payload(), payload);
        assert_eq!(frame.encode(), bytes);
    });
}

#[test]
fn frame_parser_rejects_incomplete_or_malformed_frames() {
    [
        (b"#dmr".as_slice(), FrameError::MissingLineFeed),
        (b"dmr\n".as_slice(), FrameError::InvalidStart),
        (b"#d\n".as_slice(), FrameError::InvalidCommand),
        (b"#dmr\n#dmr\n".as_slice(), FrameError::TrailingData),
    ]
    .into_iter()
    .for_each(|(bytes, expected_error)| {
        assert_eq!(Frame::parse(bytes), Err(expected_error));
    });
}

#[test]
fn frame_decoder_handles_partial_and_coalesced_notifications() {
    let mut decoder = FrameDecoder::default();

    assert!(decoder.push(b"#dm").unwrap().is_empty());
    let frames = decoder.push(b"r\n#amr1\n").unwrap();

    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].command(), *b"dmr");
    assert_eq!(frames[1].command(), *b"amr");
    assert_eq!(frames[1].payload(), b"1");
}

#[test]
fn frame_decoder_rejects_oversized_complete_and_partial_frames() {
    let mut decoder = FrameDecoder::default();
    let oversized_frame = [b"#sdr".as_slice(), &vec![b'x'; 1021], b"\n"].concat();

    assert_eq!(decoder.push(&oversized_frame), Err(FrameError::TooLong));
    assert_eq!(decoder.push(&vec![b'x'; 1025]), Err(FrameError::TooLong));
}
