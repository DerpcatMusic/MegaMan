//! Pure Linux native-path payload codecs. No filesystem, clipboard, UI or drag runtime access.
//! See docs/NATIVE-TRANSFER.md for the deliberately narrow accepted URI profile.
#![forbid(unsafe_code)]

use std::{
    fmt,
    path::{Path, PathBuf},
};

pub const URI_LIST_MIME: &str = "text/uri-list";
pub const GNOME_COPIED_FILES_MIME: &str = "x-special/gnome-copied-files";
pub const KDE_CUT_SELECTION_MIME: &str = "application/x-kde-cutselection";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyIntent {
    Copy,
    Cut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileBatch {
    pub intent: CopyIntent,
    /// Order and duplicate native spellings are preserved. No paths have been validated on disk.
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_payload_bytes: usize,
    /// Physical lines, including comments and the GNOME intent header.
    pub max_lines: usize,
    pub max_items: usize,
    /// URI/comment/header line bytes, excluding CRLF/LF terminators.
    pub max_line_bytes: usize,
    pub max_path_bytes: usize,
    pub max_total_path_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_payload_bytes: 8 << 20,
            max_lines: 8192,
            max_items: 4096,
            max_line_bytes: 64 << 10,
            max_path_bytes: 16 << 10,
            max_total_path_bytes: 4 << 20,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    UnsupportedPlatform,
    EmptyBatch,
    PayloadTooLarge,
    TooManyLines,
    TooManyItems,
    LineTooLong,
    PathTooLong,
    PathsTooLarge,
    InvalidScheme,
    RemoteAuthority,
    AbsolutePathRequired,
    AmbiguousLeadingSlashes,
    InvalidByte,
    InvalidEscape,
    EncodedSeparator,
    Nul,
    QueryOrFragment,
    InvalidLineEnding,
    EmptyLine,
    InvalidIntent,
    SizeOverflow,
    AllocationFailed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferError {
    pub kind: ErrorKind,
    /// One-based physical payload line when decoding a batch.
    pub line: Option<usize>,
    /// One-based URI/native-path item; comments and intent headers are not items.
    pub item: Option<usize>,
}
impl TransferError {
    fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            line: None,
            item: None,
        }
    }
    fn located(mut self, line: Option<usize>, item: usize) -> Self {
        self.line = line;
        self.item = Some(item);
        self
    }
}
impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "file transfer payload {:?}", self.kind)?;
        if let Some(line) = self.line {
            write!(f, " at line {line}")?;
        }
        if let Some(item) = self.item {
            write!(f, " (item {item})")?;
        }
        Ok(())
    }
}
impl std::error::Error for TransferError {}
pub type Result<T> = std::result::Result<T, TransferError>;

/// ASCII file:/// URI, preserving every native Linux byte except invalid NUL and ambiguous // roots.
pub fn encode_file_uri(path: &Path, limits: &Limits) -> Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        linux::encode_one(path, limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, limits);
        Err(TransferError::new(ErrorKind::UnsupportedPlatform))
    }
}
/// Accepts local file:/, file:/// and file://localhost/ URIs. Percent octets decode exactly once.
pub fn decode_file_uri(uri: &[u8], limits: &Limits) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        linux::decode_one(uri, limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (uri, limits);
        Err(TransferError::new(ErrorKind::UnsupportedPlatform))
    }
}
/// Canonical URI-list output uses CRLF, including after the final URI. No intent is implied.
pub fn encode_uri_list(paths: &[PathBuf], limits: &Limits) -> Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        linux::encode_batch(paths, None, limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (paths, limits);
        Err(TransferError::new(ErrorKind::UnsupportedPlatform))
    }
}
/// All-or-nothing. Accepts CRLF or LF and an unterminated final line; only first-column # is a comment.
pub fn decode_uri_list(payload: &[u8], limits: &Limits) -> Result<Vec<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        linux::decode_batch(payload, false, limits).map(|(_, paths)| paths)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (payload, limits);
        Err(TransferError::new(ErrorKind::UnsupportedPlatform))
    }
}
/// GNOME canonical form: lowercase copy/cut, then LF before each URI, with no trailing LF.
pub fn encode_gnome_copied_files(batch: &FileBatch, limits: &Limits) -> Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        linux::encode_batch(&batch.paths, Some(batch.intent), limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (batch, limits);
        Err(TransferError::new(ErrorKind::UnsupportedPlatform))
    }
}
pub fn decode_gnome_copied_files(payload: &[u8], limits: &Limits) -> Result<FileBatch> {
    #[cfg(target_os = "linux")]
    {
        linux::decode_batch(payload, true, limits).map(|(intent, paths)| FileBatch {
            intent: intent.unwrap(),
            paths,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (payload, limits);
        Err(TransferError::new(ErrorKind::UnsupportedPlatform))
    }
}
/// Companion MIME value, not a URI list. A Cut intent is not permission to delete a source.
pub fn encode_kde_cut_selection(intent: CopyIntent) -> &'static [u8; 1] {
    match intent {
        CopyIntent::Copy => b"0",
        CopyIntent::Cut => b"1",
    }
}
/// Deliberately stricter than KIO's first-byte check: reject any ambiguous or malformed indicator.
pub fn decode_kde_cut_selection(payload: &[u8]) -> Result<CopyIntent> {
    match payload {
        b"0" => Ok(CopyIntent::Copy),
        b"1" => Ok(CopyIntent::Cut),
        _ => Err(TransferError::new(ErrorKind::InvalidIntent)),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        ffi::OsString,
        os::unix::ffi::{OsStrExt, OsStringExt},
    };
    const PREFIX: &[u8] = b"file://";
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    fn error(kind: ErrorKind) -> TransferError {
        TransferError::new(kind)
    }
    fn add(a: usize, b: usize) -> Result<usize> {
        a.checked_add(b)
            .ok_or_else(|| error(ErrorKind::SizeOverflow))
    }
    fn unreserved(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)
    }
    fn pchar(byte: u8) -> bool {
        unreserved(byte) || b"!$&'()*+,;=:@/".contains(&byte)
    }
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    fn native_path(bytes: &[u8], limits: &Limits) -> Result<()> {
        if bytes.len() > limits.max_path_bytes {
            return Err(error(ErrorKind::PathTooLong));
        }
        if !bytes.starts_with(b"/") {
            return Err(error(ErrorKind::AbsolutePathRequired));
        }
        if bytes.starts_with(b"//") {
            return Err(error(ErrorKind::AmbiguousLeadingSlashes));
        }
        if bytes.contains(&0) {
            return Err(error(ErrorKind::Nul));
        }
        Ok(())
    }
    fn wire_len(path: &Path, limits: &Limits) -> Result<usize> {
        let bytes = path.as_os_str().as_bytes();
        native_path(bytes, limits)?;
        let mut len = PREFIX.len();
        for &byte in bytes {
            len = add(
                len,
                if byte == b'/' || unreserved(byte) {
                    1
                } else {
                    3
                },
            )?;
        }
        if len > limits.max_line_bytes {
            return Err(error(ErrorKind::LineTooLong));
        }
        Ok(len)
    }
    fn append_uri(path: &Path, output: &mut Vec<u8>) {
        output.extend_from_slice(PREFIX);
        for &byte in path.as_os_str().as_bytes() {
            if byte == b'/' || unreserved(byte) {
                output.push(byte);
            } else {
                output.extend_from_slice(&[
                    b'%',
                    HEX[(byte >> 4) as usize],
                    HEX[(byte & 15) as usize],
                ]);
            }
        }
    }
    fn reserve(bytes: usize) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(bytes)
            .map_err(|_| error(ErrorKind::AllocationFailed))?;
        Ok(output)
    }
    pub(super) fn encode_one(path: &Path, limits: &Limits) -> Result<Vec<u8>> {
        let length = wire_len(path, limits)?;
        if length > limits.max_payload_bytes {
            return Err(error(ErrorKind::PayloadTooLarge));
        }
        if path.as_os_str().as_bytes().len() > limits.max_total_path_bytes {
            return Err(error(ErrorKind::PathsTooLarge));
        }
        let mut output = reserve(length)?;
        append_uri(path, &mut output);
        Ok(output)
    }
    pub(super) fn decode_one(uri: &[u8], limits: &Limits) -> Result<PathBuf> {
        if uri.len() > limits.max_payload_bytes {
            return Err(error(ErrorKind::PayloadTooLarge));
        }
        if uri.len() > limits.max_line_bytes {
            return Err(error(ErrorKind::LineTooLong));
        }
        if uri.contains(&0) {
            return Err(error(ErrorKind::Nul));
        }
        if !uri
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"file:"))
        {
            return Err(error(ErrorKind::InvalidScheme));
        }
        let rest = &uri[5..];
        let path = if let Some(after_slashes) = rest.strip_prefix(b"//") {
            let end = after_slashes
                .iter()
                .position(|b| *b == b'/')
                .unwrap_or(after_slashes.len());
            let authority = &after_slashes[..end];
            if !authority.is_empty() && !authority.eq_ignore_ascii_case(b"localhost") {
                return Err(error(ErrorKind::RemoteAuthority));
            }
            &after_slashes[end..]
        } else {
            rest
        };
        if !path.starts_with(b"/") {
            return Err(error(ErrorKind::AbsolutePathRequired));
        }
        if path.starts_with(b"//") {
            return Err(error(ErrorKind::AmbiguousLeadingSlashes));
        }
        let mut decoded = reserve(
            path.len()
                .min(limits.max_path_bytes)
                .min(limits.max_total_path_bytes),
        )?;
        let mut offset = 0;
        while offset < path.len() {
            let byte = path[offset];
            let decoded_byte = if byte == b'%' {
                let first = path.get(offset + 1).and_then(|b| hex(*b));
                let second = path.get(offset + 2).and_then(|b| hex(*b));
                let value = first
                    .zip(second)
                    .map(|(a, b)| (a << 4) | b)
                    .ok_or_else(|| error(ErrorKind::InvalidEscape))?;
                if value == b'/' {
                    return Err(error(ErrorKind::EncodedSeparator));
                }
                offset += 3;
                value
            } else {
                if byte == b'?' || byte == b'#' {
                    return Err(error(ErrorKind::QueryOrFragment));
                }
                if !pchar(byte) {
                    return Err(error(ErrorKind::InvalidByte));
                }
                offset += 1;
                byte
            };
            if decoded_byte == 0 {
                return Err(error(ErrorKind::Nul));
            }
            if decoded.len() >= limits.max_path_bytes {
                return Err(error(ErrorKind::PathTooLong));
            }
            if decoded.len() >= limits.max_total_path_bytes {
                return Err(error(ErrorKind::PathsTooLarge));
            }
            decoded.push(decoded_byte);
        }
        if decoded.len() > limits.max_total_path_bytes {
            return Err(error(ErrorKind::PathsTooLarge));
        }
        Ok(PathBuf::from(OsString::from_vec(decoded)))
    }

    pub(super) fn encode_batch(
        paths: &[PathBuf],
        intent: Option<CopyIntent>,
        limits: &Limits,
    ) -> Result<Vec<u8>> {
        if paths.is_empty() {
            return Err(error(ErrorKind::EmptyBatch));
        }
        if paths.len() > limits.max_items {
            return Err(error(ErrorKind::TooManyItems));
        }
        if add(paths.len(), usize::from(intent.is_some()))? > limits.max_lines {
            return Err(error(ErrorKind::TooManyLines));
        }
        let header = match intent {
            None => &b""[..],
            Some(CopyIntent::Copy) => &b"copy"[..],
            Some(CopyIntent::Cut) => &b"cut"[..],
        };
        if header.len() > limits.max_line_bytes {
            return Err(error(ErrorKind::LineTooLong));
        }
        let mut length = header.len();
        let mut path_total = 0;
        for (index, path) in paths.iter().enumerate() {
            let uri_length = wire_len(path, limits).map_err(|e| e.located(None, index + 1))?;
            path_total = add(path_total, path.as_os_str().as_bytes().len())?;
            if path_total > limits.max_total_path_bytes {
                return Err(error(ErrorKind::PathsTooLarge).located(None, index + 1));
            }
            length = add(
                length,
                add(uri_length, if intent.is_some() { 1 } else { 2 })?,
            )?;
            if length > limits.max_payload_bytes {
                return Err(error(ErrorKind::PayloadTooLarge).located(None, index + 1));
            }
        }
        // No output allocation happens until every path and the complete encoded batch fit.
        let mut output = reserve(length)?;
        output.extend_from_slice(header);
        for path in paths {
            if intent.is_some() {
                output.push(b'\n');
            }
            append_uri(path, &mut output);
            if intent.is_none() {
                output.extend_from_slice(b"\r\n");
            }
        }
        Ok(output)
    }

    pub(super) fn decode_batch(
        payload: &[u8],
        gnome: bool,
        limits: &Limits,
    ) -> Result<(Option<CopyIntent>, Vec<PathBuf>)> {
        if payload.len() > limits.max_payload_bytes {
            return Err(error(ErrorKind::PayloadTooLarge));
        }
        if payload.is_empty() {
            return Err(error(ErrorKind::EmptyBatch));
        }
        if payload.contains(&0) {
            return Err(error(ErrorKind::Nul));
        }
        let mut paths = Vec::new();
        let mut path_total = 0;
        let mut intent = None;
        let mut start = 0;
        for (index, raw) in payload.split(|b| *b == b'\n').enumerate() {
            let line_number = index + 1;
            // text/uri-list conventionally has one final line terminator; this isn't an empty URI.
            if raw.is_empty() && start == payload.len() && payload.ends_with(b"\n") && !gnome {
                break;
            }
            let at_line = |kind| TransferError {
                kind,
                line: Some(line_number),
                item: None,
            };
            if line_number > limits.max_lines {
                return Err(at_line(ErrorKind::TooManyLines));
            }
            let terminated = start + raw.len() < payload.len();
            let line = if !gnome && terminated {
                raw.strip_suffix(b"\r").unwrap_or(raw)
            } else {
                raw
            };
            if line.contains(&b'\r') {
                return Err(at_line(ErrorKind::InvalidLineEnding));
            }
            if line.len() > limits.max_line_bytes {
                return Err(at_line(ErrorKind::LineTooLong));
            }
            if gnome && index == 0 {
                intent = Some(match line {
                    b"copy" => CopyIntent::Copy,
                    b"cut" => CopyIntent::Cut,
                    _ => return Err(at_line(ErrorKind::InvalidIntent)),
                });
            } else if !gnome && line.starts_with(b"#") {
                // Only the registered first-column comment form is ignored. No trimming occurs.
            } else {
                if line.is_empty() {
                    return Err(at_line(ErrorKind::EmptyLine));
                }
                if paths.len() >= limits.max_items {
                    return Err(at_line(ErrorKind::TooManyItems)
                        .located(Some(line_number), paths.len() + 1));
                }
                let mut item_limits = limits.clone();
                item_limits.max_total_path_bytes -= path_total;
                let path = decode_one(line, &item_limits)
                    .map_err(|e| e.located(Some(line_number), paths.len() + 1))?;
                path_total = add(path_total, path.as_os_str().as_bytes().len())?;
                if path_total > limits.max_total_path_bytes {
                    return Err(at_line(ErrorKind::PathsTooLarge)
                        .located(Some(line_number), paths.len() + 1));
                }
                paths
                    .try_reserve(1)
                    .map_err(|_| at_line(ErrorKind::AllocationFailed))?;
                paths.push(path);
            }
            start = add(start, add(raw.len(), 1)?)?;
        }
        if paths.is_empty() {
            return Err(error(ErrorKind::EmptyBatch));
        }
        Ok((intent, paths))
    }
}
