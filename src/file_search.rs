use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufRead, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{Decoder, DecoderResult, Encoding, GB18030, GBK, UTF_16BE, UTF_16LE};
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Result;

const ENCODING_SAMPLE_BYTES: usize = 64 * 1024;
const TRANSCODE_INPUT_BYTES: usize = 8 * 1024;
const TRANSCODE_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug)]
enum SearchEncoding {
    Utf8 {
        bom_len: usize,
    },
    EncodingRs {
        encoding: &'static Encoding,
        bom_len: usize,
    },
    Utf32 {
        little_endian: bool,
        bom_len: usize,
    },
}

impl SearchEncoding {
    fn is_utf8(self) -> bool {
        matches!(self, Self::Utf8 { .. })
    }
}

struct EncodingProbe {
    encoding: SearchEncoding,
    sample: Vec<u8>,
}

struct StrictDecodeReader<R> {
    inner: R,
    decoder: Decoder,
    input: [u8; TRANSCODE_INPUT_BYTES],
    input_start: usize,
    input_end: usize,
    output: [u8; TRANSCODE_OUTPUT_BYTES],
    output_start: usize,
    output_end: usize,
    eof: bool,
    finished: bool,
}

impl<R: Read> StrictDecodeReader<R> {
    fn new(inner: R, encoding: &'static Encoding) -> Self {
        Self {
            inner,
            decoder: encoding.new_decoder_without_bom_handling(),
            input: [0; TRANSCODE_INPUT_BYTES],
            input_start: 0,
            input_end: 0,
            output: [0; TRANSCODE_OUTPUT_BYTES],
            output_start: 0,
            output_end: 0,
            eof: false,
            finished: false,
        }
    }

    fn fill_output(&mut self) -> io::Result<()> {
        self.output_start = 0;
        self.output_end = 0;
        while !self.finished && self.output_end == 0 {
            if self.input_start == self.input_end && !self.eof {
                self.input_start = 0;
                self.input_end = self.inner.read(&mut self.input)?;
                self.eof = self.input_end == 0;
            }
            let (result, read, written) = self.decoder.decode_to_utf8_without_replacement(
                &self.input[self.input_start..self.input_end],
                &mut self.output,
                self.eof,
            );
            self.input_start += read;
            self.output_end = written;
            match result {
                DecoderResult::InputEmpty if self.eof => self.finished = true,
                DecoderResult::InputEmpty => {}
                DecoderResult::OutputFull if written == 0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "encoding decoder made no progress",
                    ));
                }
                DecoderResult::OutputFull => {}
                DecoderResult::Malformed(_, _) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "malformed byte sequence for detected encoding",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for StrictDecodeReader<R> {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        if self.output_start == self.output_end {
            self.fill_output()?;
        }
        if self.output_start == self.output_end {
            return Ok(0);
        }
        let count = destination
            .len()
            .min(self.output_end.saturating_sub(self.output_start));
        destination[..count]
            .copy_from_slice(&self.output[self.output_start..self.output_start + count]);
        self.output_start += count;
        Ok(count)
    }
}

struct Utf32Reader<R> {
    inner: R,
    little_endian: bool,
    pending: Vec<u8>,
    output: Vec<u8>,
    output_start: usize,
    finished: bool,
}

impl<R: Read> Utf32Reader<R> {
    fn new(inner: R, little_endian: bool) -> Self {
        Self {
            inner,
            little_endian,
            pending: Vec::with_capacity(TRANSCODE_INPUT_BYTES + 3),
            output: Vec::with_capacity(TRANSCODE_OUTPUT_BYTES),
            output_start: 0,
            finished: false,
        }
    }

    fn fill_output(&mut self) -> io::Result<()> {
        self.output.clear();
        self.output_start = 0;
        while self.output.is_empty() && !self.finished {
            let mut input = [0; TRANSCODE_INPUT_BYTES];
            let read = self.inner.read(&mut input)?;
            if read == 0 {
                self.finished = true;
                if !self.pending.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "incomplete UTF-32 code unit",
                    ));
                }
                break;
            }
            self.pending.extend_from_slice(&input[..read]);
            let complete = self.pending.len() / 4 * 4;
            for unit in self.pending[..complete].chunks_exact(4) {
                let bytes = [unit[0], unit[1], unit[2], unit[3]];
                let scalar = if self.little_endian {
                    u32::from_le_bytes(bytes)
                } else {
                    u32::from_be_bytes(bytes)
                };
                let character = char::from_u32(scalar).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-32 scalar value")
                })?;
                let mut encoded = [0; 4];
                self.output
                    .extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
            self.pending.drain(..complete);
        }
        Ok(())
    }
}

impl<R: Read> Read for Utf32Reader<R> {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        if self.output_start == self.output.len() {
            self.fill_output()?;
        }
        if self.output_start == self.output.len() {
            return Ok(0);
        }
        let count = destination
            .len()
            .min(self.output.len().saturating_sub(self.output_start));
        destination[..count]
            .copy_from_slice(&self.output[self.output_start..self.output_start + count]);
        self.output_start += count;
        Ok(count)
    }
}

#[derive(Debug, Deserialize)]
struct SearchRequest {
    path: PathBuf,
    query: String,
    #[serde(default)]
    regex: bool,
    #[serde(default = "default_true")]
    case_sensitive: bool,
    #[serde(default)]
    globs: Vec<String>,
    #[serde(default)]
    depth: Option<usize>,
    #[serde(default)]
    context_before: usize,
    #[serde(default)]
    context_after: usize,
    #[serde(default = "default_max_matches")]
    max_matches: usize,
}

fn default_true() -> bool {
    true
}

fn default_max_matches() -> usize {
    500
}

#[derive(Debug, Serialize)]
struct SearchOutput {
    path: String,
    matches: Vec<SearchMatch>,
    skipped_binary: usize,
    returned: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tip: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchMatch {
    path: String,
    column: usize,
    match_length: usize,
    before: BTreeMap<String, String>,
    match_text: BTreeMap<String, String>,
    after: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct SearchError {
    code: String,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tip: Option<String>,
}

impl SearchError {
    fn invalid_regex(error: impl std::fmt::Display) -> Self {
        Self {
            code: "invalid_regex".to_owned(),
            message: error.to_string(),
            retryable: false,
            tip: Some(
                "Correct the Rust/ripgrep regular expression, or set regex=false for a literal search."
                    .to_owned(),
            ),
        }
    }

    fn invalid_glob(error: impl std::fmt::Display) -> Self {
        Self {
            code: "invalid_arguments".to_owned(),
            message: format!("invalid glob: {error}"),
            retryable: false,
            tip: Some("Use valid glob patterns, or omit globs.".to_owned()),
        }
    }

    fn execution(error: impl std::fmt::Display) -> Self {
        Self {
            code: "search_error".to_owned(),
            message: error.to_string(),
            retryable: false,
            tip: Some("Check the search path and narrow the search scope if needed.".to_owned()),
        }
    }
}

#[derive(Default)]
struct ProbeSink {
    matched: bool,
    binary: bool,
    invalid_utf8: bool,
    invalid_utf8_sample: Vec<u8>,
}

impl ProbeSink {
    fn inspect(&mut self, bytes: &[u8]) -> bool {
        if bytes.contains(&0) {
            self.binary = true;
            return false;
        }
        if let Err(error) = std::str::from_utf8(bytes) {
            self.invalid_utf8 = true;
            if self.invalid_utf8_sample.is_empty() {
                let start = error
                    .valid_up_to()
                    .saturating_sub(ENCODING_SAMPLE_BYTES / 4);
                let end = bytes.len().min(start + ENCODING_SAMPLE_BYTES);
                self.invalid_utf8_sample
                    .extend_from_slice(&bytes[start..end]);
            }
            return false;
        }
        true
    }
}

impl Sink for ProbeSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, matched: &SinkMatch<'_>) -> io::Result<bool> {
        self.matched = true;
        Ok(self.inspect(matched.bytes()))
    }

    fn context(&mut self, _searcher: &Searcher, context: &SinkContext<'_>) -> io::Result<bool> {
        Ok(self.inspect(context.bytes()))
    }

    fn binary_data(&mut self, _searcher: &Searcher, _binary_byte_offset: u64) -> io::Result<bool> {
        self.binary = true;
        Ok(false)
    }
}

#[derive(Default)]
struct LineSink {
    lines: Vec<String>,
    binary: bool,
    invalid_utf8: bool,
}

impl LineSink {
    fn push(&mut self, bytes: &[u8]) -> bool {
        if bytes.contains(&0) {
            self.binary = true;
            return false;
        }
        let mut end = bytes.len();
        if end > 0 && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        for logical_line in bytes[..end].split(|byte| *byte == b'\r') {
            let Ok(line) = std::str::from_utf8(logical_line) else {
                self.invalid_utf8 = true;
                return false;
            };
            self.lines.push(line.to_owned());
        }
        true
    }
}

impl Sink for LineSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, matched: &SinkMatch<'_>) -> io::Result<bool> {
        Ok(self.push(matched.bytes()))
    }

    fn context(&mut self, _searcher: &Searcher, context: &SinkContext<'_>) -> io::Result<bool> {
        Ok(self.push(context.bytes()))
    }

    fn binary_data(&mut self, _searcher: &Searcher, _binary_byte_offset: u64) -> io::Result<bool> {
        self.binary = true;
        Ok(false)
    }
}

pub fn run<R: BufRead, W: Write>(input: R, mut output: W, workspace: &Path) -> Result<()> {
    let frame = match serde_json::from_reader::<_, SearchRequest>(input) {
        Ok(request) => match search(request, workspace) {
            Ok(output) => json!({"ok": true, "output": output}),
            Err(error) => json!({"ok": false, "error": error}),
        },
        Err(error) => json!({
            "ok": false,
            "error": SearchError {
                code: "invalid_arguments".to_owned(),
                message: format!("invalid File.Search worker request: {error}"),
                retryable: false,
                tip: None,
            }
        }),
    };
    serde_json::to_writer(&mut output, &frame)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn search(
    request: SearchRequest,
    workspace: &Path,
) -> std::result::Result<SearchOutput, SearchError> {
    let matcher = build_matcher(&request)?;
    let globs = build_globs(&request.globs)?;
    let root = request
        .path
        .canonicalize()
        .map_err(|error| SearchError::execution(format!("cannot resolve search path: {error}")))?;
    if !root.is_file() && !root.is_dir() {
        return Err(SearchError::execution(
            "search path is not a regular file or directory",
        ));
    }
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let root_display = display_path(&root, &workspace);
    let mut matches = Vec::new();
    let mut skipped_binary = 0usize;

    for candidate in candidates(&root, request.depth) {
        let path = match candidate {
            Ok(path) => path,
            Err(()) => {
                skipped_binary += 1;
                continue;
            }
        };
        let display = display_path(&path, &workspace);
        if !glob_matches(globs.as_ref(), &display, &path) {
            continue;
        }

        let encoding_probe = match detect_file_encoding(&path) {
            Ok(Some(probe)) => probe,
            Ok(None) | Err(_) => {
                skipped_binary += 1;
                continue;
            }
        };
        let mut encoding = encoding_probe.encoding;
        let mut probe = ProbeSink::default();
        let mut probe_result = search_file(&matcher, &path, encoding, &mut probe);
        if probe_result.is_ok() && !probe.binary && probe.invalid_utf8 && encoding.is_utf8() {
            if let Some(fallback) =
                detect_legacy_from_fragments(&encoding_probe.sample, &probe.invalid_utf8_sample)
            {
                encoding = fallback;
                probe = ProbeSink::default();
                probe_result = search_file(&matcher, &path, encoding, &mut probe);
            }
        }
        if probe_result.is_err() || probe.binary || probe.invalid_utf8 {
            skipped_binary += 1;
            continue;
        }
        if !probe.matched {
            continue;
        }

        let mut lines = LineSink::default();
        if search_file(&matcher, &path, encoding, &mut lines).is_err()
            || lines.binary
            || lines.invalid_utf8
        {
            skipped_binary += 1;
            continue;
        }
        append_file_matches(
            &matcher,
            &display,
            &lines.lines,
            request.context_before,
            request.context_after,
            request.max_matches,
            &mut matches,
        )?;
        if matches.len() >= request.max_matches {
            return Ok(SearchOutput {
                path: root_display,
                returned: matches.len(),
                matches,
                skipped_binary,
                truncated: true,
                tip: None,
            });
        }
    }

    let tip = matches.is_empty().then(|| {
        "No text matched. Check query, path, depth, globs, case_sensitive, and regex if you expected results."
            .to_owned()
    });
    Ok(SearchOutput {
        path: root_display,
        returned: matches.len(),
        matches,
        skipped_binary,
        truncated: false,
        tip,
    })
}

fn build_matcher(request: &SearchRequest) -> std::result::Result<RegexMatcher, SearchError> {
    RegexMatcherBuilder::new()
        .fixed_strings(!request.regex)
        .case_insensitive(!request.case_sensitive)
        .line_terminator(Some(b'\n'))
        .ban_byte(Some(0))
        .build(&request.query)
        .map_err(SearchError::invalid_regex)
}

fn build_globs(patterns: &[String]) -> std::result::Result<Option<GlobSet>, SearchError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(SearchError::invalid_glob)?);
    }
    builder.build().map(Some).map_err(SearchError::invalid_glob)
}

fn build_searcher(passthru: bool) -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .passthru(passthru)
        .binary_detection(BinaryDetection::quit(0))
        .bom_sniffing(false)
        .build()
}

fn detect_file_encoding(path: &Path) -> io::Result<Option<EncodingProbe>> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut sample = Vec::with_capacity(ENCODING_SAMPLE_BYTES);
    Read::by_ref(&mut file)
        .take(ENCODING_SAMPLE_BYTES as u64)
        .read_to_end(&mut sample)?;
    let complete = size <= sample.len() as u64;

    let encoding = if sample.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        SearchEncoding::Utf32 {
            little_endian: false,
            bom_len: 4,
        }
    } else if sample.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        SearchEncoding::Utf32 {
            little_endian: true,
            bom_len: 4,
        }
    } else if sample.starts_with(&[0xEF, 0xBB, 0xBF]) {
        SearchEncoding::Utf8 { bom_len: 3 }
    } else if sample.starts_with(&[0xFE, 0xFF]) {
        SearchEncoding::EncodingRs {
            encoding: UTF_16BE,
            bom_len: 2,
        }
    } else if sample.starts_with(&[0xFF, 0xFE]) {
        SearchEncoding::EncodingRs {
            encoding: UTF_16LE,
            bom_len: 2,
        }
    } else {
        if sample_looks_binary(&sample) {
            return Ok(None);
        }
        if utf8_prefix_is_valid(&sample, complete) {
            SearchEncoding::Utf8 { bom_len: 0 }
        } else {
            let Some(encoding) = detect_legacy_encoding(&sample, complete) else {
                return Ok(None);
            };
            encoding
        }
    };
    Ok(Some(EncodingProbe { encoding, sample }))
}

fn utf8_prefix_is_valid(sample: &[u8], complete: bool) -> bool {
    match std::str::from_utf8(sample) {
        Ok(_) => true,
        Err(error) => !complete && error.error_len().is_none(),
    }
}

fn sample_looks_binary(sample: &[u8]) -> bool {
    if sample.contains(&0) {
        return true;
    }
    let controls = sample
        .iter()
        .filter(|byte| **byte < 0x20 && !matches!(**byte, b'\t' | b'\n' | b'\r' | 0x0C))
        .count();
    controls >= 3 && controls.saturating_mul(100) > sample.len().max(1)
}

fn detect_legacy_encoding(sample: &[u8], complete: bool) -> Option<SearchEncoding> {
    if sample.is_empty() || sample_looks_binary(sample) {
        return None;
    }
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Allow);
    if !detector.feed(sample, complete) {
        return None;
    }
    Some(search_encoding_for_guess(
        detector.guess(None, Utf8Detection::Deny),
    ))
}

fn detect_legacy_from_fragments(prefix: &[u8], invalid: &[u8]) -> Option<SearchEncoding> {
    if invalid.is_empty() || sample_looks_binary(invalid) {
        return None;
    }
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Allow);
    let prefix_limit = prefix.len().min(ENCODING_SAMPLE_BYTES / 2);
    let invalid_limit = invalid
        .len()
        .min(ENCODING_SAMPLE_BYTES.saturating_sub(prefix_limit));
    let mut non_ascii = detector.feed(&prefix[..prefix_limit], false);
    non_ascii |= detector.feed(&invalid[..invalid_limit], false);
    non_ascii.then(|| search_encoding_for_guess(detector.guess(None, Utf8Detection::Deny)))
}

fn search_encoding_for_guess(encoding: &'static Encoding) -> SearchEncoding {
    SearchEncoding::EncodingRs {
        encoding: if std::ptr::eq(encoding, GBK) {
            GB18030
        } else {
            encoding
        },
        bom_len: 0,
    }
}

fn search_file<S: Sink<Error = io::Error>>(
    matcher: &RegexMatcher,
    path: &Path,
    encoding: SearchEncoding,
    sink: &mut S,
) -> io::Result<()> {
    match encoding {
        SearchEncoding::Utf8 { bom_len: 0 } => {
            build_searcher(true).search_path(matcher, path, sink)
        }
        SearchEncoding::Utf8 { bom_len } => {
            let file = open_after_bom(path, bom_len)?;
            build_searcher(true).search_reader(matcher, file, sink)
        }
        SearchEncoding::EncodingRs { encoding, bom_len } => {
            let file = open_after_bom(path, bom_len)?;
            let reader = StrictDecodeReader::new(file, encoding);
            build_searcher(true).search_reader(matcher, reader, sink)
        }
        SearchEncoding::Utf32 {
            little_endian,
            bom_len,
        } => {
            let file = open_after_bom(path, bom_len)?;
            let reader = Utf32Reader::new(file, little_endian);
            build_searcher(true).search_reader(matcher, reader, sink)
        }
    }
}

fn open_after_bom(path: &Path, bom_len: usize) -> io::Result<File> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(bom_len as u64))?;
    Ok(file)
}

fn candidates(
    root: &Path,
    depth: Option<usize>,
) -> Box<dyn Iterator<Item = std::result::Result<PathBuf, ()>>> {
    if root.is_file() {
        return Box::new(std::iter::once(Ok(root.to_path_buf())));
    }
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(true)
        .follow_links(false)
        .max_depth(depth)
        .sort_by_file_path(|left, right| left.cmp(right));
    Box::new(builder.build().filter_map(|entry| match entry {
        Ok(entry)
            if !entry.path_is_symlink() && entry.file_type().is_some_and(|kind| kind.is_file()) =>
        {
            Some(Ok(entry.into_path()))
        }
        Ok(_) => None,
        Err(_) => Some(Err(())),
    }))
}

fn glob_matches(globs: Option<&GlobSet>, display: &str, path: &Path) -> bool {
    let Some(globs) = globs else {
        return true;
    };
    globs.is_match(display)
        || path
            .file_name()
            .is_some_and(|name| globs.is_match(name.to_string_lossy().as_ref()))
}

fn append_file_matches(
    matcher: &RegexMatcher,
    path: &str,
    lines: &[String],
    context_before: usize,
    context_after: usize,
    max_matches: usize,
    output: &mut Vec<SearchMatch>,
) -> std::result::Result<(), SearchError> {
    let width = lines.len().to_string().len().max(1);
    for (line_index, line) in lines.iter().enumerate() {
        let mut ranges = Vec::new();
        matcher
            .find_iter(line.as_bytes(), |matched| {
                ranges.push((matched.start(), matched.end()));
                output.len() + ranges.len() < max_matches
            })
            .map_err(SearchError::execution)?;
        for (start, end) in ranges {
            let before_start = line_index.saturating_sub(context_before);
            let after_end = lines.len().min(line_index + 1 + context_after);
            let before = (before_start..line_index)
                .map(|index| (line_key(index, width), lines[index].clone()))
                .collect();
            let match_text = [(line_key(line_index, width), line.clone())]
                .into_iter()
                .collect();
            let after = (line_index + 1..after_end)
                .map(|index| (line_key(index, width), lines[index].clone()))
                .collect();
            output.push(SearchMatch {
                path: path.to_owned(),
                column: line[..start].chars().count() + 1,
                match_length: line[start..end].chars().count(),
                before,
                match_text,
                after,
            });
            if output.len() >= max_matches {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn line_key(index: usize, width: usize) -> String {
    format!("{:0width$}", index + 1, width = width)
}

fn display_path(path: &Path, workspace: &Path) -> String {
    match path.strip_prefix(workspace) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => public_absolute_path(path),
    }
}

fn public_absolute_path(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        if let Some(rest) = value.strip_prefix("//?/UNC/") {
            return format!("//{rest}");
        }
        if let Some(rest) = value.strip_prefix("//?/") {
            return rest.to_owned();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn logical_lines_strip_crlf_and_split_cr_only_content() {
        let mut sink = LineSink::default();
        assert!(sink.push(b"first\rsecond\r\n"));
        assert_eq!(sink.lines, ["first", "second"]);
    }

    #[test]
    fn line_keys_use_total_line_width() {
        assert_eq!(line_key(8, 2), "09");
        assert_eq!(line_key(9, 2), "10");
    }

    #[test]
    fn gbk_detection_uses_gb18030_superset() {
        let detected = detect_legacy_encoding(
            b"\xbc\xf2\xcc\xe5\xd6\xd0\xce\xc4\xc4\xda\xc8\xdd\xa3\xac\xc4\xe3\xba\xc3\xca\xc0\xbd\xe7\xa1\xa3",
            true,
        )
        .unwrap();
        match detected {
            SearchEncoding::EncodingRs { encoding, .. } => {
                assert!(std::ptr::eq(encoding, GB18030));
            }
            _ => panic!("expected a legacy encoding"),
        }
    }

    #[test]
    fn strict_decoder_rejects_malformed_input() {
        let mut reader = StrictDecodeReader::new(Cursor::new(vec![0x81]), GB18030);
        let mut output = Vec::new();
        assert_eq!(
            reader.read_to_end(&mut output).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn utf32_reader_streams_unicode_scalars() {
        let input = [0x41, 0, 0, 0, 0x2D, 0x4E, 0, 0];
        let mut reader = Utf32Reader::new(Cursor::new(input), true);
        let mut output = String::new();
        reader.read_to_string(&mut output).unwrap();
        assert_eq!(output, "A中");
    }
}
