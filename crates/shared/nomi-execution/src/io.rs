use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[cfg(windows)]
use windows_sys::Win32::Globalization::{
    GetACP, IsDBCSLeadByteEx, MB_ERR_INVALID_CHARS, MultiByteToWideChar,
};

use crate::outcome::{
    EncodingMetadata, ExecutionEvent, OutputChunk, OutputCursor, OutputSnapshot, OutputStream,
};

const UTF8_LABEL: &str = "utf-8";
const DECODE_SCRATCH_BYTES: usize = 4 * 1024;
// Retained text may use UTF-8's fixed maximum of four bytes per decoded scalar. The raw
// cap plus bounded decoder carry keeps that representation cap-proportional without loss.

struct StoredChunk {
    seq: u64,
    start: u64,
    stream: OutputStream,
    bytes: Vec<u8>,
}

pub struct OutputBuffer {
    limit: usize,
    inner: Mutex<OutputBufferState>,
    activity: Option<Arc<dyn Fn() + Send + Sync>>,
    changed: tokio::sync::watch::Sender<u64>,
}

struct OutputState {
    next_seq: u64,
    next_offset: u64,
    base_offset: u64,
    retained: usize,
    dropped_bytes: u64,
    chunks: VecDeque<StoredChunk>,
    decoders: StreamDecoders,
    base_decoders: StreamDecoders,
    finalized: bool,
}

enum OutputBufferState {
    Live(Box<OutputState>),
    Frozen(Arc<OutputState>),
    Transition,
}

#[derive(Clone)]
pub(crate) struct FrozenOutput {
    state: Arc<OutputState>,
}

impl OutputBuffer {
    pub fn new(limit_bytes: usize) -> Self {
        let decoders = StreamDecoders::new();
        let base_decoders = decoders.clone();
        let (changed, _receiver) = tokio::sync::watch::channel(0);
        Self {
            limit: limit_bytes,
            inner: Mutex::new(OutputBufferState::Live(Box::new(OutputState {
                next_seq: 0,
                next_offset: 0,
                base_offset: 0,
                retained: 0,
                dropped_bytes: 0,
                chunks: VecDeque::new(),
                decoders,
                base_decoders,
                finalized: false,
            }))),
            activity: None,
            changed,
        }
    }

    pub(crate) fn with_activity(
        limit_bytes: usize,
        activity: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let mut output = Self::new(limit_bytes);
        output.activity = Some(activity);
        output
    }

    pub fn push(&self, stream: OutputStream, bytes: &[u8]) -> Vec<ExecutionEvent> {
        let mut storage = self
            .inner
            .lock()
            .expect("process output state mutex is poisoned");
        let state = match &mut *storage {
            OutputBufferState::Live(state) => state,
            OutputBufferState::Frozen(_) => return Vec::new(),
            OutputBufferState::Transition => {
                unreachable!("output state transition is only visible while its lock is held")
            }
        };
        let start = state.next_offset;
        state.next_offset = state
            .next_offset
            .checked_add(byte_count(bytes.len()))
            .expect("process output offset overflowed u64");
        let retained_start = bytes.len().saturating_sub(self.limit);
        let retained_bytes = &bytes[retained_start..];

        let decoded = {
            let decoder = state.decoders.for_stream_mut(stream);
            decoder.decode_event(bytes, retained_start)
        };
        let output_seq = state.take_seq();
        let mut events = vec![ExecutionEvent::Output {
            seq: output_seq,
            stream,
            bytes: retained_bytes.to_vec(),
            text: decoded.text,
            encoding: decoded.encoding,
        }];

        let dropped = state.retain(self.limit, output_seq, start, stream, bytes);
        if dropped > 0 {
            let dropped_seq = state.take_seq();
            events.push(ExecutionEvent::OutputDropped {
                seq: dropped_seq,
                bytes: dropped,
            });
        }

        drop(storage);
        if !bytes.is_empty()
            && let Some(activity) = &self.activity
        {
            activity();
        }
        if !bytes.is_empty() {
            self.changed.send_modify(|generation| {
                *generation = generation.wrapping_add(1);
            });
        }
        events
    }

    pub(crate) fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn snapshot_from(&self, cursor: OutputCursor) -> OutputSnapshot {
        let storage = self
            .inner
            .lock()
            .expect("process output state mutex is poisoned");
        match &*storage {
            OutputBufferState::Live(state) => snapshot_from_state(state, cursor),
            OutputBufferState::Frozen(state) => snapshot_from_state(state, cursor),
            OutputBufferState::Transition => {
                unreachable!("output state transition is only visible while its lock is held")
            }
        }
    }

    pub(crate) fn freeze(&self) -> FrozenOutput {
        let mut storage = self
            .inner
            .lock()
            .expect("process output state mutex is poisoned");
        if let OutputBufferState::Frozen(state) = &*storage {
            return FrozenOutput {
                state: Arc::clone(state),
            };
        }

        let OutputBufferState::Live(mut state) =
            std::mem::replace(&mut *storage, OutputBufferState::Transition)
        else {
            unreachable!("output state transition is only visible while its lock is held")
        };
        state.finalize();
        let state = Arc::from(state);
        *storage = OutputBufferState::Frozen(Arc::clone(&state));
        drop(storage);
        self.changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
        FrozenOutput { state }
    }
}

impl FrozenOutput {
    pub(crate) fn snapshot_from(&self, cursor: OutputCursor) -> OutputSnapshot {
        snapshot_from_state(&self.state, cursor)
    }
}

fn snapshot_from_state(state: &OutputState, cursor: OutputCursor) -> OutputSnapshot {
    let start = cursor
        .offset()
        .max(state.base_offset)
        .min(state.next_offset);
    let mut decoders = state.base_decoders.clone();
    let mut chunks = Vec::with_capacity(state.chunks.len());

    for stored in &state.chunks {
        let end = stored
            .start
            .checked_add(byte_count(stored.bytes.len()))
            .expect("stored output offset overflowed u64");
        if end <= start {
            decoders
                .for_stream_mut(stored.stream)
                .decode(&stored.bytes);
            continue;
        }

        let slice_start = start.saturating_sub(stored.start) as usize;
        let decoder = decoders.for_stream_mut(stored.stream);
        if slice_start > 0 {
            decoder.decode(&stored.bytes[..slice_start]);
        }
        let bytes = stored.bytes[slice_start..].to_vec();
        let chunk_start = stored
            .start
            .checked_add(byte_count(slice_start))
            .expect("snapshot output offset overflowed u64");
        let text = decoder.decode(&bytes).text;
        chunks.push(OutputChunk {
            seq: stored.seq,
            start: chunk_start,
            stream: stored.stream,
            bytes,
            text,
        });
    }

    if state.finalized {
        for (ordinal, (stream, text)) in decoders.finish().into_iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            if let Some(chunk) = chunks.iter_mut().rev().find(|chunk| chunk.stream == stream) {
                chunk.text.push_str(&text);
            } else if state
                .chunks
                .iter()
                .any(|chunk| chunk.stream == stream)
            {
                let seq = state
                    .next_seq
                    .checked_add(byte_count(ordinal))
                    .expect("terminal output sequence overflowed u64");
                chunks.push(OutputChunk {
                    seq,
                    start: state.next_offset,
                    stream,
                    bytes: Vec::new(),
                    text,
                });
            }
        }
    }

    OutputSnapshot {
        chunks,
        next_cursor: OutputCursor::new(state.next_offset),
        retained_bytes: state.retained,
        dropped_bytes: state.dropped_bytes,
        encoding: state.decoders.metadata(),
    }
}

impl OutputState {
    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        let _ = self.decoders.finish();
        self.finalized = true;
    }

    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self
            .next_seq
            .checked_add(1)
            .expect("process output event sequence overflowed u64");
        seq
    }

    fn retain(
        &mut self,
        limit: usize,
        seq: u64,
        start: u64,
        stream: OutputStream,
        bytes: &[u8],
    ) -> u64 {
        if bytes.is_empty() {
            return 0;
        }

        let keep = bytes.len().min(limit);
        let dropped_from_new = bytes.len() - keep;
        let dropped_from_old = self
            .retained
            .checked_add(keep)
            .expect("retained process output size overflowed usize")
            .saturating_sub(limit);
        self.drop_oldest(dropped_from_old);
        if dropped_from_new > 0 {
            self.base_decoders
                .for_stream_mut(stream)
                .discard_bounded(&bytes[..dropped_from_new]);
        }

        if keep > 0 {
            let kept_start = start
                .checked_add(byte_count(dropped_from_new))
                .expect("retained output offset overflowed u64");
            self.chunks.push_back(StoredChunk {
                seq,
                start: kept_start,
                stream,
                bytes: bytes[dropped_from_new..].to_vec(),
            });
            self.retained = self
                .retained
                .checked_add(keep)
                .expect("retained process output size overflowed usize");
        }

        let dropped = dropped_from_old
            .checked_add(dropped_from_new)
            .expect("dropped process output size overflowed usize");
        let dropped = byte_count(dropped);
        self.base_offset = self
            .base_offset
            .checked_add(dropped)
            .expect("process output base offset overflowed u64");
        self.dropped_bytes = self
            .dropped_bytes
            .checked_add(dropped)
            .expect("dropped process output count overflowed u64");

        debug_assert!(self.retained <= limit);
        debug_assert_eq!(
            self.retained,
            self.chunks
                .iter()
                .map(|chunk| chunk.bytes.len())
                .sum::<usize>()
        );
        debug_assert_eq!(self.base_offset, self.dropped_bytes);
        dropped
    }

    fn drop_oldest(&mut self, mut count: usize) {
        while count > 0 {
            let oldest_len = self
                .chunks
                .front()
                .expect("retained output bytes must have a chunk")
                .bytes
                .len();
            let drop_now = count.min(oldest_len);
            let (stream, dropped) = {
                let oldest = self
                    .chunks
                    .front()
                    .expect("retained output bytes must have a chunk");
                (oldest.stream, oldest.bytes[..drop_now].to_vec())
            };
            self.base_decoders
                .for_stream_mut(stream)
                .decode(&dropped);

            if drop_now == oldest_len {
                self.chunks.pop_front();
            } else {
                let oldest = self
                    .chunks
                    .front_mut()
                    .expect("oldest output chunk disappeared while trimming");
                oldest.start = oldest
                    .start
                    .checked_add(byte_count(drop_now))
                    .expect("trimmed output offset overflowed u64");
                oldest.bytes = oldest.bytes[drop_now..].to_vec();
            }

            self.retained -= drop_now;
            count -= drop_now;
        }
    }
}

fn byte_count(count: usize) -> u64 {
    u64::try_from(count).expect("process output byte count does not fit in u64")
}

#[derive(Clone)]
struct StreamDecoders {
    stdout: IncrementalDecoder,
    stderr: IncrementalDecoder,
    pty: IncrementalDecoder,
}

impl StreamDecoders {
    fn new() -> Self {
        Self {
            stdout: IncrementalDecoder::new(),
            stderr: IncrementalDecoder::new(),
            pty: IncrementalDecoder::new(),
        }
    }

    fn for_stream_mut(&mut self, stream: OutputStream) -> &mut IncrementalDecoder {
        match stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
            OutputStream::Pty => &mut self.pty,
        }
    }

    fn metadata(&self) -> EncodingMetadata {
        let decoders = [&self.stdout, &self.stderr, &self.pty];
        let decode_errors = decoders.iter().fold(0_u64, |total, decoder| {
            total.saturating_add(decoder.decode_errors)
        });
        let mut sources = decoders
            .iter()
            .filter(|decoder| decoder.observed_bytes)
            .map(|decoder| decoder.source_encoding.as_str())
            .collect::<Vec<_>>();
        let source_encoding = match sources.pop() {
            None => UTF8_LABEL.to_owned(),
            Some(source) if sources.iter().all(|other| *other == source) => source.to_owned(),
            Some(_) => "mixed".to_owned(),
        };
        EncodingMetadata {
            source_encoding,
            decode_errors,
        }
    }

    fn finish(&mut self) -> [(OutputStream, String); 3] {
        [
            (OutputStream::Stdout, self.stdout.finish().text),
            (OutputStream::Stderr, self.stderr.finish().text),
            (OutputStream::Pty, self.pty.finish().text),
        ]
    }
}

struct DecodedDelta {
    text: String,
    encoding: EncodingMetadata,
    sources: DeltaSources,
}

#[derive(Default)]
struct DeltaSources {
    utf8_non_ascii: bool,
    platform_encoding: Option<String>,
}

impl DeltaSources {
    fn merge(&mut self, other: Self) {
        self.utf8_non_ascii |= other.utf8_non_ascii;
        if let Some(other_encoding) = other.platform_encoding {
            match &self.platform_encoding {
                None => self.platform_encoding = Some(other_encoding),
                Some(encoding) if *encoding == other_encoding => {}
                Some(_) => self.platform_encoding = Some("mixed".to_owned()),
            }
        }
    }

    fn label(&self) -> String {
        match &self.platform_encoding {
            Some(_) if self.utf8_non_ascii => "mixed".to_owned(),
            Some(encoding) => encoding.clone(),
            None => UTF8_LABEL.to_owned(),
        }
    }
}

#[derive(Clone)]
struct IncrementalDecoder {
    utf8_pending: Vec<u8>,
    decode_errors: u64,
    source_encoding: String,
    observed_bytes: bool,
    saw_non_ascii_utf8: bool,
    #[cfg(windows)]
    active_code_page: u32,
    #[cfg(windows)]
    windows: Option<WindowsDecoder>,
}

impl IncrementalDecoder {
    fn new() -> Self {
        Self {
            utf8_pending: Vec::with_capacity(3),
            decode_errors: 0,
            source_encoding: UTF8_LABEL.to_owned(),
            observed_bytes: false,
            saw_non_ascii_utf8: false,
            #[cfg(windows)]
            active_code_page: active_code_page(),
            #[cfg(windows)]
            windows: None,
        }
    }

    fn decode(&mut self, bytes: &[u8]) -> DecodedDelta {
        let errors_before = self.decode_errors;
        self.observed_bytes |= !bytes.is_empty();
        let mut sources = DeltaSources::default();
        let text = self.decode_text(bytes, &mut sources);
        DecodedDelta {
            text,
            encoding: EncodingMetadata {
                source_encoding: sources.label(),
                decode_errors: self.decode_errors.saturating_sub(errors_before),
            },
            sources,
        }
    }

    fn decode_event(&mut self, bytes: &[u8], retained_start: usize) -> DecodedDelta {
        let errors_before = self.decode_errors;
        let mut sources = DeltaSources::default();
        for chunk in bytes[..retained_start].chunks(DECODE_SCRATCH_BYTES) {
            sources.merge(self.decode(chunk).sources);
        }
        let mut decoded = self.decode(&bytes[retained_start..]);
        sources.merge(decoded.sources);
        decoded.encoding.decode_errors = self.decode_errors.saturating_sub(errors_before);
        decoded.encoding.source_encoding = sources.label();
        decoded.sources = sources;
        decoded
    }

    fn discard_bounded(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(DECODE_SCRATCH_BYTES) {
            self.decode(chunk);
        }
    }

    fn finish(&mut self) -> DecodedDelta {
        let errors_before = self.decode_errors;
        #[cfg(windows)]
        let mut sources = DeltaSources::default();
        #[cfg(not(windows))]
        let sources = DeltaSources::default();

        #[cfg(windows)]
        let text = if let Some(decoder) = &mut self.windows {
            let platform_encoding = decoder.label();
            sources.platform_encoding = Some(platform_encoding.clone());
            self.source_encoding = if self.saw_non_ascii_utf8 {
                "mixed".to_owned()
            } else {
                platform_encoding
            };
            let (text, errors) = decoder.finish();
            self.decode_errors = self.decode_errors.saturating_add(errors);
            text
        } else if self.utf8_pending.is_empty() {
            String::new()
        } else {
            let pending = std::mem::take(&mut self.utf8_pending);
            self.decode_errors = self.decode_errors.saturating_add(1);
            if self.active_code_page == 65001 {
                "\u{fffd}".to_owned()
            } else {
                let mut decoder = WindowsDecoder::new(self.active_code_page);
                let platform_encoding = decoder.label();
                sources.platform_encoding = Some(platform_encoding.clone());
                self.source_encoding = if self.saw_non_ascii_utf8 {
                    "mixed".to_owned()
                } else {
                    platform_encoding
                };
                let (mut text, decode_errors) = decoder.decode(&pending);
                let (suffix, finish_errors) = decoder.finish();
                text.push_str(&suffix);
                self.decode_errors = self
                    .decode_errors
                    .saturating_add(decode_errors)
                    .saturating_add(finish_errors);
                self.windows = Some(decoder);
                text
            }
        };

        #[cfg(not(windows))]
        let text = if self.utf8_pending.is_empty() {
            String::new()
        } else {
            self.utf8_pending.clear();
            self.decode_errors = self.decode_errors.saturating_add(1);
            "\u{fffd}".to_owned()
        };

        DecodedDelta {
            text,
            encoding: EncodingMetadata {
                source_encoding: sources.label(),
                decode_errors: self.decode_errors.saturating_sub(errors_before),
            },
            sources,
        }
    }

    fn decode_text(&mut self, bytes: &[u8], sources: &mut DeltaSources) -> String {
        #[cfg(windows)]
        if let Some(decoder) = &mut self.windows {
            sources.platform_encoding = Some(decoder.label());
            let (text, errors) = decoder.decode(bytes);
            self.decode_errors = self.decode_errors.saturating_add(errors);
            return text;
        }

        let mut input = std::mem::take(&mut self.utf8_pending);
        input.extend_from_slice(bytes);
        let mut text = String::new();
        let mut index = 0;

        while index < input.len() {
            match std::str::from_utf8(&input[index..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    sources.utf8_non_ascii |= !valid.is_ascii();
                    self.saw_non_ascii_utf8 |= !valid.is_ascii();
                    break;
                }
                Err(error) => {
                    let valid_end = index + error.valid_up_to();
                    let valid = std::str::from_utf8(&input[index..valid_end])
                        .expect("Utf8Error valid prefix must be UTF-8");
                    text.push_str(valid);
                    sources.utf8_non_ascii |= !valid.is_ascii();
                    self.saw_non_ascii_utf8 |= !valid.is_ascii();
                    index = valid_end;

                    let Some(error_len) = error.error_len() else {
                        self.utf8_pending.extend_from_slice(&input[index..]);
                        break;
                    };
                    self.decode_errors = self.decode_errors.saturating_add(1);

                    #[cfg(windows)]
                    {
                        let code_page = self.active_code_page;
                        if code_page != 65001 {
                            let mut decoder = WindowsDecoder::new(code_page);
                            let platform_encoding = decoder.label();
                            sources.platform_encoding = Some(platform_encoding.clone());
                            self.source_encoding = if self.saw_non_ascii_utf8 {
                                "mixed".to_owned()
                            } else {
                                platform_encoding
                            };
                            let (decoded, errors) = decoder.decode(&input[index..]);
                            self.decode_errors = self.decode_errors.saturating_add(errors);
                            self.windows = Some(decoder);
                            text.push_str(&decoded);
                            break;
                        }
                    }

                    text.push('\u{fffd}');
                    index += error_len;
                }
            }
        }

        text
    }
}

#[cfg(windows)]
#[derive(Clone)]
struct WindowsDecoder {
    code_page: u32,
    pending: Vec<u8>,
}

#[cfg(windows)]
impl WindowsDecoder {
    fn new(code_page: u32) -> Self {
        Self {
            code_page,
            pending: Vec::with_capacity(1),
        }
    }

    fn label(&self) -> String {
        format!("windows-{}", self.code_page)
    }

    fn decode(&mut self, bytes: &[u8]) -> (String, u64) {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        if let Some(index) = trailing_dbcs_lead(self.code_page, &input) {
            self.pending.extend_from_slice(&input[index..]);
            input.truncate(index);
        }
        decode_windows_input(self.code_page, &input)
    }

    fn finish(&mut self) -> (String, u64) {
        if self.pending.is_empty() {
            return (String::new(), 0);
        }
        self.pending.clear();
        ("\u{fffd}".to_owned(), 1)
    }
}

#[cfg(windows)]
fn active_code_page() -> u32 {
    // SAFETY: GetACP has no parameters and only reads the process-wide Windows setting.
    unsafe { GetACP() }
}

#[cfg(windows)]
fn trailing_dbcs_lead(code_page: u32, input: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index < input.len() {
        // SAFETY: IsDBCSLeadByteEx accepts every u8 value for a valid code-page identifier.
        let is_lead = unsafe { IsDBCSLeadByteEx(code_page, input[index]) != 0 };
        if is_lead {
            if index + 1 == input.len() {
                return Some(index);
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    None
}

#[cfg(windows)]
fn decode_windows_input(code_page: u32, input: &[u8]) -> (String, u64) {
    if input.is_empty() {
        return (String::new(), 0);
    }

    let strict = multi_byte_to_string(code_page, MB_ERR_INVALID_CHARS, input);
    let errors = u64::from(strict.is_none());
    match strict.or_else(|| multi_byte_to_string(code_page, 0, input)) {
        Some(text) => (text, errors),
        None => ("\u{fffd}".to_owned(), errors.saturating_add(1)),
    }
}

#[cfg(windows)]
fn multi_byte_to_string(code_page: u32, flags: u32, input: &[u8]) -> Option<String> {
    let input_len = i32::try_from(input.len()).ok()?;
    // SAFETY: input points to input_len readable bytes; a null output pointer requests the size.
    let required = unsafe {
        MultiByteToWideChar(
            code_page,
            flags,
            input.as_ptr(),
            input_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if required == 0 {
        return None;
    }

    let mut wide = vec![0_u16; required as usize];
    // SAFETY: wide has capacity for required UTF-16 code units, as returned by the sizing call.
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            flags,
            input.as_ptr(),
            input_len,
            wide.as_mut_ptr(),
            required,
        )
    };
    if written == 0 {
        return None;
    }
    wide.truncate(written as usize);
    String::from_utf16(&wide).ok()
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::Arc,
        thread,
    };

    use super::{FrozenOutput, OutputBuffer, OutputBufferState};
    use crate::{ExecutionEvent, OutputCursor, OutputStream};

    const MAX_DECODED_TEXT_BYTES_PER_SOURCE_BYTE: usize = 4;

    fn retained_allocation(output: &OutputBuffer) -> Option<(*const u8, usize)> {
        let storage = output.inner.lock().expect("fresh mutex must lock");
        let state = match &*storage {
            OutputBufferState::Live(state) => state.as_ref(),
            OutputBufferState::Frozen(state) => state.as_ref(),
            OutputBufferState::Transition => panic!("freeze cannot expose its transition state"),
        };
        state
            .chunks
            .front()
            .map(|chunk| (chunk.bytes.as_ptr(), chunk.bytes.len()))
    }

    fn frozen_allocation(output: &FrozenOutput) -> Option<(*const u8, usize)> {
        output
            .state
            .chunks
            .front()
            .map(|chunk| (chunk.bytes.as_ptr(), chunk.bytes.len()))
    }

    #[cfg(windows)]
    fn set_code_page(output: &OutputBuffer, stream: OutputStream, code_page: u32) {
        let mut storage = output.inner.lock().expect("fresh mutex must lock");
        let OutputBufferState::Live(state) = &mut *storage else {
            panic!("code page can only be set before output is frozen");
        };
        state.decoders.for_stream_mut(stream).active_code_page = code_page;
        state
            .base_decoders
            .for_stream_mut(stream)
            .active_code_page = code_page;
    }

    #[test]
    fn retained_replacement_expansion_is_complete_and_matches_the_snapshot() {
        let output = OutputBuffer::new(1);
        #[cfg(windows)]
        set_code_page(&output, OutputStream::Stdout, 65001);

        let events = output.push(OutputStream::Stdout, &[0xff]);
        let ExecutionEvent::Output { bytes, text, .. } = &events[0] else {
            panic!("first push event was not output");
        };
        let snapshot = output.snapshot_from(OutputCursor::START);

        assert_eq!(bytes, &[0xff]);
        assert_eq!(text, "\u{fffd}");
        assert_eq!(snapshot.text(), "\u{fffd}");
        assert_eq!(snapshot.dropped_bytes, 0);
        assert!(text.len() > bytes.len());
        assert!(
            text.len()
                <= bytes
                    .len()
                    .saturating_mul(MAX_DECODED_TEXT_BYTES_PER_SOURCE_BYTE)
        );
    }

    #[test]
    fn freeze_migrates_retained_output_once_and_rejects_late_pushes() {
        let output = OutputBuffer::new(8);
        output.push(OutputStream::Stdout, b"12345678");
        let allocation_before =
            retained_allocation(&output).expect("live output must retain its chunk");

        let frozen = output.freeze();
        let frozen_clone = frozen.clone();
        let frozen_again = output.freeze();
        assert_eq!(
            frozen_allocation(&frozen),
            Some(allocation_before),
            "freeze must migrate the retained allocation instead of copying it"
        );
        assert_eq!(
            frozen_allocation(&frozen_clone),
            Some(allocation_before),
            "cloning terminal output must be shallow"
        );
        assert_eq!(
            frozen_allocation(&frozen_again),
            Some(allocation_before),
            "repeated freeze must reuse the same terminal allocation"
        );
        assert_eq!(
            retained_allocation(&output),
            Some(allocation_before),
            "live and terminal views must share the frozen allocation"
        );

        let terminal_before = frozen.snapshot_from(OutputCursor::START);
        let events = output.push(OutputStream::Stdout, b"late");
        let terminal_after = frozen.snapshot_from(OutputCursor::START);
        let live_after = output.snapshot_from(OutputCursor::START);
        assert!(events.is_empty(), "push after freeze must be rejected");
        assert_eq!(terminal_after, terminal_before);
        assert_eq!(live_after, terminal_before);
    }

    #[test]
    fn freeze_eof_finalizes_pending_utf8_for_all_streams_once() {
        let output = OutputBuffer::new(3);
        #[cfg(windows)]
        for stream in [
            OutputStream::Stdout,
            OutputStream::Stderr,
            OutputStream::Pty,
        ] {
            set_code_page(&output, stream, 65001);
        }

        for stream in [
            OutputStream::Stdout,
            OutputStream::Stderr,
            OutputStream::Pty,
        ] {
            output.push(stream, &[0xe4]);
        }
        let live = output.snapshot_from(OutputCursor::START);
        assert_eq!(live.text(), "");
        assert_eq!(live.encoding.decode_errors, 0);

        let frozen = output.freeze();
        let terminal = frozen.snapshot_from(OutputCursor::START);
        let terminal_again = output.freeze().snapshot_from(OutputCursor::START);
        assert_eq!(terminal.text(), "\u{fffd}\u{fffd}\u{fffd}");
        assert_eq!(terminal.encoding.decode_errors, 3);
        assert_eq!(terminal_again, terminal, "EOF finalization must be idempotent");
        assert_eq!(output.snapshot_from(OutputCursor::START), terminal);
    }

    #[test]
    fn terminal_eof_text_survives_a_live_cursor_past_that_stream() {
        let output = OutputBuffer::new(2);
        #[cfg(windows)]
        set_code_page(&output, OutputStream::Stdout, 65001);

        output.push(OutputStream::Stdout, &[0xe4]);
        let live = output.snapshot_from(OutputCursor::START);
        assert_eq!(live.raw_bytes(), [0xe4]);
        assert_eq!(live.text(), "");
        let live_cursor = live.next_cursor;

        output.push(OutputStream::Stderr, b"x");
        let frozen = output.freeze();
        let terminal = frozen.snapshot_from(live_cursor);
        let terminal_again = frozen.snapshot_from(live_cursor);

        assert_eq!(terminal, terminal_again);
        assert_eq!(terminal.raw_bytes(), b"x");
        assert_eq!(terminal.text(), "x\u{fffd}");
        assert_eq!(terminal.next_cursor, OutputCursor::new(2));
        assert_eq!(terminal.retained_bytes, 2);
        assert_eq!(terminal.dropped_bytes, 0);
        assert_eq!(terminal.chunks.len(), 2);
        assert_eq!(terminal.chunks[0].stream, OutputStream::Stderr);
        assert_eq!(terminal.chunks[0].seq, 1);
        assert_eq!(terminal.chunks[0].start, 1);
        assert_eq!(terminal.chunks[0].bytes, b"x");
        assert_eq!(terminal.chunks[1].stream, OutputStream::Stdout);
        assert_eq!(terminal.chunks[1].seq, 2);
        assert_eq!(terminal.chunks[1].start, 2);
        assert!(terminal.chunks[1].bytes.is_empty());
        assert_eq!(terminal.chunks[1].text, "\u{fffd}");
    }

    #[test]
    fn terminal_eof_does_not_reveal_an_incomplete_tail_dropped_by_zero_cap() {
        let output = OutputBuffer::new(0);
        #[cfg(windows)]
        set_code_page(&output, OutputStream::Stdout, 65001);

        output.push(OutputStream::Stdout, &[0xe4]);
        let frozen = output.freeze();
        let terminal = frozen.snapshot_from(OutputCursor::START);
        let terminal_again = frozen.snapshot_from(OutputCursor::START);

        assert_eq!(terminal, terminal_again);
        assert!(terminal.chunks.is_empty());
        assert!(terminal.raw_bytes().is_empty());
        assert_eq!(terminal.text(), "");
        assert_eq!(terminal.next_cursor, OutputCursor::new(1));
        assert_eq!(terminal.retained_bytes, 0);
        assert_eq!(terminal.dropped_bytes, 1);
        assert_eq!(terminal.encoding.decode_errors, 1);
    }

    #[test]
    fn terminal_eof_does_not_reveal_a_tail_evicted_by_another_stream() {
        let output = OutputBuffer::new(1);
        #[cfg(windows)]
        set_code_page(&output, OutputStream::Stdout, 65001);

        output.push(OutputStream::Stdout, &[0xe4]);
        output.push(OutputStream::Stderr, b"x");
        let frozen = output.freeze();
        let terminal = frozen.snapshot_from(OutputCursor::START);
        let terminal_again = frozen.snapshot_from(OutputCursor::START);

        assert_eq!(terminal, terminal_again);
        assert_eq!(terminal.chunks.len(), 1);
        assert_eq!(terminal.chunks[0].stream, OutputStream::Stderr);
        assert_eq!(terminal.chunks[0].seq, 1);
        assert_eq!(terminal.chunks[0].start, 1);
        assert_eq!(terminal.chunks[0].bytes, b"x");
        assert_eq!(terminal.chunks[0].text, "x");
        assert_eq!(terminal.raw_bytes(), b"x");
        assert_eq!(terminal.text(), "x");
        assert_eq!(terminal.next_cursor, OutputCursor::new(2));
        assert_eq!(terminal.retained_bytes, 1);
        assert_eq!(terminal.dropped_bytes, 1);
        assert_eq!(terminal.encoding.decode_errors, 1);
    }

    #[cfg(windows)]
    #[test]
    fn freeze_eof_falls_back_to_the_active_code_page_for_utf8_carry() {
        let output = OutputBuffer::new(1);
        set_code_page(&output, OutputStream::Stdout, 1252);
        output.push(OutputStream::Stdout, &[0xe4]);
        let live = output.snapshot_from(OutputCursor::START);
        assert_eq!(live.text(), "");
        assert_eq!(live.encoding.source_encoding, "utf-8");
        assert_eq!(live.encoding.decode_errors, 0);

        let terminal = output.freeze().snapshot_from(OutputCursor::START);
        assert_eq!(terminal.chunks[0].bytes, [0xe4]);
        assert_eq!(terminal.text(), "ä");
        assert_eq!(terminal.encoding.source_encoding, "windows-1252");
        assert_eq!(terminal.encoding.decode_errors, 1);
    }

    #[cfg(windows)]
    #[test]
    fn freeze_eof_finalizes_pending_windows_dbcs_lead_byte() {
        let output = OutputBuffer::new(1);
        set_code_page(&output, OutputStream::Stdout, 936);
        output.push(OutputStream::Stdout, &[0x81]);
        let live = output.snapshot_from(OutputCursor::START);
        assert_eq!(live.text(), "");

        let terminal = output.freeze().snapshot_from(OutputCursor::START);
        assert_eq!(terminal.chunks[0].bytes, [0x81]);
        assert_eq!(terminal.text(), "\u{fffd}");
        assert!(terminal.encoding.decode_errors > live.encoding.decode_errors);
    }

    #[test]
    fn poisoned_output_state_fails_closed() {
        let output = Arc::new(OutputBuffer::new(8));
        let poisoner = Arc::clone(&output);
        let result = thread::spawn(move || {
            let _guard = poisoner.inner.lock().expect("fresh mutex must lock");
            panic!("poison output state");
        })
        .join();
        assert!(result.is_err());

        let push = catch_unwind(AssertUnwindSafe(|| {
            output.push(OutputStream::Stdout, b"must not recover");
        }));
        assert!(push.is_err());
    }
}
