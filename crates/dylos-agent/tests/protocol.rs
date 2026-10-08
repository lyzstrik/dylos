use dylos_agent::protocol::{
    PROTOCOL_VERSION, ProtocolError, Reply, ReplyBody, Request, RequestBody,
};

#[test]
fn request_round_trip() {
    for body in [
        RequestBody::Resync {
            unix_time_ns: 1_700_000_000_123_456_789,
        },
        RequestBody::Health,
    ] {
        let request = Request::new(body);
        let line = request.to_line().unwrap();
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
        assert_eq!(Request::from_line(&line).unwrap(), request);
    }
}

#[test]
fn reply_round_trip() {
    for body in [
        ReplyBody::Ok,
        ReplyBody::Health {
            uptime_ms: 42,
            wall_clock_unix_ns: 7,
            agent_version: "0.1.0".to_owned(),
        },
        ReplyBody::Error {
            message: "boom".to_owned(),
        },
    ] {
        let reply = Reply::new(body);
        assert_eq!(Reply::from_line(&reply.to_line().unwrap()).unwrap(), reply);
    }
}

#[test]
fn wire_format_is_stable() {
    let line = Request::new(RequestBody::Resync { unix_time_ns: 5 })
        .to_line()
        .unwrap();
    assert_eq!(line, "{\"v\":1,\"type\":\"resync\",\"unix_time_ns\":5}\n");
}

#[test]
fn other_versions_are_rejected() {
    let line = r#"{"v":2,"type":"future_request"}"#;
    assert!(matches!(
        Request::from_line(line),
        Err(ProtocolError::UnsupportedVersion { got: 2 })
    ));
    assert_eq!(PROTOCOL_VERSION, 1);
}

#[test]
fn missing_version_and_garbage_are_rejected() {
    assert!(matches!(
        Request::from_line(r#"{"type":"health"}"#),
        Err(ProtocolError::MissingVersion)
    ));
    assert!(matches!(
        Request::from_line("not json"),
        Err(ProtocolError::Malformed(_))
    ));
    assert!(matches!(
        Request::from_line(r#"{"v":1,"type":"nope"}"#),
        Err(ProtocolError::Malformed(_))
    ));
}
