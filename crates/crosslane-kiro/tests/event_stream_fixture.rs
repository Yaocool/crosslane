use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use bytes::BytesMut;
use crosslane_kiro::{EventStreamDecoder, KiroEvent};
use tokio_util::codec::Decoder;

fn fixture() -> Vec<u8> {
    let encoded = include_str!("fixtures/sanitized-kiro-response.aws-eventstream.b64")
        .split_whitespace()
        .collect::<String>();
    STANDARD.decode(encoded).expect("valid fixture base64")
}

#[test]
fn sanitized_wire_fixture_survives_half_frames_and_sticky_frames() {
    let bytes = fixture();
    let split = 37;
    let mut buffer = BytesMut::from(&bytes[..split]);
    let mut decoder = EventStreamDecoder;
    assert_eq!(decoder.decode(&mut buffer).expect("half frame"), None);
    buffer.extend_from_slice(&bytes[split..]);

    assert_eq!(
        decoder.decode(&mut buffer).expect("assistant frame"),
        Some(KiroEvent::AssistantResponse {
            content: "sanitized captured response".into()
        })
    );
    assert_eq!(
        decoder.decode(&mut buffer).expect("tool frame"),
        Some(KiroEvent::ToolUse {
            id: "fixture-tool-1".into(),
            name: "read_file".into(),
            input_delta: r#"{"path":"README.md"}"#.into(),
            stop: true,
        })
    );
    assert!(matches!(
        decoder.decode(&mut buffer).expect("metadata frame"),
        Some(KiroEvent::MessageMetadata { .. })
    ));
    assert!(buffer.is_empty());
}

#[test]
fn sanitized_wire_fixture_rejects_crc_corruption_and_truncation() {
    let bytes = fixture();
    let first_length = u32::from_be_bytes(bytes[..4].try_into().expect("length")) as usize;

    let mut corrupt = bytes[..first_length].to_vec();
    corrupt[first_length - 5] ^= 0x01;
    let mut corrupt = BytesMut::from(corrupt.as_slice());
    assert!(EventStreamDecoder.decode(&mut corrupt).is_err());

    let mut truncated = BytesMut::from(&bytes[..first_length - 1]);
    assert!(EventStreamDecoder.decode_eof(&mut truncated).is_err());
}
