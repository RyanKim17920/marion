use marion_core::contract::AgentId;
use marion_core::proto::{
    Event, OpaquePaneBytesV1, PaneFrameKindV1, PaneFrameV1, PaneReadyTokenV1,
};

#[test]
fn opaque_pane_bytes_round_trip_non_utf8_with_canonical_base64() {
    let bytes = [0x00, 0x01, 0xff, 0x80];
    let opaque = OpaquePaneBytesV1::new(bytes);

    assert_eq!(opaque.as_bytes(), &bytes);
    assert_eq!(serde_json::to_string(&opaque).unwrap(), r#""AAH/gA==""#);
    assert_eq!(
        serde_json::from_str::<OpaquePaneBytesV1>(r#""AAH/gA==""#).unwrap(),
        opaque
    );
}

#[test]
fn opaque_pane_bytes_reject_noncanonical_or_malformed_base64() {
    for encoded in [r#""AAH/gA""#, r#""AAH/gA=""#, r#""AAH/gA===""#, r#""%%%""#] {
        let error = serde_json::from_str::<OpaquePaneBytesV1>(encoded)
            .expect_err("pane bytes must use canonical padded standard base64")
            .to_string();

        assert!(error.contains("base64-encoded pane bytes"), "{error}");
    }
}

#[test]
fn pane_ready_token_round_trips_exactly_32_bytes_with_canonical_base64() {
    let bytes = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];
    let token = PaneReadyTokenV1::new(bytes);

    assert_eq!(token.as_bytes(), &bytes);
    assert_eq!(
        serde_json::to_string(&token).unwrap(),
        r#""AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=""#
    );
    assert_eq!(
        serde_json::from_str::<PaneReadyTokenV1>(
            r#""AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=""#,
        )
        .unwrap(),
        token
    );
}

#[test]
fn pane_ready_token_rejects_wrong_length_and_noncanonical_base64() {
    for encoded in [r#""""#, r#""AA==""#] {
        let error = serde_json::from_str::<PaneReadyTokenV1>(encoded)
            .expect_err("pane-ready tokens must decode to exactly 32 bytes")
            .to_string();
        assert!(error.contains("exactly 32 bytes"), "{error}");
    }

    for encoded in [
        r#""AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8""#,
        r#""AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8==""#,
    ] {
        let error = serde_json::from_str::<PaneReadyTokenV1>(encoded)
            .expect_err("pane-ready tokens must use canonical padded standard base64")
            .to_string();
        assert!(error.contains("base64-encoded pane-ready token"), "{error}");
    }
}

#[test]
fn pane_frame_events_pin_output_resize_and_end_wire_shapes() {
    let cases = [
        (
            Event::NodePaneFrame(PaneFrameV1::new(
                AgentId("a".into()),
                0,
                PaneFrameKindV1::Output {
                    bytes: OpaquePaneBytesV1::new([0x00, 0x01, 0xff, 0x80]),
                },
            )),
            r#"{"method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"Output","bytes":"AAH/gA=="}}}"#,
        ),
        (
            Event::NodePaneFrame(PaneFrameV1::new(
                AgentId("a".into()),
                1,
                PaneFrameKindV1::Resize {
                    cols: 140,
                    rows: 40,
                },
            )),
            r#"{"method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":1,"frame":{"kind":"Resize","cols":140,"rows":40}}}"#,
        ),
        (
            Event::NodePaneFrame(PaneFrameV1::new(
                AgentId("a".into()),
                2,
                PaneFrameKindV1::End {},
            )),
            r#"{"method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":2,"frame":{"kind":"End"}}}"#,
        ),
    ];

    for (event, json) in cases {
        assert_eq!(serde_json::to_string(&event).unwrap(), json);
        assert_eq!(serde_json::from_str::<Event>(json).unwrap(), event);
    }
}

#[test]
fn pane_frame_events_reject_invalid_versions_kinds_and_fields() {
    for invalid in [
        r#"{"method":"node/pane-frame","params":{"agent_id":"a","seq":0,"frame":{"kind":"End"}}}"#,
        r#"{"method":"node/pane-frame","params":{"version":0,"agent_id":"a","seq":0,"frame":{"kind":"End"}}}"#,
        r#"{"method":"node/pane-frame","params":{"version":2,"agent_id":"a","seq":0,"frame":{"kind":"End"}}}"#,
        r#"{"method":"node/pane-frame","params":{"agent_id":"a","seq":0,"version":1}}"#,
        r#"{"method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"Unknown"}}}"#,
        r#"{"method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"Output"}}}"#,
        r#"{"method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"Output","bytes":"AAH/gA==","cols":80}}}"#,
        r#"{"method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"Resize","cols":80}}}"#,
        r#"{"method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"End","bytes":"AAH/gA=="}}}"#,
        r#"{"method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"End","bytes":null}}}"#,
    ] {
        assert!(
            serde_json::from_str::<Event>(invalid).is_err(),
            "accepted malformed pane frame event: {invalid}"
        );
    }
}
