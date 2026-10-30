use alloc::vec::Vec;

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::message::Frame;

pub const MAX_FRAME_BYTES: usize = 1 << 20;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame exceeds {MAX_FRAME_BYTES} bytes without a newline")]
    TooLarge,
    #[error("malformed frame: {0}")]
    Malformed(serde_json::Error),
}

/// Appends `frame` as one JSON line to `out`.
pub fn encode_frame<Body: Serialize>(
    frame: &Frame<Body>,
    out: &mut Vec<u8>,
) -> Result<(), FrameError> {
    out.extend(serde_json::to_vec(frame).map_err(FrameError::Malformed)?);
    out.push(b'\n');
    Ok(())
}

/// Sans-IO line framer: feed it bytes as they arrive, pull complete frames out.
#[derive(Debug, Default)]
pub struct LineDecoder {
    buffer: Vec<u8>,
}

impl LineDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete frame, `None` when more bytes are needed.
    pub fn next_frame<Body: DeserializeOwned>(
        &mut self,
    ) -> Option<Result<Frame<Body>, FrameError>> {
        let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') else {
            return (self.buffer.len() > MAX_FRAME_BYTES).then(|| Err(FrameError::TooLarge));
        };
        let line: Vec<u8> = self.buffer.drain(..=end).collect();
        let body = line.get(..end).unwrap_or(&[]);
        Some(serde_json::from_slice(body).map_err(FrameError::Malformed))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::string::ToString;

    use super::*;
    use crate::message::{ErrorKind, Request, Response};

    fn encoded(frame: &Frame<Request>) -> Vec<u8> {
        let mut out = Vec::new();
        encode_frame(frame, &mut out).expect("encode");
        out
    }

    #[test]
    fn a_frame_split_across_pushes_decodes_once_complete() {
        let frame = Frame {
            id: 7,
            body: Request::Search {
                query: "walk".to_string(),
                path: "/r".to_string(),
                limit: 5,
            },
        };
        let bytes = encoded(&frame);
        let (head, tail) = bytes.split_at(bytes.len() / 2);
        let mut decoder = LineDecoder::new();

        decoder.push(head);
        let early = decoder.next_frame::<Request>().is_none();
        decoder.push(tail);
        let parsed = decoder
            .next_frame::<Request>()
            .expect("complete")
            .expect("valid");

        assert!(early);
        assert_eq!(parsed, frame);
    }

    #[test]
    fn two_frames_in_one_push_come_out_in_order() {
        let mut bytes = encoded(&Frame {
            id: 1,
            body: Request::Status,
        });
        bytes.extend(encoded(&Frame {
            id: 2,
            body: Request::Stop,
        }));
        let mut decoder = LineDecoder::new();

        decoder.push(&bytes);

        let ids: Vec<u64> = core::iter::from_fn(|| decoder.next_frame::<Request>())
            .map(|frame| frame.expect("valid").id)
            .collect();
        assert_eq!(ids, [1, 2]);
    }

    #[test]
    fn garbage_line_is_a_malformed_error_and_the_stream_continues() {
        let mut decoder = LineDecoder::new();
        decoder.push(b"not json\n");
        decoder.push(&encoded(&Frame {
            id: 3,
            body: Request::WatchList,
        }));

        let first = decoder.next_frame::<Request>().expect("line");
        let second = decoder
            .next_frame::<Request>()
            .expect("line")
            .expect("valid");

        assert!(matches!(first, Err(FrameError::Malformed(_))));
        assert_eq!(second.id, 3);
    }

    #[test]
    fn an_unterminated_oversized_buffer_is_rejected() {
        let mut decoder = LineDecoder::new();
        decoder.push(&alloc::vec![b'x'; MAX_FRAME_BYTES + 1]);

        assert!(matches!(
            decoder.next_frame::<Request>(),
            Some(Err(FrameError::TooLarge))
        ));
    }

    #[test]
    fn error_response_round_trips_with_its_kind() {
        let frame = Frame {
            id: 9,
            body: Response::Error {
                error: ErrorKind::Busy,
                message: "queue full".to_string(),
            },
        };
        let mut out = Vec::new();
        encode_frame(&frame, &mut out).expect("encode");
        let mut decoder = LineDecoder::new();
        decoder.push(&out);

        let parsed = decoder
            .next_frame::<Response>()
            .expect("complete")
            .expect("valid");

        assert_eq!(parsed, frame);
    }
}
