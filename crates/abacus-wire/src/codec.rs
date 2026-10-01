//! Request and response encoding for wire ABI v2.
//!
//! Frame: u32 LE length prefix, then payload. Payload: version byte, tag byte, fields.
//! Strings: u16 LE length prefix, UTF-8 bytes. Integers: u64 LE.

use abacus_core::error::{Condition, ProtocolFault};

/// Bytes in the frame length prefix.
pub const LENGTH_PREFIX_BYTES: usize = 4;
/// Maximum payload length in bytes.
pub const MAX_MESSAGE_SIZE: usize = 4096;
/// Maximum bytes in one frame: prefix plus payload.
pub const MAX_FRAME_BYTES: usize = LENGTH_PREFIX_BYTES + MAX_MESSAGE_SIZE;
/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u8 = 2;

const TAG_CREATE_INTERLOCK: u8 = 0x01;
const TAG_ATTACH_INTERLOCK: u8 = 0x02;

const TAG_CREATED: u8 = 0x81;
const TAG_ATTACHED: u8 = 0x82;
const TAG_ERROR: u8 = 0x88;

/// Error code: InterlockReaped. The daemon returns it when a create's owner is gone or replaced.
pub const ERR_INTERLOCK_REAPED: u8 = 0x01;
/// Error code: InterlockNotFound.
pub const ERR_INTERLOCK_NOT_FOUND: u8 = 0x02;
/// Error code: AllocationFailed.
pub const ERR_ALLOCATION_FAILED: u8 = 0x03;
/// Error code: InvalidRequest.
pub const ERR_INVALID_REQUEST: u8 = 0x04;

// -- Request --

/// A client request.
///
/// `CreateInterlock` carries a tier (0 = Interlock, 1 = WaitCounter, 2 = WaitTimer,
/// 3 = WaitCron, 4 = WaitBarrier) and the fields that tier requires:
/// WaitCounter `watched_name` and `watched_word` (0 = open_count, 1 = closed_count),
/// WaitCron `interval_ns`, WaitBarrier `conditions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Create a named interlock with a tier. Answered with `Created` plus one fd.
    CreateInterlock {
        /// The interlock name.
        name: String,
        /// The tier discriminant.
        tier: u8,
        /// The ProcessClock that owns this interlock: name and registry id.
        owner: Option<(String, u64)>,
        /// Names this interlock dies with, resolved at create.
        dependencies: Vec<String>,
        /// WaitCounter (tier 1): the interlock to watch.
        watched_name: Option<String>,
        /// WaitCounter (tier 1): which word of it to watch.
        watched_word: Option<u8>,
        /// WaitCron (tier 3): grid interval in nanoseconds.
        interval_ns: Option<u64>,
        /// WaitBarrier (tier 4): list of (watched_name, watched_word, threshold).
        conditions: Option<Vec<(String, u8, u64)>>,
    },
    /// Attach to an existing interlock by name. Answered with `Attached` plus one fd.
    AttachInterlock {
        /// The interlock name.
        name: String,
    },
}

// -- Response --

/// A daemon response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// The interlock was created; one fd accompanies this frame.
    Created {
        /// The registry id.
        id: u64,
    },
    /// The interlock was found; one fd accompanies this frame.
    Attached {
        /// The registry id.
        id: u64,
        /// The tier of the attached interlock.
        tier: u8,
    },
    /// The request failed. No fd.
    Error {
        /// One of the `ERR_*` codes.
        code: u8,
        /// Human-readable detail.
        message: String,
    },
}

impl Response {
    /// The error response for a daemon-domain condition.
    pub fn from_condition(condition: &Condition) -> Self {
        let (code, message) = match condition {
            Condition::InterlockReaped => (ERR_INTERLOCK_REAPED, condition.to_string()),
            Condition::InterlockNotFound { name } => (ERR_INTERLOCK_NOT_FOUND, name.clone()),
            Condition::AllocationFailed { .. } => (ERR_ALLOCATION_FAILED, condition.to_string()),
            Condition::InvalidRequest { message } => (ERR_INVALID_REQUEST, message.clone()),
        };
        Self::Error { code, message }
    }

    /// An `InvalidRequest` error response.
    pub fn invalid_request(message: &str) -> Self {
        Self::Error {
            code: ERR_INVALID_REQUEST,
            message: message.to_string(),
        }
    }

    /// An `AllocationFailed` error response.
    pub fn allocation_failed(message: &str) -> Self {
        Self::Error {
            code: ERR_ALLOCATION_FAILED,
            message: message.to_string(),
        }
    }
}

/// Number of fds that accompany a response: 1 for Created and Attached, 0 for Error.
pub fn expected_fd_count(response: &Response) -> usize {
    match response {
        Response::Created { .. } | Response::Attached { .. } => 1,
        Response::Error { .. } => 0,
    }
}

// -- Encoding --

/// Encode a request as a complete frame (prefix plus payload).
pub fn encode_request(req: &Request) -> Result<Vec<u8>, ProtocolFault> {
    let mut payload = vec![PROTOCOL_VERSION];
    match req {
        Request::CreateInterlock {
            name,
            tier,
            owner,
            dependencies,
            watched_name,
            watched_word,
            interval_ns,
            conditions,
        } => {
            payload.push(TAG_CREATE_INTERLOCK);
            encode_string_into(&mut payload, name)?;
            payload.push(*tier);
            match owner {
                Some((oname, oid)) => {
                    payload.push(1);
                    encode_string_into(&mut payload, oname)?;
                    payload.extend_from_slice(&oid.to_le_bytes());
                }
                None => {
                    payload.push(0);
                }
            }
            let dep_count =
                u16::try_from(dependencies.len()).map_err(|_| ProtocolFault::FrameTooLarge {
                    len: dependencies.len(),
                    max: u16::MAX as usize,
                })?;
            payload.extend_from_slice(&dep_count.to_le_bytes());
            for dep in dependencies {
                encode_string_into(&mut payload, dep)?;
            }
            match *tier {
                1 => {
                    let wn = watched_name.as_ref().ok_or(ProtocolFault::MissingField {
                        field: "watched_name",
                    })?;
                    let ww = watched_word.ok_or(ProtocolFault::MissingField {
                        field: "watched_word",
                    })?;
                    encode_string_into(&mut payload, wn)?;
                    payload.push(ww);
                }
                3 => {
                    let ival = interval_ns.ok_or(ProtocolFault::MissingField {
                        field: "interval_ns",
                    })?;
                    payload.extend_from_slice(&ival.to_le_bytes());
                }
                4 => {
                    let conds = conditions.as_ref().ok_or(ProtocolFault::MissingField {
                        field: "conditions",
                    })?;
                    let count =
                        u16::try_from(conds.len()).map_err(|_| ProtocolFault::FrameTooLarge {
                            len: conds.len(),
                            max: u16::MAX as usize,
                        })?;
                    payload.extend_from_slice(&count.to_le_bytes());
                    for (wn, ww, threshold) in conds {
                        encode_string_into(&mut payload, wn)?;
                        payload.push(*ww);
                        payload.extend_from_slice(&threshold.to_le_bytes());
                    }
                }
                _ => {
                    // Tier 0 (Interlock) and tier 2 (WaitTimer): no extra fields.
                }
            }
        }
        Request::AttachInterlock { name } => {
            payload.push(TAG_ATTACH_INTERLOCK);
            encode_string_into(&mut payload, name)?;
        }
    }
    frame(payload)
}

/// Encode a response as a complete frame (prefix plus payload).
pub fn encode_response(resp: &Response) -> Result<Vec<u8>, ProtocolFault> {
    let mut payload = vec![PROTOCOL_VERSION];
    match resp {
        Response::Created { id } => {
            payload.push(TAG_CREATED);
            payload.extend_from_slice(&id.to_le_bytes());
        }
        Response::Attached { id, tier } => {
            payload.push(TAG_ATTACHED);
            payload.extend_from_slice(&id.to_le_bytes());
            payload.push(*tier);
        }
        Response::Error { code, message } => {
            payload.push(TAG_ERROR);
            payload.push(*code);
            encode_string_into(&mut payload, message)?;
        }
    }
    frame(payload)
}

fn frame(payload: Vec<u8>) -> Result<Vec<u8>, ProtocolFault> {
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(ProtocolFault::FrameTooLarge {
            len: payload.len(),
            max: MAX_MESSAGE_SIZE,
        });
    }
    let len = payload.len() as u32;
    let mut frame = len.to_le_bytes().to_vec();
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn encode_string_into(buf: &mut Vec<u8>, s: &str) -> Result<(), ProtocolFault> {
    let bytes = s.as_bytes();
    if bytes.len() > u16::MAX as usize {
        return Err(ProtocolFault::FrameTooLarge {
            len: bytes.len(),
            max: u16::MAX as usize,
        });
    }
    buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(bytes);
    Ok(())
}

// -- Decoding --

/// A cursor over a payload. Every read reports the exact absolute byte count it needed, so a
/// `Truncated` fault's `needed` is the payload length that would have satisfied that read.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolFault> {
        let needed = self.pos + n;
        if self.data.len() < needed {
            return Err(ProtocolFault::Truncated {
                needed,
                have: self.data.len(),
            });
        }
        let out = &self.data[self.pos..needed];
        self.pos = needed;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, ProtocolFault> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtocolFault> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u64(&mut self) -> Result<u64, ProtocolFault> {
        let b = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(b);
        Ok(u64::from_le_bytes(arr))
    }

    fn string(&mut self) -> Result<String, ProtocolFault> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtocolFault::InvalidUtf8)
    }
}

fn check_version(cur: &mut Cursor<'_>) -> Result<(), ProtocolFault> {
    let version = cur.u8()?;
    if version != PROTOCOL_VERSION {
        return Err(ProtocolFault::UnsupportedVersion { version });
    }
    Ok(())
}

/// Decode a request payload (the bytes after the length prefix).
pub fn decode_request(data: &[u8]) -> Result<Request, ProtocolFault> {
    let mut cur = Cursor::new(data);
    check_version(&mut cur)?;
    let tag = cur.u8()?;
    match tag {
        TAG_CREATE_INTERLOCK => {
            let name = cur.string()?;
            let tier = cur.u8()?;
            let owner_flag = cur.u8()?;
            let owner = match owner_flag {
                0 => None,
                1 => {
                    let oname = cur.string()?;
                    let oid = cur.u64()?;
                    Some((oname, oid))
                }
                other => {
                    return Err(ProtocolFault::InvalidFlag {
                        field: "owner",
                        value: other,
                    });
                }
            };
            let dep_count = cur.u16()? as usize;
            let mut dependencies = Vec::with_capacity(dep_count.min(MAX_MESSAGE_SIZE / 2));
            for _ in 0..dep_count {
                dependencies.push(cur.string()?);
            }
            let mut watched_name = None;
            let mut watched_word = None;
            let mut interval_ns = None;
            let mut conditions = None;
            match tier {
                1 => {
                    watched_name = Some(cur.string()?);
                    watched_word = Some(cur.u8()?);
                }
                3 => {
                    interval_ns = Some(cur.u64()?);
                }
                4 => {
                    let count = cur.u16()? as usize;
                    let mut conds = Vec::with_capacity(count.min(MAX_MESSAGE_SIZE / 11));
                    for _ in 0..count {
                        let wn = cur.string()?;
                        let ww = cur.u8()?;
                        let threshold = cur.u64()?;
                        conds.push((wn, ww, threshold));
                    }
                    conditions = Some(conds);
                }
                _ => {
                    // Tier 0 (Interlock), tier 2 (WaitTimer), and unknown tiers: no extra
                    // fields. The daemon rejects unknown tiers as InvalidRequest.
                }
            }
            Ok(Request::CreateInterlock {
                name,
                tier,
                owner,
                dependencies,
                watched_name,
                watched_word,
                interval_ns,
                conditions,
            })
        }
        TAG_ATTACH_INTERLOCK => {
            let name = cur.string()?;
            Ok(Request::AttachInterlock { name })
        }
        _ => Err(ProtocolFault::UnknownTag { tag }),
    }
}

/// Decode a response payload (the bytes after the length prefix).
pub fn decode_response(data: &[u8]) -> Result<Response, ProtocolFault> {
    let mut cur = Cursor::new(data);
    check_version(&mut cur)?;
    let tag = cur.u8()?;
    match tag {
        TAG_CREATED => Ok(Response::Created { id: cur.u64()? }),
        TAG_ATTACHED => {
            let id = cur.u64()?;
            let tier = cur.u8()?;
            Ok(Response::Attached { id, tier })
        }
        TAG_ERROR => {
            let code = cur.u8()?;
            let message = cur.string()?;
            Ok(Response::Error { code, message })
        }
        _ => Err(ProtocolFault::UnknownTag { tag }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(tier: u8) -> Request {
        Request::CreateInterlock {
            name: "cam0".into(),
            tier,
            owner: None,
            dependencies: vec![],
            watched_name: None,
            watched_word: None,
            interval_ns: None,
            conditions: None,
        }
    }

    #[test]
    fn create_interlock_round_trip() {
        let req = create(0);
        let frame = encode_request(&req).unwrap();
        assert_eq!(decode_request(&frame[4..]).unwrap(), req);
    }

    #[test]
    fn create_wait_counter_round_trip() {
        let req = Request::CreateInterlock {
            name: "frame_done".into(),
            tier: 1,
            owner: None,
            dependencies: vec![],
            watched_name: Some("cam0".into()),
            watched_word: Some(1),
            interval_ns: None,
            conditions: None,
        };
        let frame = encode_request(&req).unwrap();
        assert_eq!(decode_request(&frame[4..]).unwrap(), req);
    }

    #[test]
    fn create_wait_timer_round_trip() {
        let req = create(2);
        let frame = encode_request(&req).unwrap();
        assert_eq!(decode_request(&frame[4..]).unwrap(), req);
    }

    #[test]
    fn create_wait_cron_round_trip() {
        let req = Request::CreateInterlock {
            name: "cron_33ms".into(),
            tier: 3,
            owner: None,
            dependencies: vec![],
            watched_name: None,
            watched_word: None,
            interval_ns: Some(33_000_000),
            conditions: None,
        };
        let frame = encode_request(&req).unwrap();
        assert_eq!(decode_request(&frame[4..]).unwrap(), req);
    }

    #[test]
    fn create_wait_barrier_round_trip() {
        let req = Request::CreateInterlock {
            name: "barrier_all".into(),
            tier: 4,
            owner: None,
            dependencies: vec![],
            watched_name: None,
            watched_word: None,
            interval_ns: None,
            conditions: Some(vec![("cam0".into(), 1, 100), ("cam1".into(), 0, 50)]),
        };
        let frame = encode_request(&req).unwrap();
        assert_eq!(decode_request(&frame[4..]).unwrap(), req);
    }

    #[test]
    fn attach_round_trip() {
        let req = Request::AttachInterlock {
            name: "sensor".into(),
        };
        let frame = encode_request(&req).unwrap();
        assert_eq!(decode_request(&frame[4..]).unwrap(), req);
    }

    #[test]
    fn response_round_trips() {
        for resp in [
            Response::Created { id: 42 },
            Response::Attached { id: 7, tier: 0 },
            Response::Attached { id: 3, tier: 4 },
            Response::from_condition(&Condition::InterlockNotFound {
                name: "missing".into(),
            }),
            Response::invalid_request("bad"),
            Response::allocation_failed("no fds"),
            Response::from_condition(&Condition::InterlockReaped),
        ] {
            let frame = encode_response(&resp).unwrap();
            assert_eq!(decode_response(&frame[4..]).unwrap(), resp);
        }
    }

    #[test]
    fn conditions_map_to_error_codes() {
        let cases = [
            (Condition::InterlockReaped, ERR_INTERLOCK_REAPED),
            (
                Condition::InterlockNotFound { name: "x".into() },
                ERR_INTERLOCK_NOT_FOUND,
            ),
            (
                Condition::InvalidRequest {
                    message: "why".into(),
                },
                ERR_INVALID_REQUEST,
            ),
        ];
        for (cond, code) in cases {
            let resp = Response::from_condition(&cond);
            assert!(matches!(&resp, Response::Error { code: c, .. } if *c == code));
        }
        assert!(matches!(
            Response::from_condition(&Condition::AllocationFailed {
                step: abacus_core::error::AllocationStep::Mmap,
                errno: 12
            }),
            Response::Error {
                code: ERR_ALLOCATION_FAILED,
                ..
            }
        ));
    }

    #[test]
    fn unknown_version_rejected() {
        let data = [99, TAG_CREATE_INTERLOCK, 0];
        assert!(matches!(
            decode_request(&data),
            Err(ProtocolFault::UnsupportedVersion { version: 99 })
        ));
    }

    #[test]
    fn unknown_tag_rejected() {
        assert!(matches!(
            decode_request(&[PROTOCOL_VERSION, 0x7e, 0, 0]),
            Err(ProtocolFault::UnknownTag { tag: 0x7e })
        ));
        assert!(matches!(
            decode_response(&[PROTOCOL_VERSION, 0x7e]),
            Err(ProtocolFault::UnknownTag { tag: 0x7e })
        ));
    }

    #[test]
    fn missing_tier_fields_rejected_on_encode() {
        let cases: [(u8, &str); 3] = [(1, "watched_name"), (3, "interval_ns"), (4, "conditions")];
        for (tier, field) in cases {
            let err = encode_request(&create(tier)).unwrap_err();
            assert_eq!(err, ProtocolFault::MissingField { field }, "tier {tier}");
        }
        let req = Request::CreateInterlock {
            name: "w".into(),
            tier: 1,
            owner: None,
            dependencies: vec![],
            watched_name: Some("src".into()),
            watched_word: None,
            interval_ns: None,
            conditions: None,
        };
        assert_eq!(
            encode_request(&req).unwrap_err(),
            ProtocolFault::MissingField {
                field: "watched_word"
            }
        );
    }

    #[test]
    fn truncation_at_every_offset_reports_exact_needed() {
        let frames = [
            encode_request(&create(0)).unwrap(),
            encode_request(&Request::CreateInterlock {
                name: "w".into(),
                tier: 1,
                owner: None,
                dependencies: vec![],
                watched_name: Some("src".into()),
                watched_word: Some(0),
                interval_ns: None,
                conditions: None,
            })
            .unwrap(),
            encode_request(&Request::CreateInterlock {
                name: "c".into(),
                tier: 3,
                owner: None,
                dependencies: vec![],
                watched_name: None,
                watched_word: None,
                interval_ns: Some(10_000_000),
                conditions: None,
            })
            .unwrap(),
            encode_request(&Request::CreateInterlock {
                name: "b".into(),
                tier: 4,
                owner: None,
                dependencies: vec![],
                watched_name: None,
                watched_word: None,
                interval_ns: None,
                conditions: Some(vec![("a".into(), 0, 1), ("bb".into(), 1, 2)]),
            })
            .unwrap(),
            encode_request(&Request::AttachInterlock { name: "s".into() }).unwrap(),
            encode_request(&Request::CreateInterlock {
                name: "o".into(),
                tier: 0,
                owner: Some(("pc".into(), 42)),
                dependencies: vec![],
                watched_name: None,
                watched_word: None,
                interval_ns: None,
                conditions: None,
            })
            .unwrap(),
            encode_request(&Request::CreateInterlock {
                name: "d".into(),
                tier: 0,
                owner: Some(("pc".into(), 1)),
                dependencies: vec!["a".into(), "bb".into(), "ccc".into()],
                watched_name: None,
                watched_word: None,
                interval_ns: None,
                conditions: None,
            })
            .unwrap(),
            encode_request(&Request::CreateInterlock {
                name: "e".into(),
                tier: 1,
                owner: Some(("pc".into(), 7)),
                dependencies: vec!["x".into()],
                watched_name: Some("tgt".into()),
                watched_word: Some(1),
                interval_ns: None,
                conditions: None,
            })
            .unwrap(),
        ];
        for frame in &frames {
            let payload = &frame[4..];
            for cut in 0..payload.len() {
                match decode_request(&payload[..cut]) {
                    Err(ProtocolFault::Truncated { needed, have }) => {
                        assert_eq!(have, cut);
                        assert!(needed > have, "needed {needed} > have {have}");
                        assert!(
                            needed <= payload.len(),
                            "needed {needed} within {}",
                            payload.len()
                        );
                    }
                    other => panic!("cut {cut}: expected Truncated, got {other:?}"),
                }
            }
        }
        let responses = [
            encode_response(&Response::Created { id: 5 }).unwrap(),
            encode_response(&Response::Attached { id: 9, tier: 3 }).unwrap(),
            encode_response(&Response::invalid_request("m")).unwrap(),
        ];
        for frame in &responses {
            let payload = &frame[4..];
            for cut in 0..payload.len() {
                match decode_response(&payload[..cut]) {
                    Err(ProtocolFault::Truncated { needed, have }) => {
                        assert_eq!(have, cut);
                        assert!(needed > have);
                    }
                    other => panic!("cut {cut}: expected Truncated, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn oversized_string_rejected_on_encode() {
        let req = Request::AttachInterlock {
            name: "x".repeat(u16::MAX as usize + 1),
        };
        assert!(matches!(
            encode_request(&req),
            Err(ProtocolFault::FrameTooLarge { .. })
        ));
        let req = Request::AttachInterlock {
            name: "x".repeat(MAX_MESSAGE_SIZE),
        };
        assert!(matches!(
            encode_request(&req),
            Err(ProtocolFault::FrameTooLarge {
                max: MAX_MESSAGE_SIZE,
                ..
            })
        ));
    }

    #[test]
    fn expected_fd_counts() {
        assert_eq!(expected_fd_count(&Response::Created { id: 1 }), 1);
        assert_eq!(expected_fd_count(&Response::Attached { id: 1, tier: 0 }), 1);
        assert_eq!(expected_fd_count(&Response::allocation_failed("x")), 0);
    }

    #[test]
    fn every_tier_round_trips_with_owner_absent_and_present() {
        for tier in 0..=4u8 {
            for owner in [None, Some(("pc".into(), 99u64))] {
                for deps in [
                    vec![],
                    vec!["x".into()],
                    vec!["a".into(), "b".into(), "c".into()],
                ] {
                    let req = Request::CreateInterlock {
                        name: format!("t{tier}"),
                        tier,
                        owner: owner.clone(),
                        dependencies: deps.clone(),
                        watched_name: if tier == 1 { Some("w".into()) } else { None },
                        watched_word: if tier == 1 { Some(0) } else { None },
                        interval_ns: if tier == 3 { Some(1_000_000) } else { None },
                        conditions: if tier == 4 {
                            Some(vec![("z".into(), 1, 5)])
                        } else {
                            None
                        },
                    };
                    let frame = encode_request(&req).unwrap();
                    assert_eq!(
                        decode_request(&frame[4..]).unwrap(),
                        req,
                        "tier={tier} owner={owner:?} deps={deps:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn attached_round_trip_with_tier() {
        for tier in 0..=4u8 {
            let resp = Response::Attached { id: 100, tier };
            let frame = encode_response(&resp).unwrap();
            assert_eq!(decode_response(&frame[4..]).unwrap(), resp, "tier={tier}");
        }
    }

    #[test]
    fn v1_frame_is_unsupported_version() {
        let mut data = vec![1u8, TAG_CREATE_INTERLOCK];
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(b"ab");
        data.push(0);
        assert!(matches!(
            decode_request(&data),
            Err(ProtocolFault::UnsupportedVersion { version: 1 })
        ));
        let resp_data = [1u8, TAG_ATTACHED, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(
            decode_response(&resp_data),
            Err(ProtocolFault::UnsupportedVersion { version: 1 })
        ));
    }

    #[test]
    fn owner_flag_2_is_invalid_flag() {
        let req = create(0);
        let mut frame = encode_request(&req).unwrap();
        let payload = &mut frame[4..];
        // owner flag sits right after: version(1) + tag(1) + name_len(2) + name(4) + tier(1) = offset 9
        let name_len = u16::from_le_bytes([payload[2], payload[3]]) as usize;
        let owner_flag_offset = 2 + 2 + name_len + 1;
        assert_eq!(payload[owner_flag_offset], 0, "sanity: owner flag is 0");
        payload[owner_flag_offset] = 2;
        assert!(matches!(
            decode_request(payload),
            Err(ProtocolFault::InvalidFlag {
                field: "owner",
                value: 2
            })
        ));
    }
}
