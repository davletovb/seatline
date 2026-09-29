#![no_main]

use std::collections::VecDeque;

use libfuzzer_sys::fuzz_target;
use seatline_core::stream::{LineSplitter, StreamError};

const MAX_LINE_BYTES: usize = 4096;
const SCHEDULE_BYTES: usize = 8;

fn drain(queue: &mut VecDeque<String>, lines: &mut Vec<String>) {
    for line in queue.drain(..) {
        assert!(line.len() <= MAX_LINE_BYTES);
        assert!(!line.contains('\n'));
        lines.push(line);
    }
}

fn split(payload: &[u8], sizes: &[u8]) -> (Vec<String>, Result<(), StreamError>) {
    let mut splitter = LineSplitter::new(MAX_LINE_BYTES);
    let mut ready = VecDeque::new();
    let mut lines = Vec::new();
    let mut offset = 0;
    let mut chunk = 0;
    while offset < payload.len() {
        let size = if sizes.is_empty() {
            payload.len()
        } else {
            usize::from(sizes[chunk % sizes.len()]) + 1
        };
        let end = offset.saturating_add(size).min(payload.len());
        let result = splitter.push(&payload[offset..end], &mut ready);
        drain(&mut ready, &mut lines);
        if let Err(error) = result {
            return (lines, Err(error));
        }
        offset = end;
        chunk += 1;
    }
    match splitter.finish() {
        Ok(Some(line)) => ready.push_back(line),
        Ok(None) => {}
        Err(error) => return (lines, Err(error)),
    }
    drain(&mut ready, &mut lines);
    (lines, Ok(()))
}

fuzz_target!(|data: &[u8]| {
    let control = data.len().min(SCHEDULE_BYTES);
    let (sizes, payload) = data.split_at(control);
    let chunked = split(payload, sizes);
    let one_shot = split(payload, &[]);
    assert_eq!(chunked, one_shot);

    if let (lines, Ok(())) = chunked {
        let mut expected = Vec::new();
        for part in payload.split_inclusive(|byte| *byte == b'\n') {
            let line = if let Some(without_lf) = part.strip_suffix(b"\n") {
                without_lf.strip_suffix(b"\r").unwrap_or(without_lf)
            } else {
                part
            };
            if !part.is_empty() {
                expected.push(String::from_utf8(line.to_vec()).unwrap());
            }
        }
        assert_eq!(lines, expected);
    }
});
