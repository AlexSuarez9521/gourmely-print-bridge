//! Engine.IO v4 / Socket.IO v5 text frames, by hand.
//!
//! # Why this file exists instead of a `socket.io` crate
//!
//! `rust_socketio` 0.6 is the obvious choice and it was tried first, against
//! the real API. It does not work here, and the way it fails is worse than not
//! working: **it reports a successful connection to a server that has already
//! hung up.**
//!
//! Captured on the wire (a logging proxy between the client and the API):
//!
//! ```text
//! S->C  0{"sid":"…","upgrades":[],"pingInterval":25000,"pingTimeout":20000}
//! C->S  3
//! C->S  40/print,{"stationToken":"…"}
//! C->S  42/print,["station-hello",{…}]     <- sent WITHOUT waiting for the ack
//! ---   upstream close 1005                <- the server hangs up
//! ```
//!
//! Socket.IO's server closes the whole client when an EVENT arrives for a
//! namespace it has no socket for yet (`Client._onpacket`: *"no socket for
//! namespace"* → `this.close()`). The `/print` middleware awaits a database
//! lookup before calling `next()`, so the namespace socket does not exist yet
//! when that second frame lands, and the race is lost every single time — four
//! attempts, four failures, `print_stations.last_seen_at` never written. Held
//! the same event back in the proxy until the server's `40/print,{"sid":…}` and
//! the very same client connected, `station-hello` landed, presence was
//! written. That is the whole bug.
//!
//! Meanwhile `ClientBuilder::connect()` returns `Ok` before any ack, so a bad
//! token also looks like a healthy connection. A bridge that believes it is
//! connected never retries and never warns: the till goes quiet and the API
//! keeps answering "queued". The feature exists to delete exactly that failure
//! mode, so a transport that reintroduces it is not a transport we can use.
//!
//! What is left is small and completely specified, so it is written here:
//! the protocol is seven packet types and one grammar, and this module owns it
//! with tests for the shapes the server actually sends.
//!
//! # The grammar
//!
//! A text frame is `<engine.io digit><rest>`:
//!
//! | digit | meaning | who sends it |
//! |-------|---------|--------------|
//! | `0`   | OPEN, with the session JSON | server |
//! | `1`   | CLOSE | either |
//! | `2`   | PING  | server (v4 reversed this from v3) |
//! | `3`   | PONG  | client |
//! | `4`   | MESSAGE — a Socket.IO packet follows | either |
//!
//! Inside a MESSAGE: `<socket.io digit>[<namespace>,][<ack id>][<json>]`, with
//! `0` CONNECT, `1` DISCONNECT, `2` EVENT, `3` ACK, `4` CONNECT_ERROR. The
//! namespace is present only when it is not `/`, and it runs up to the first
//! comma.

use serde::Deserialize;

/// Session parameters from the OPEN packet. `ping_interval`/`ping_timeout` are
/// the server's, and the watchdog is built from them rather than from a
/// constant of ours: if the API changes its heartbeat, the bridge follows.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenPayload {
    pub sid: String,
    #[serde(rename = "pingInterval", default = "default_ping_interval")]
    pub ping_interval: u64,
    #[serde(rename = "pingTimeout", default = "default_ping_timeout")]
    pub ping_timeout: u64,
}

fn default_ping_interval() -> u64 {
    25_000
}
fn default_ping_timeout() -> u64 {
    20_000
}

/// A frame from the server, already classified.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Open(Box<OpenPayloadOwned>),
    /// Server heartbeat. Must be answered with [`pong`] or it hangs up.
    Ping,
    Pong,
    /// Engine.IO close.
    Close,
    /// The namespace accepted us. Everything before this is not yet a
    /// connection, whatever any library says.
    Connected {
        namespace: String,
        sid: String,
    },
    /// The namespace refused us — bad or revoked station token.
    ConnectError {
        namespace: String,
        message: String,
    },
    Event {
        namespace: String,
        name: String,
        args: Vec<serde_json::Value>,
    },
    Disconnected {
        namespace: String,
    },
    /// Anything else: acks we do not use, binary announcements, future types.
    /// Ignored rather than fatal — an unknown frame must not drop the socket.
    Other(String),
}

/// `OpenPayload` without borrows, so `Incoming` can be compared in tests.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenPayloadOwned {
    pub sid: String,
    pub ping_interval: u64,
    pub ping_timeout: u64,
}

/// Splits `<namespace>,<payload>` out of a Socket.IO packet body.
///
/// The namespace is only on the wire when it is not the default one, and it
/// ends at the first comma — which is why the payload is found by that comma
/// and not by looking for `{`: `40/print,` (no payload at all) is a legal
/// frame and the naive parse loses it.
fn split_namespace(rest: &str) -> (String, &str) {
    if let Some(stripped) = rest.strip_prefix('/') {
        match stripped.find(',') {
            Some(idx) => (format!("/{}", &stripped[..idx]), &stripped[idx + 1..]),
            // `40/print` with no comma: namespace, empty payload.
            None => (format!("/{stripped}"), ""),
        }
    } else {
        ("/".to_string(), rest)
    }
}

/// Strips the optional numeric ack id that can sit between the namespace and
/// the payload (`42/print,17["ev",…]`). We never request acks, but the server
/// is free to send one and a parser that chokes on it drops the job.
fn strip_ack_id(payload: &str) -> &str {
    let digits = payload
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .count();
    &payload[digits..]
}

/// Classifies one text frame. Never fails: an unparseable frame becomes
/// [`Incoming::Other`], because dropping the connection over a frame we did not
/// expect would take printing down for a protocol extension.
pub fn parse(frame: &str) -> Incoming {
    let mut chars = frame.chars();
    let Some(kind) = chars.next() else {
        return Incoming::Other(String::new());
    };
    let rest = &frame[kind.len_utf8()..];

    match kind {
        '0' => match serde_json::from_str::<OpenPayload>(rest) {
            Ok(open) => Incoming::Open(Box::new(OpenPayloadOwned {
                sid: open.sid,
                ping_interval: open.ping_interval,
                ping_timeout: open.ping_timeout,
            })),
            Err(_) => Incoming::Other(frame.to_string()),
        },
        '1' => Incoming::Close,
        '2' => Incoming::Ping,
        '3' => Incoming::Pong,
        '4' => parse_socketio(rest, frame),
        _ => Incoming::Other(frame.to_string()),
    }
}

fn parse_socketio(rest: &str, whole: &str) -> Incoming {
    let mut chars = rest.chars();
    let Some(kind) = chars.next() else {
        return Incoming::Other(whole.to_string());
    };
    let body = &rest[kind.len_utf8()..];
    let (namespace, payload) = split_namespace(body);
    let payload = strip_ack_id(payload);

    match kind {
        '0' => {
            let sid = serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|v| v.get("sid").and_then(|s| s.as_str()).map(str::to_string))
                .unwrap_or_default();
            Incoming::Connected { namespace, sid }
        }
        '1' => Incoming::Disconnected { namespace },
        '2' => match serde_json::from_str::<Vec<serde_json::Value>>(payload) {
            Ok(mut args) if !args.is_empty() => {
                let name = match args.remove(0) {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                Incoming::Event {
                    namespace,
                    name,
                    args,
                }
            }
            _ => Incoming::Other(whole.to_string()),
        },
        '4' => {
            let message = serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| payload.to_string());
            Incoming::ConnectError { namespace, message }
        }
        _ => Incoming::Other(whole.to_string()),
    }
}

/// `40<ns>,<auth json>` — the handshake. The auth object is where the station
/// token travels; the server reads nothing else from this side.
pub fn connect_packet(namespace: &str, auth: &serde_json::Value) -> String {
    format!("40{namespace},{auth}")
}

/// `42<ns>,["<event>",<arg>]`.
///
/// **Never send this before the server's CONNECT ack.** That is the exact bug
/// documented at the top of this file: Socket.IO closes the whole connection
/// when an event names a namespace it has not finished joining.
pub fn event_packet(namespace: &str, event: &str, arg: &serde_json::Value) -> String {
    let body = serde_json::Value::Array(vec![
        serde_json::Value::String(event.to_string()),
        arg.clone(),
    ]);
    format!("42{namespace},{body}")
}

/// `41<ns>,` — leave the namespace politely before closing the socket.
pub fn disconnect_packet(namespace: &str) -> String {
    format!("41{namespace},")
}

/// Engine.IO PONG. The v4 server pings and expects this back within
/// `pingTimeout`; silence is a disconnect.
pub fn pong() -> &'static str {
    "3"
}

/// Query string the server expects on the upgrade. `EIO=4` is the protocol
/// version and `transport=websocket` skips the polling handshake — this client
/// is a long-lived native process, not a browser that needs the fallback.
pub const HANDSHAKE_QUERY: &str = "EIO=4&transport=websocket";

/// Turns `https://api.example.com` into the WebSocket URL for the handshake.
pub fn websocket_url(server_url: &str) -> String {
    let base = server_url.trim().trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // No scheme: assume TLS. Guessing plaintext would silently send the
        // station token in the clear.
        format!("wss://{base}")
    };
    format!("{ws_base}/socket.io/?{HANDSHAKE_QUERY}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Verbatim from the wire capture against the running API.
    #[test]
    fn parses_the_open_packet_the_api_sends() {
        let frame = r#"0{"sid":"I6oMfzGZMVERukDXAAAU","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}"#;
        match parse(frame) {
            Incoming::Open(open) => {
                assert_eq!(open.sid, "I6oMfzGZMVERukDXAAAU");
                assert_eq!(open.ping_interval, 25_000);
                assert_eq!(open.ping_timeout, 20_000);
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn parses_the_namespace_connect_ack() {
        assert_eq!(
            parse(r#"40/print,{"sid":"2M_9LF8A05nSMhiiAAAF"}"#),
            Incoming::Connected {
                namespace: "/print".into(),
                sid: "2M_9LF8A05nSMhiiAAAF".into()
            }
        );
    }

    /// The server's own wording, from `stationAuthMiddleware`.
    #[test]
    fn parses_the_connect_error_for_a_revoked_token() {
        assert_eq!(
            parse(r#"44/print,{"message":"Station auth error: token invalid or revoked"}"#),
            Incoming::ConnectError {
                namespace: "/print".into(),
                message: "Station auth error: token invalid or revoked".into()
            }
        );
    }

    #[test]
    fn parses_a_print_job_event() {
        let frame = r#"42/print,["print-job",{"jobId":"abc","copies":1,"escposBase64":"G0A="}]"#;
        match parse(frame) {
            Incoming::Event {
                namespace,
                name,
                args,
            } => {
                assert_eq!(namespace, "/print");
                assert_eq!(name, "print-job");
                assert_eq!(args.len(), 1);
                assert_eq!(args[0]["jobId"], "abc");
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn heartbeats_and_close_are_not_confused_with_messages() {
        assert_eq!(parse("2"), Incoming::Ping);
        assert_eq!(parse("3"), Incoming::Pong);
        assert_eq!(parse("1"), Incoming::Close);
        assert_eq!(
            parse("41/print,"),
            Incoming::Disconnected {
                namespace: "/print".into()
            }
        );
    }

    #[test]
    fn an_event_carrying_an_ack_id_still_parses() {
        // We never ask for acks, but the server may number a packet and a
        // parser that trips on the digits drops the job on the floor.
        match parse(r#"42/print,17["print-job",{"jobId":"x"}]"#) {
            Incoming::Event { name, args, .. } => {
                assert_eq!(name, "print-job");
                assert_eq!(args[0]["jobId"], "x");
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn a_frame_we_do_not_understand_is_ignored_not_fatal() {
        assert!(matches!(parse("46/print,[]"), Incoming::Other(_)));
        assert!(matches!(parse(""), Incoming::Other(_)));
        assert!(matches!(parse("9"), Incoming::Other(_)));
        // A truncated OPEN must not be mistaken for a good one.
        assert!(matches!(parse("0{not json"), Incoming::Other(_)));
    }

    #[test]
    fn the_default_namespace_has_no_prefix_on_the_wire() {
        assert_eq!(
            parse(r#"42["hello",{}]"#),
            Incoming::Event {
                namespace: "/".into(),
                name: "hello".into(),
                args: vec![json!({})]
            }
        );
    }

    #[test]
    fn encodes_exactly_what_the_capture_shows() {
        assert_eq!(
            connect_packet("/print", &json!({"stationToken": "tok"})),
            r#"40/print,{"stationToken":"tok"}"#
        );
        assert_eq!(
            event_packet("/print", "print-result", &json!({"jobId":"a","ok":true})),
            r#"42/print,["print-result",{"jobId":"a","ok":true}]"#
        );
        assert_eq!(disconnect_packet("/print"), "41/print,");
        assert_eq!(pong(), "3");
    }

    #[test]
    fn builds_the_websocket_url_for_either_scheme() {
        assert_eq!(
            websocket_url("http://localhost:4000"),
            "ws://localhost:4000/socket.io/?EIO=4&transport=websocket"
        );
        assert_eq!(
            websocket_url("https://api-gourmelyhub.busticco.com/"),
            "wss://api-gourmelyhub.busticco.com/socket.io/?EIO=4&transport=websocket"
        );
        // No scheme must never downgrade to plaintext: the station token is a
        // bearer credential.
        assert!(websocket_url("api.example.com").starts_with("wss://"));
    }
}
