use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Result;

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
}

impl ProbeSink {
    fn inspect(&mut self, bytes: &[u8]) -> bool {
        if bytes.contains(&0) {
            self.binary = true;
            return false;
        }
        if std::str::from_utf8(bytes).is_err() {
            self.invalid_utf8 = true;
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

        let mut probe = ProbeSink::default();
        if build_searcher(true)
            .search_path(&matcher, &path, &mut probe)
            .is_err()
            || probe.binary
            || probe.invalid_utf8
        {
            skipped_binary += 1;
            continue;
        }
        if !probe.matched {
            continue;
        }

        let mut lines = LineSink::default();
        if build_searcher(true)
            .search_path(&matcher, &path, &mut lines)
            .is_err()
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
        .build()
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
}
