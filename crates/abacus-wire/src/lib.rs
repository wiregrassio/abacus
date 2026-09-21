//! Abacus RTS wire protocol v1, shared by the daemon and the client SDK: the request and
//! response codec, length-prefixed framing, and SCM_RIGHTS fd passing over a Unix stream.
//!
//! Neither side depends on the other for wire types; both depend on this crate.

#![warn(missing_docs)]

pub mod codec;
pub mod fdpass;
pub mod framing;

pub use codec::{
    decode_request, decode_response, encode_request, encode_response, expected_fd_count, Request,
    Response, ERR_ALLOCATION_FAILED, ERR_INTERLOCK_NOT_FOUND, ERR_INTERLOCK_REAPED,
    ERR_INVALID_REQUEST, LENGTH_PREFIX_BYTES, MAX_FRAME_BYTES, MAX_MESSAGE_SIZE, PROTOCOL_VERSION,
};
pub use fdpass::{
    cmsg_space_for_fds, recv_prefix_with_fds, send_frame_with_fds, MAX_FDS_PER_MESSAGE,
};
pub use framing::{read_exact, write_all, FrameReader, ReadOutcome};
