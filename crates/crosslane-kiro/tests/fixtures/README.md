# Kiro Event Stream fixture

[`sanitized-kiro-response.aws-eventstream.b64`](sanitized-kiro-response.aws-eventstream.b64)
is a sanitized, re-encoded compatibility fixture. It preserves the AWS Event Stream
wire shape (all three standard string headers, non-zero prelude and message CRCs,
and assistant/tool/metadata payload shapes) while using inert content,
identifiers, and tool arguments. It is base64 rather than a raw binary file so
reviews and source archives cannot silently alter or drop bytes.

The fixture contains assistant text, one `read_file` tool call and metadata.
It verifies decoding behavior, not current upstream availability or the accuracy
of all production event shapes. The repository does not include the original
capture or a pinned provenance record; do not describe this file as a raw capture.

## Verify

Run from the repository root:

```bash
cargo test -p crosslane-kiro --test event_stream_fixture --locked
```

The [fixture tests](../event_stream_fixture.rs) exercise split frames, concatenated
frames, corrupted CRCs and truncated input. Related decoder behavior lives in
[event_stream.rs](../../src/event_stream.rs).

## Refresh

Raw production responses must not be committed because they may contain
credentials, prompt content, repository paths, and model output. When refreshing
this fixture, sanitize payloads first, reassemble the frames with valid CRCs, and
verify the half-frame, sticky-frame, and malformed-frame tests still exercise
the decoder. Keep identifiers and arguments inert, document the source/version
and sanitization steps, and review decoded content as well as the base64 diff.

Return to [contributor guidance](../../../../CONTRIBUTING.md) or the
[documentation links](../../../../README.md#documentation).
