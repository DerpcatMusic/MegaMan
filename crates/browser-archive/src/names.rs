use crate::{ArchiveError, ErrorKind, NameEncoding, Result};
use std::collections::HashMap;

pub(crate) fn decode<'a>(
    raw: &[u8],
    utf8: bool,
    extras: impl Iterator<Item = (u16, &'a [u8])>,
) -> (String, NameEncoding, Option<String>) {
    let mut unicode = None;
    let mut issue = None;
    for (kind, bytes) in extras {
        if kind != 0x7075 {
            continue;
        }
        if unicode.is_some() {
            issue = Some("duplicate Unicode Path extra".into());
        }
        if bytes.len() < 5
            || bytes[0] != 1
            || crc32fast::hash(raw) != u32::from_le_bytes(bytes[1..5].try_into().unwrap())
        {
            issue = Some("invalid Unicode Path extra or name CRC".into());
            continue;
        }
        match std::str::from_utf8(&bytes[5..]) {
            Ok(name) => unicode = Some(name.to_owned()),
            Err(_) => issue = Some("invalid UTF-8 Unicode Path extra".into()),
        }
    }
    if utf8 {
        match std::str::from_utf8(raw) {
            Ok(name) => {
                if unicode.as_ref().is_some_and(|extra| extra != name) {
                    issue = Some("conflicting flagged UTF-8 and Unicode Path extra".into());
                }
                (name.to_owned(), NameEncoding::Utf8, issue)
            }
            Err(_) => (
                escaped(raw),
                NameEncoding::Invalid,
                Some("invalid flagged UTF-8 filename".into()),
            ),
        }
    } else if let Some(name) = unicode {
        (name, NameEncoding::UnicodeExtra, issue)
    } else {
        (
            raw.iter()
                .map(|&b| {
                    if b < 128 {
                        char::from(b)
                    } else {
                        CP437[(b - 128) as usize]
                    }
                })
                .collect(),
            NameEncoding::Cp437,
            issue,
        )
    }
}

fn escaped(raw: &[u8]) -> String {
    raw.iter()
        .map(|b| {
            if b.is_ascii_graphic() {
                char::from(*b).to_string()
            } else {
                format!("\\x{b:02x}")
            }
        })
        .collect()
}

/// Conservative portable naming policy. Native exclusive creates are still
/// authoritative for filesystem-specific normalization/case aliases.
pub(crate) fn components(name: &str, maximum: usize) -> Result<Vec<String>> {
    let invalid = |why| ArchiveError::new(ErrorKind::UnsafePath, why);
    if name.is_empty()
        || name.len() > u16::MAX as usize
        || name.starts_with('/')
        || name.contains(['\\', '\0', ':'])
    {
        return Err(invalid("empty, absolute, drive/ADS, backslash or NUL path"));
    }
    let body = name.strip_suffix('/').unwrap_or(name);
    let mut result = Vec::new();
    for part in body.split('/') {
        if part.is_empty()
            || matches!(part, "." | "..")
            || part.ends_with(['.', ' '])
            || part
                .chars()
                .any(|c| c.is_control() || "<>\"|?*".contains(c))
        {
            return Err(invalid("unsafe or non-portable archive path component"));
        }
        let stem = part.split('.').next().unwrap().to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || stem
                .strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
        {
            return Err(invalid("Windows reserved path component"));
        }
        result.push(part.to_owned());
        if result.len() > maximum {
            return Err(invalid("path component-count limit"));
        }
    }
    Ok(result)
}

pub(crate) fn check_tree<'a>(
    paths: impl Iterator<Item = (&'a [String], bool)>,
    maximum_bytes: usize,
) -> Result<()> {
    check_tree_control(paths, maximum_bytes, None)
}
pub(crate) fn check_tree_control<'a>(
    paths: impl Iterator<Item = (&'a [String], bool)>,
    maximum_bytes: usize,
    control: Option<&crate::Control<'_>>,
) -> Result<()> {
    let mut kinds = HashMap::<Vec<String>, bool>::new();
    let mut folded = HashMap::<Vec<String>, Vec<String>>::new();
    let mut charged = 0usize;
    for (path, directory) in paths {
        for length in 1..=path.len() {
            if let Some(control) = control {
                control.check()?;
            }
            let strings = path[..length].iter().map(String::len).sum::<usize>();
            charged = charged
                .checked_add(length.saturating_mul(192))
                .and_then(|value| value.checked_add(strings.saturating_mul(8)))
                .ok_or_else(|| {
                    ArchiveError::new(ErrorKind::LimitExceeded, "path-index arithmetic overflow")
                })?;
            if charged > maximum_bytes {
                return Err(ArchiveError::new(
                    ErrorKind::LimitExceeded,
                    "path collision-index budget",
                ));
            }
            let prefix = path[..length].to_vec();
            let fold: Vec<_> = prefix.iter().map(|s| s.to_lowercase()).collect();
            if let Some(previous) = folded.insert(fold, prefix.clone())
                && previous != prefix
            {
                return Err(ArchiveError::new(
                    ErrorKind::Conflict,
                    "case-alias destination paths",
                ));
            }
            let is_directory = length < path.len() || directory;
            if let Some(old) = kinds.insert(prefix, is_directory)
                && (old != is_directory || !is_directory)
            {
                return Err(ArchiveError::new(
                    ErrorKind::Conflict,
                    "file/directory or duplicate destination conflict",
                ));
            }
        }
    }
    Ok(())
}

// Unicode mapping for the upper half of IBM Code Page 437.
const CP437: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ',
    'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ',
    'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕',
    '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦',
    '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐',
    '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±',
    '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_extra_needs_matching_crc_and_strict_utf8() {
        let raw = b"legacy";
        let mut extra = vec![1];
        extra.extend(crc32fast::hash(raw).to_le_bytes());
        extra.extend("שלום".as_bytes());
        let (name, encoding, issue) = decode(raw, false, [(0x7075, extra.as_slice())].into_iter());
        assert_eq!(name, "שלום");
        assert_eq!(encoding, NameEncoding::UnicodeExtra);
        assert!(issue.is_none());
        extra[1] ^= 1;
        assert!(
            decode(raw, false, [(0x7075, extra.as_slice())].into_iter())
                .2
                .is_some()
        );
    }
    #[test]
    fn conflicting_unicode_extras_are_not_name_repairs() {
        let raw = b"utf8";
        let mut extra = vec![1];
        extra.extend(crc32fast::hash(raw).to_le_bytes());
        extra.extend(b"different");
        assert!(
            decode(raw, true, [(0x7075, extra.as_slice())].into_iter())
                .2
                .is_some()
        );
        assert!(
            decode(
                raw,
                false,
                [(0x7075, extra.as_slice()), (0x7075, extra.as_slice())].into_iter()
            )
            .2
            .is_some()
        );
    }
}
