//! Wire-crate codec round-trip, truncation, fuzz-like mutation, random-input, and
//! dependency-boundary tests.

#![allow(non_snake_case)]

use abacus_core::error::{Condition, ProtocolFault};
use abacus_tests::Rng;
use abacus_wire::{
    decode_request, decode_response, encode_request, encode_response, expected_fd_count, Request,
    Response, LENGTH_PREFIX_BYTES, MAX_MESSAGE_SIZE, PROTOCOL_VERSION,
};

fn every_request() -> Vec<Request> {
    vec![
        Request::CreateInterlock {
            name: "cam0".into(),
            tier: 0,
            watched_name: None,
            watched_word: None,
            interval_ns: None,
            conditions: None,
        },
        Request::CreateInterlock {
            name: "frame_done".into(),
            tier: 1,
            watched_name: Some("cam0".into()),
            watched_word: Some(1),
            interval_ns: None,
            conditions: None,
        },
        Request::CreateInterlock {
            name: "timeout".into(),
            tier: 2,
            watched_name: None,
            watched_word: None,
            interval_ns: None,
            conditions: None,
        },
        Request::CreateInterlock {
            name: "cron".into(),
            tier: 3,
            watched_name: None,
            watched_word: None,
            interval_ns: Some(33_000_000),
            conditions: None,
        },
        Request::CreateInterlock {
            name: "barrier".into(),
            tier: 4,
            watched_name: None,
            watched_word: None,
            interval_ns: None,
            conditions: Some(vec![("cam0".into(), 1, 100), ("cam1".into(), 0, 50)]),
        },
        Request::AttachInterlock {
            name: "sensor".into(),
        },
    ]
}

fn every_response() -> Vec<Response> {
    vec![
        Response::Created { id: 42 },
        Response::Attached { id: 7 },
        Response::from_condition(&Condition::InterlockReaped),
        Response::from_condition(&Condition::InterlockNotFound {
            name: "missing".into(),
        }),
        Response::allocation_failed("memfd"),
        Response::invalid_request("bad tier"),
    ]
}

/// CONTRACTS.md wire ABI: every request variant survives encode then decode.
#[test]
fn wire__round_trip_every_request_variant() {
    for req in every_request() {
        let frame = encode_request(&req).expect("encode");
        assert_eq!(
            frame[LENGTH_PREFIX_BYTES], PROTOCOL_VERSION,
            "version byte leads: {frame:?}"
        );
        let decoded = decode_request(&frame[LENGTH_PREFIX_BYTES..]).expect("decode");
        assert_eq!(decoded, req);
    }
}

/// CONTRACTS.md wire ABI: every response variant survives encode then decode and
/// carries the right fd count.
#[test]
fn wire__round_trip_every_response_variant() {
    for resp in every_response() {
        let frame = encode_response(&resp).expect("encode");
        let decoded = decode_response(&frame[LENGTH_PREFIX_BYTES..]).expect("decode");
        assert_eq!(decoded, resp);
        let fds = expected_fd_count(&resp);
        assert_eq!(
            fds,
            if matches!(resp, Response::Error { .. }) {
                0
            } else {
                1
            }
        );
    }
}

/// Every strict prefix of every valid frame decodes to Truncated with
/// needed > have, and nothing panics.
#[test]
fn wire__truncation_at_every_offset_never_panics() {
    for req in every_request() {
        let frame = encode_request(&req).expect("encode");
        let payload = &frame[LENGTH_PREFIX_BYTES..];
        for cut in 0..payload.len() {
            match decode_request(&payload[..cut]) {
                Err(ProtocolFault::Truncated { needed, have }) => {
                    assert!(
                        needed > have,
                        "{req:?} cut at {cut}: needed {needed} <= have {have}"
                    )
                }
                other => panic!("{req:?} cut at {cut}: {other:?}"),
            }
        }
    }
    for resp in every_response() {
        let frame = encode_response(&resp).expect("encode");
        let payload = &frame[LENGTH_PREFIX_BYTES..];
        for cut in 0..payload.len() {
            match decode_response(&payload[..cut]) {
                Err(ProtocolFault::Truncated { needed, have }) => {
                    assert!(
                        needed > have,
                        "{resp:?} cut at {cut}: needed {needed} <= have {have}"
                    )
                }
                other => panic!("{resp:?} cut at {cut}: {other:?}"),
            }
        }
    }
}

/// 10 000 valid frames with 1 to 4 random byte flips decode without panicking.
#[test]
fn wire__random_mutation_never_panics() {
    let mut rng = Rng::from_env(0x5EED_0001);
    let requests = every_request();
    let responses = every_response();
    for i in 0..10_000 {
        let pick = rng.below((requests.len() + responses.len()) as u64) as usize;
        let mut payload = if pick < requests.len() {
            encode_request(&requests[pick]).expect("encode")
        } else {
            encode_response(&responses[pick - requests.len()]).expect("encode")
        };
        payload.drain(..LENGTH_PREFIX_BYTES);
        let flips = 1 + rng.below(4) as usize;
        for _ in 0..flips {
            let at = rng.below(payload.len() as u64) as usize;
            payload[at] ^= 1 << rng.below(8);
        }
        let outcome = std::panic::catch_unwind(|| {
            let _ = decode_request(&payload);
            let _ = decode_response(&payload);
        });
        assert!(
            outcome.is_ok(),
            "decoder panicked on mutation {i} (seed {}): {payload:?}",
            rng.seed()
        );
    }
}

/// 10 000 random byte strings of length 0 to MAX_MESSAGE_SIZE decode without panicking.
#[test]
fn wire__random_bytes_never_panic() {
    let mut rng = Rng::from_env(0x5EED_0002);
    for i in 0..10_000 {
        let len = rng.below(MAX_MESSAGE_SIZE as u64 + 1) as usize;
        let bytes = rng.bytes(len);
        let outcome = std::panic::catch_unwind(|| {
            let _ = decode_request(&bytes);
            let _ = decode_response(&bytes);
        });
        assert!(
            outcome.is_ok(),
            "decoder panicked on random input {i} (seed {})",
            rng.seed()
        );
    }
}

/// `cargo tree -p abacus-client` has no `abacus-daemon`: the client manifest names
/// core and wire only.
#[test]
fn wire__client_manifest_has_no_daemon_dependency() {
    let manifest = include_str!("../../abacus-client/Cargo.toml");
    let daemon_lines: Vec<_> = manifest
        .lines()
        .filter(|l| l.trim_start().starts_with("abacus-daemon"))
        .collect();
    assert!(
        daemon_lines.is_empty(),
        "abacus-client still depends on abacus-daemon: {daemon_lines:?}"
    );
    assert!(
        manifest
            .lines()
            .any(|l| l.trim_start().starts_with("abacus-wire")),
        "abacus-client does not depend on abacus-wire"
    );
    let daemon_manifest = include_str!("../../abacus-daemon/Cargo.toml");
    assert!(
        daemon_manifest
            .lines()
            .any(|l| l.trim_start().starts_with("abacus-wire")),
        "abacus-daemon does not depend on abacus-wire: one codec must serve both sides"
    );
}
