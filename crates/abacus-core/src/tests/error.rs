//! Error vocabulary: every variant of every enum displays, and the conversions hold.

use crate::error::{
    AllocationStep, Condition, IoOperation, ProtocolFault, StartupError, TransportError,
};

fn shown<T: std::fmt::Display>(label: &str, v: T, must_contain: &str) {
    let s = v.to_string();
    assert!(!s.is_empty(), "{label} displays as an empty string");
    assert!(
        s.contains(must_contain),
        "{label} displays as {s:?}, missing {must_contain:?}"
    );
}

/// CONTRACTS.md errors table plus the transport and startup vocabulary: one Display
/// assertion per variant of Condition, ProtocolFault, TransportError (both
/// SocketPathOccupied arms), and StartupError. SdkError is covered in abacus-client.
#[test]
fn error__every_variant_displays() {
    shown(
        "Condition::InterlockReaped",
        Condition::InterlockReaped,
        "InterlockReaped",
    );
    shown(
        "Condition::InterlockNotFound",
        Condition::InterlockNotFound {
            name: "cam0".into(),
        },
        "cam0",
    );
    for step in [
        AllocationStep::MemfdCreate,
        AllocationStep::Ftruncate,
        AllocationStep::Mmap,
    ] {
        shown(
            "Condition::AllocationFailed",
            Condition::AllocationFailed { step, errno: 24 },
            "errno=24",
        );
        assert!(
            Condition::AllocationFailed { step, errno: 24 }
                .to_string()
                .contains(&format!("{step:?}")),
            "AllocationFailed does not name its step {step:?}"
        );
    }
    shown(
        "Condition::InvalidRequest",
        Condition::InvalidRequest {
            message: "bad tier".into(),
        },
        "bad tier",
    );

    shown(
        "ProtocolFault::Truncated",
        ProtocolFault::Truncated {
            needed: 10,
            have: 4,
        },
        "10",
    );
    shown(
        "ProtocolFault::FrameTooLarge",
        ProtocolFault::FrameTooLarge {
            len: 5000,
            max: 4096,
        },
        "4096",
    );
    shown(
        "ProtocolFault::UnsupportedVersion",
        ProtocolFault::UnsupportedVersion { version: 9 },
        "9",
    );
    shown(
        "ProtocolFault::UnknownTag",
        ProtocolFault::UnknownTag { tag: 0x7f },
        "0x7f",
    );
    shown(
        "ProtocolFault::InvalidUtf8",
        ProtocolFault::InvalidUtf8,
        "UTF-8",
    );
    shown(
        "ProtocolFault::InvalidFlag",
        ProtocolFault::InvalidFlag {
            field: "owner",
            value: 2,
        },
        "owner",
    );

    shown(
        "TransportError::Protocol",
        TransportError::Protocol {
            fault: ProtocolFault::InvalidUtf8,
        },
        "protocol fault",
    );
    for op in [
        IoOperation::Bind,
        IoOperation::Listen,
        IoOperation::Accept,
        IoOperation::Connect,
        IoOperation::SendMsg,
        IoOperation::RecvMsg,
        IoOperation::Read,
        IoOperation::Write,
        IoOperation::Stat,
        IoOperation::Unlink,
        IoOperation::SetNonBlocking,
        IoOperation::Poll,
    ] {
        let e = TransportError::Io {
            operation: op,
            errno: 11,
        };
        shown("TransportError::Io", e.clone(), "errno 11");
        assert!(
            e.to_string().contains(&format!("{op:?}")),
            "Io does not name {op:?}"
        );
    }
    shown(
        "TransportError::ConnectionClosed",
        TransportError::ConnectionClosed,
        "closed",
    );
    shown(
        "TransportError::SocketPathOccupied live",
        TransportError::SocketPathOccupied {
            path: "/tmp/x.sock".into(),
            live_daemon: true,
        },
        "live daemon",
    );
    shown(
        "TransportError::SocketPathOccupied non-socket",
        TransportError::SocketPathOccupied {
            path: "/tmp/x.sock".into(),
            live_daemon: false,
        },
        "non-socket",
    );

    shown(
        "StartupError::Transport",
        StartupError::Transport(TransportError::ConnectionClosed),
        "transport",
    );
    shown(
        "StartupError::Allocation",
        StartupError::Allocation(Condition::InterlockReaped),
        "allocation",
    );
    shown(
        "StartupError::EventLoopFailed",
        StartupError::EventLoopFailed { errno: 22 },
        "errno=22",
    );
}

/// StartupError wraps both failure sources through From, preserving the inner value.
#[test]
fn error__startup_error_from_transport_and_condition() {
    let t: StartupError = TransportError::ConnectionClosed.into();
    assert!(
        matches!(t, StartupError::Transport(TransportError::ConnectionClosed)),
        "got {t:?}"
    );
    let c: StartupError = Condition::InterlockReaped.into();
    assert!(
        matches!(c, StartupError::Allocation(Condition::InterlockReaped)),
        "got {c:?}"
    );
}

/// Condition, ProtocolFault, and TransportError compare by value so tests can assert_eq.
#[test]
fn error__value_equality_holds() {
    assert_eq!(Condition::InterlockReaped, Condition::InterlockReaped);
    assert_ne!(
        Condition::InterlockNotFound { name: "a".into() },
        Condition::InterlockNotFound { name: "b".into() }
    );
    assert_eq!(
        ProtocolFault::Truncated { needed: 2, have: 1 },
        ProtocolFault::Truncated { needed: 2, have: 1 }
    );
    assert_ne!(
        TransportError::Io {
            operation: IoOperation::Read,
            errno: 1,
        },
        TransportError::ConnectionClosed
    );
}

/// Every error type implements std::error::Error (boxable, source-less).
#[test]
fn error__types_are_std_errors() {
    let boxed: Vec<Box<dyn std::error::Error>> = vec![
        Box::new(Condition::InterlockReaped),
        Box::new(TransportError::ConnectionClosed),
        Box::new(StartupError::EventLoopFailed { errno: 1 }),
    ];
    for e in &boxed {
        assert!(e.source().is_none(), "{e} unexpectedly reports a source");
        assert!(!e.to_string().is_empty());
    }
}
