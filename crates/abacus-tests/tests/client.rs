//! L1: AbacusClient contract (SURFACE.md "Connection"), daemon as a thread.

#![allow(non_snake_case)]

use std::path::Path;
use std::time::Duration;

use abacus_client::{AbacusClient, SdkError};
use abacus_core::interlock::interlock_create;
use abacus_tests::{
    unique_socket_path, wait_for, FakeDaemon, RawClient, RawResponse, ThreadDaemon,
    ERR_ALLOCATION_FAILED, ERR_INTERLOCK_NOT_FOUND, ERR_INTERLOCK_REAPED, ERR_INVALID_REQUEST,
    TAG_ATTACH, TAG_CREATE, TIER_INTERLOCK,
};

/// SURFACE.md Connection: connect auto-attaches "clock"; LIFECYCLE.md clock advancement:
/// open_count is current monotonic ms and advances every cycle.
#[test]
fn client__connect_attaches_clock_and_clock_advances() {
    let d = ThreadDaemon::start("clock-adv");
    let client = d.client();
    let t1 = client.clock().now_ms();
    let elapsed = wait_for(Duration::from_millis(200), Duration::from_millis(1), || {
        client.clock().now_ms() > t1
    })
    .unwrap_or_else(|e| panic!("clock never advanced past {t1}: {e}"));
    assert!(
        elapsed < Duration::from_millis(50),
        "clock took {elapsed:?} to tick once"
    );
    let (now, start) = client.clock().peek();
    assert!(
        start > 0 && now >= start,
        "clock peek now={now} start={start}"
    );
    assert_eq!(client.clock().uptime_ms(), (now - start) as i64);
}

/// SURFACE.md Connection: a missing socket is a transport error, not a panic.
#[test]
fn client__connect_to_missing_socket_is_transport_error() {
    let path = unique_socket_path("missing");
    match AbacusClient::connect(&path) {
        Err(SdkError::Transport(_)) => {}
        Err(other) => panic!("expected Transport error, got {other}"),
        Ok(_) => panic!("connect to a missing socket succeeded"),
    }
}

/// CONTRACTS.md UDS surface: "clock" is reserved. The daemon itself rejects create("clock")
/// with InvalidRequest (the SDK never sends it, so this goes over the raw wire).
#[test]
fn client__create_clock_rejected_by_daemon() {
    let d = ThreadDaemon::start("create-clock");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    let (resp, fds) = raw
        .create("clock", TIER_INTERLOCK, None, None, None)
        .expect("create round trip");
    assert!(fds.is_empty(), "error response carried {} fds", fds.len());
    match resp {
        RawResponse::Error { code, message } => {
            assert_eq!(
                code, ERR_INVALID_REQUEST,
                "wrong error code; message={message}"
            );
            assert!(
                message.contains("reserved"),
                "message does not say reserved: {message}"
            );
        }
        other => panic!("create(\"clock\") was accepted: {other:?}"),
    }
    // The daemon is still serving after the rejection.
    let client = AbacusClient::connect(Path::new(d.socket_path())).expect("connect after reject");
    assert!(client.is_connected());
}

/// SURFACE.md Connection and Clock: attach_interlock("clock") is refused by the SDK itself
/// with InvalidRequest and never reaches the wire. A scripted fake daemon shows that the
/// next frame on the wire is the create that follows, not an attach.
#[test]
fn client__attach_clock_by_name_rejected_client_side() {
    let fake = FakeDaemon::bind("clock-cs");
    let path = fake.socket_path().to_path_buf();
    let server = std::thread::spawn(move || -> Result<Vec<u8>, String> {
        let mut conn = fake.accept(Duration::from_secs(5))?;
        let clock = interlock_create().map_err(|e| e.to_string())?;
        let first = conn.recv_request()?;
        if first.get(1) != Some(&TAG_ATTACH) {
            return Err(format!("first frame is not the clock attach: {first:?}"));
        }
        conn.send_attached(0, &[clock.as_raw_fd()])
            .map_err(|e| e.to_string())?;
        let next = conn.recv_request()?;
        conn.send_error(ERR_INVALID_REQUEST, "scripted")
            .map_err(|e| e.to_string())?;
        Ok(next)
    });
    let mut client = AbacusClient::connect(&path).expect("connect to fake daemon");
    match client.attach_interlock("clock") {
        Err(SdkError::InvalidRequest { message }) => assert!(
            message.contains("client.clock()"),
            "rejection does not point at client.clock(): {message}"
        ),
        Err(other) => panic!("attach_interlock(\"clock\") returned the wrong error: {other}"),
        Ok(_) => panic!("attach_interlock(\"clock\") was accepted"),
    }
    // The next request the fake daemon sees must be this create, not an attach.
    let _ = client.create_interlock("after");
    let next = server
        .join()
        .expect("fake daemon thread panicked")
        .expect("fake daemon script");
    assert_eq!(
        next.get(1),
        Some(&TAG_CREATE),
        "frame after the rejected attach is not a create: {next:?}"
    );
    assert!(
        next.windows(5).any(|w| w == b"after"),
        "create frame does not carry the expected name: {next:?}"
    );
}

/// after a transport error (timeout), the connection is poisoned and every subsequent
/// call fails immediately instead of reading the previous call's stale response. A fake
/// daemon delays its response past the client's transport timeout, then sends it; the next
/// SDK call must not silently consume that stale reply.
#[test]
fn client__poisoned_after_transport_error() {
    let fake = FakeDaemon::bind("poison");
    let path = fake.socket_path().to_path_buf();

    let server = std::thread::spawn(move || -> Result<(), String> {
        let mut conn = fake.accept(Duration::from_secs(5))?;

        // Handle the automatic clock attach that connect() performs.
        let clock = interlock_create().map_err(|e| e.to_string())?;
        let req = conn.recv_request()?;
        if req.get(1) != Some(&TAG_ATTACH) {
            return Err(format!("expected clock attach, got {req:?}"));
        }
        conn.send_attached(0, &[clock.as_raw_fd()])
            .map_err(|e| e.to_string())?;

        // Receive the first create request, then delay past the client's transport
        // timeout before sending the response. The stale response lands on the socket
        // after the client has already given up on this call.
        let req = conn.recv_request()?;
        if req.get(1) != Some(&TAG_CREATE) {
            return Err(format!("expected create request, got {req:?}"));
        }
        std::thread::sleep(Duration::from_millis(300));
        let il = interlock_create().map_err(|e| e.to_string())?;
        conn.send_created(1, &[il.as_raw_fd()])
            .map_err(|e| e.to_string())?;

        // The client may or may not send a second request depending on whether
        // the fix is applied. Do not fail either way.
        let _ = conn.recv_request();
        Ok(())
    });

    // A short transport timeout so the first create hits ETIMEDOUT quickly.
    let mut client = AbacusClient::connect_with_timeout(&path, Duration::from_millis(100))
        .expect("connect to fake daemon");

    // First create: the fake daemon delays past the timeout, so this fails.
    let first = client.create_interlock("first");
    assert!(first.is_err(), "first create should time out");

    // Wait long enough for the stale response to land on the socket.
    std::thread::sleep(Duration::from_millis(400));

    // Second create: before the fix, send_recv reads the stale response from the
    // first create and succeeds (silently returning the wrong interlock). After the
    // fix, the connection is poisoned and this fails immediately.
    let second = client.create_interlock("second");
    assert!(
        second.is_err(),
        "second call after transport error must fail (connection should be poisoned), \
         but it succeeded: the client silently read a stale response"
    );

    server
        .join()
        .expect("server thread panicked")
        .expect("server script");
}

/// `client.rs` parse_daemon_error and CONTRACTS.md wire ABI error codes: 0x01 InterlockReaped,
/// 0x02 InterlockNotFound, 0x03 AllocationFailed, 0x04 InvalidRequest map to the named
/// SdkError variants; an unknown code is UnexpectedResponse. The real daemon never emits
/// 0x01, so a scripted fake daemon supplies every code.
#[test]
fn client__daemon_error_codes_map_to_sdk_errors() {
    const CODES: [u8; 5] = [
        ERR_INTERLOCK_REAPED,
        ERR_INTERLOCK_NOT_FOUND,
        ERR_ALLOCATION_FAILED,
        ERR_INVALID_REQUEST,
        0x7f,
    ];
    let fake = FakeDaemon::bind("errmap");
    let path = fake.socket_path().to_path_buf();
    let server = std::thread::spawn(move || -> Result<(), String> {
        let mut conn = fake.accept(Duration::from_secs(5))?;
        let clock = interlock_create().map_err(|e| e.to_string())?;
        let first = conn.recv_request()?;
        if first.get(1) != Some(&TAG_ATTACH) {
            return Err(format!("first frame is not the clock attach: {first:?}"));
        }
        conn.send_attached(0, &[clock.as_raw_fd()])
            .map_err(|e| e.to_string())?;
        for code in CODES {
            let req = conn.recv_request()?;
            if req.get(1) != Some(&TAG_CREATE) {
                return Err(format!("expected a create, got {req:?}"));
            }
            conn.send_error(code, &format!("scripted-{code:#04x}"))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    });
    let mut client = AbacusClient::connect(&path).expect("connect to fake daemon");
    for code in CODES {
        let r = client.create_interlock("x");
        let err = match r {
            Ok(_) => panic!("code {code:#04x}: create succeeded"),
            Err(e) => e,
        };
        let expected_message = format!("scripted-{code:#04x}");
        match (code, &err) {
            (ERR_INTERLOCK_REAPED, SdkError::InterlockReaped) => {}
            (ERR_INTERLOCK_NOT_FOUND, SdkError::InterlockNotFound { name }) => {
                assert_eq!(name, &expected_message, "message not carried as name")
            }
            (ERR_ALLOCATION_FAILED, SdkError::AllocationFailed { message }) => {
                assert_eq!(message, &expected_message)
            }
            (ERR_INVALID_REQUEST, SdkError::InvalidRequest { message }) => {
                assert_eq!(message, &expected_message)
            }
            (0x7f, SdkError::UnexpectedResponse { message }) => assert!(
                message.contains("0x7f"),
                "unknown-code message does not name the code: {message}"
            ),
            _ => panic!("code {code:#04x} mapped to {err:?}"),
        }
    }
    server
        .join()
        .expect("fake daemon thread panicked")
        .expect("fake daemon script");
}
