//! Bounded, checksummed commit breadcrumbs, not an automatic recovery/undo implementation.
//! Records use native Linux leaf-name bytes, never display strings. A CommitIntent without a
//! later Committed record means the rename outcome is uncertain until native identities are checked.
//! Version2 optionally records desired root metadata and observed ContentPublished /
//! MetadataFinalized transitions. Those phases alone are not power-loss durability proof.
//! Existing version1 records remain decodable; no automatic recovery is performed.
use crate::{ErrorKind, NativeIdentity, OperationError, Result};
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};

pub const VERSION: u16 = 2;
pub const MAX_LOG_BYTES: usize = 32 * 1024;
const MAGIC: &[u8; 8] = b"BRCOPY01";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum JournalPhase {
    Staging = 1,
    Verified = 2,
    CommitIntent = 3,
    Committed = 4,
    ContentPublished = 5,
    MetadataFinalized = 6,
}
/// Exact desired root metadata; no ownership/ACL or special permission bits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootMetadata {
    pub mode: u32,
    pub accessed: (i64, u32),
    pub modified: (i64, u32),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    pub operation_id: [u8; 16],
    pub phase: JournalPhase,
    pub destination_leaf: OsString,
    pub payload_identity: Option<NativeIdentity>,
    /// Present in every v2 record for an opted-in root needing finalization.
    /// Absent in legacy v1 journals and ordinary pre-publication transactions.
    pub root_metadata: Option<RootMetadata>,
}
#[derive(Clone, Debug)]
pub struct IntentLog {
    pub records: Vec<Record>,
    /// An incomplete final frame was found. Never infer a clean outcome from this flag.
    pub torn_tail: bool,
}
fn invalid(message: &str) -> OperationError {
    OperationError::new(
        ErrorKind::InvalidJournal,
        "journal",
        PathBuf::new(),
        message,
    )
}
fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
pub(super) fn encode(record: &Record) -> Result<Vec<u8>> {
    let name = record.destination_leaf.as_bytes();
    if name.is_empty()
        || name.len() > 4096
        || name.contains(&0)
        || name.contains(&b'/')
        || name == b"."
        || name == b".."
    {
        return Err(invalid("invalid destination leaf in journal"));
    }
    if record.root_metadata.as_ref().is_some_and(|m| {
        m.mode & !0o777 != 0 || m.accessed.1 >= 1_000_000_000 || m.modified.1 >= 1_000_000_000
    }) {
        return Err(invalid("invalid desired root metadata"));
    }
    let mut body = Vec::with_capacity(77 + name.len());
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&VERSION.to_le_bytes());
    body.push(record.phase as u8);
    body.push(u8::from(record.payload_identity.is_some()));
    body.extend_from_slice(&record.operation_id);
    let id = record.payload_identity.unwrap_or(NativeIdentity {
        device: 0,
        inode: 0,
    });
    body.extend_from_slice(&id.device.to_le_bytes());
    body.extend_from_slice(&id.inode.to_le_bytes());
    body.extend_from_slice(&(name.len() as u32).to_le_bytes());
    body.push(u8::from(record.root_metadata.is_some()));
    let metadata = record.root_metadata.clone().unwrap_or(RootMetadata {
        mode: 0,
        accessed: (0, 0),
        modified: (0, 0),
    });
    body.extend_from_slice(&metadata.mode.to_le_bytes());
    body.extend_from_slice(&metadata.accessed.0.to_le_bytes());
    body.extend_from_slice(&metadata.accessed.1.to_le_bytes());
    body.extend_from_slice(&metadata.modified.0.to_le_bytes());
    body.extend_from_slice(&metadata.modified.1.to_le_bytes());
    body.extend_from_slice(name);
    let mut frame = Vec::with_capacity(body.len() + 8);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    frame.extend_from_slice(&checksum(&body).to_le_bytes());
    Ok(frame)
}

/// Memory-only decoder; callers must bound the read to MAX_LOG_BYTES before calling. No recovery
/// action is performed. A corrupt/unknown record is an error rather than permission to remove data.
pub fn decode(bytes: &[u8]) -> Result<IntentLog> {
    if bytes.len() > MAX_LOG_BYTES {
        return Err(invalid("journal exceeds budget"));
    }
    let mut log = IntentLog {
        records: Vec::new(),
        torn_tail: false,
    };
    let mut offset = 0;
    while offset < bytes.len() {
        if log.records.len() == 6 {
            return Err(invalid("too many journal records"));
        }
        let remaining = &bytes[offset..];
        if remaining.len() < 4 {
            log.torn_tail = true;
            break;
        }
        let length = u32::from_le_bytes(remaining[..4].try_into().unwrap()) as usize;
        if !(48..=4173).contains(&length) {
            return Err(invalid("invalid journal frame length"));
        }
        if remaining.len() < length + 8 {
            log.torn_tail = true;
            break;
        }
        let body = &remaining[4..4 + length];
        let expected = u32::from_le_bytes(remaining[4 + length..8 + length].try_into().unwrap());
        if checksum(body) != expected || &body[..8] != MAGIC {
            return Err(invalid("journal checksum or magic mismatch"));
        }
        let version = u16::from_le_bytes(body[8..10].try_into().unwrap());
        let header = match version {
            1 => 48,
            VERSION if length >= 77 => 77,
            _ => return Err(invalid("unsupported journal version")),
        };
        let root_metadata = if version == 1 || body[48] == 0 {
            None
        } else if body[48] == 1 {
            Some(RootMetadata {
                mode: u32::from_le_bytes(body[49..53].try_into().unwrap()),
                accessed: (
                    i64::from_le_bytes(body[53..61].try_into().unwrap()),
                    u32::from_le_bytes(body[61..65].try_into().unwrap()),
                ),
                modified: (
                    i64::from_le_bytes(body[65..73].try_into().unwrap()),
                    u32::from_le_bytes(body[73..77].try_into().unwrap()),
                ),
            })
        } else {
            return Err(invalid("invalid root metadata flag"));
        };
        let phase = match body[10] {
            1 => JournalPhase::Staging,
            2 => JournalPhase::Verified,
            3 => JournalPhase::CommitIntent,
            4 => JournalPhase::Committed,
            5 if root_metadata.is_some() => JournalPhase::ContentPublished,
            6 if root_metadata.is_some() => JournalPhase::MetadataFinalized,
            _ => return Err(invalid("unknown journal phase")),
        };
        let payload_identity = match body[11] {
            0 if phase == JournalPhase::Staging => None,
            1 if phase != JournalPhase::Staging => Some(NativeIdentity {
                device: u64::from_le_bytes(body[28..36].try_into().unwrap()),
                inode: u64::from_le_bytes(body[36..44].try_into().unwrap()),
            }),
            _ => return Err(invalid("invalid journal payload identity")),
        };
        let name_length = u32::from_le_bytes(body[44..48].try_into().unwrap()) as usize;
        if name_length != length - header {
            return Err(invalid("journal name length mismatch"));
        }
        let record = Record {
            operation_id: body[12..28].try_into().unwrap(),
            phase,
            destination_leaf: OsString::from_vec(body[header..].to_vec()),
            payload_identity,
            root_metadata,
        };
        encode(&record)?;
        if let Some(previous) = log.records.last() {
            if previous.operation_id != record.operation_id
                || previous.destination_leaf != record.destination_leaf
                || previous.root_metadata != record.root_metadata
                || !matches!(
                    (previous.phase, record.phase),
                    (JournalPhase::Staging, JournalPhase::Verified)
                        | (JournalPhase::Verified, JournalPhase::CommitIntent)
                        | (JournalPhase::CommitIntent, JournalPhase::Committed)
                        | (JournalPhase::CommitIntent, JournalPhase::ContentPublished)
                        | (
                            JournalPhase::ContentPublished,
                            JournalPhase::MetadataFinalized
                        )
                        | (JournalPhase::MetadataFinalized, JournalPhase::Committed)
                )
                || (record.root_metadata.is_some()
                    && previous.phase == JournalPhase::CommitIntent
                    && record.phase == JournalPhase::Committed)
                || (previous.payload_identity.is_some()
                    && previous.payload_identity != record.payload_identity)
            {
                return Err(invalid("inconsistent journal sequence"));
            }
        } else if phase != JournalPhase::Staging {
            return Err(invalid("journal does not begin with staging"));
        }
        log.records.push(record);
        offset += length + 8;
    }
    if log.records.is_empty() && !log.torn_tail {
        return Err(invalid("empty journal"));
    }
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksum_matches_standard_crc32_vector() {
        assert_eq!(checksum(b"123456789"), 0xcbf4_3926);
    }
    #[test]
    fn corrupt_torn_unknown_and_native_name_records_are_distinct() {
        let mut record = Record {
            operation_id: [42; 16],
            phase: JournalPhase::Staging,
            destination_leaf: OsString::from_vec(b"native-\xff".to_vec()),
            payload_identity: None,
            root_metadata: None,
        };
        let mut bytes = encode(&record).unwrap();
        record.phase = JournalPhase::Verified;
        record.payload_identity = Some(NativeIdentity {
            device: 3,
            inode: 9,
        });
        let next = encode(&record).unwrap();
        bytes.extend_from_slice(&next[..next.len() - 1]);
        let log = decode(&bytes).unwrap();
        assert!(log.torn_tail);
        assert_eq!(log.records.len(), 1);
        bytes.pop();
        bytes[20] ^= 1;
        assert_eq!(decode(&bytes).unwrap_err().kind, ErrorKind::InvalidJournal);
        assert!(decode(&vec![0; MAX_LOG_BYTES + 1]).is_err());
        let mut unknown = encode(&Record {
            phase: JournalPhase::Staging,
            payload_identity: None,
            ..record
        })
        .unwrap();
        unknown[12..14].copy_from_slice(&3u16.to_le_bytes());
        let length = u32::from_le_bytes(unknown[..4].try_into().unwrap()) as usize;
        let crc = checksum(&unknown[4..4 + length]);
        unknown[4 + length..8 + length].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            decode(&unknown).unwrap_err().message,
            "unsupported journal version"
        );
    }
    fn fixture_record() -> Record {
        Record {
            operation_id: [7; 16],
            phase: JournalPhase::Staging,
            destination_leaf: OsString::from_vec(b"native-\xff".to_vec()),
            payload_identity: None,
            root_metadata: Some(RootMetadata {
                mode: 0o555,
                accessed: (-10, 999_999_999),
                modified: (50, 123_456_789),
            }),
        }
    }
    #[test]
    fn finalized_root_journal_roundtrips_each_crash_prefix_and_rejects_skipped_finalization() {
        let mut record = fixture_record();
        let mut bytes = Vec::new();
        for (index, phase) in [
            JournalPhase::Staging,
            JournalPhase::Verified,
            JournalPhase::CommitIntent,
            JournalPhase::ContentPublished,
            JournalPhase::MetadataFinalized,
            JournalPhase::Committed,
        ]
        .into_iter()
        .enumerate()
        {
            record.phase = phase;
            if index > 0 {
                record.payload_identity = Some(NativeIdentity {
                    device: 5,
                    inode: 9,
                });
            }
            bytes.extend(encode(&record).unwrap());
            let log = decode(&bytes).unwrap();
            assert_eq!(log.records.len(), index + 1);
            assert_eq!(log.records.last().unwrap(), &record);
            assert!(!log.torn_tail);
        }
        let log = decode(&bytes[..bytes.len() - 1]).unwrap();
        assert!(log.torn_tail);
        assert_eq!(
            log.records.last().unwrap().phase,
            JournalPhase::MetadataFinalized
        );
        let mut invalid = fixture_record();
        let mut bytes = encode(&invalid).unwrap();
        invalid.payload_identity = Some(NativeIdentity {
            device: 5,
            inode: 9,
        });
        for phase in [
            JournalPhase::Verified,
            JournalPhase::CommitIntent,
            JournalPhase::Committed,
        ] {
            invalid.phase = phase;
            bytes.extend(encode(&invalid).unwrap());
        }
        assert_eq!(
            decode(&bytes).unwrap_err().message,
            "inconsistent journal sequence"
        );
    }
    #[test]
    fn version1_breadcrumbs_remain_decodable_without_inventing_finalization_metadata() {
        let mut record = fixture_record();
        record.root_metadata = None;
        let mut bytes = Vec::new();
        for (index, phase) in [
            JournalPhase::Staging,
            JournalPhase::Verified,
            JournalPhase::CommitIntent,
            JournalPhase::Committed,
        ]
        .into_iter()
        .enumerate()
        {
            record.phase = phase;
            if index > 0 {
                record.payload_identity = Some(NativeIdentity {
                    device: 1,
                    inode: 2,
                });
            }
            let v2 = encode(&record).unwrap();
            let mut body = v2[4..v2.len() - 4].to_vec();
            body.drain(48..77);
            body[8..10].copy_from_slice(&1u16.to_le_bytes());
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&body);
            bytes.extend_from_slice(&checksum(&body).to_le_bytes());
        }
        let log = decode(&bytes).unwrap();
        assert_eq!(log.records.len(), 4);
        assert_eq!(log.records.last().unwrap(), &record);
        assert!(log.records.iter().all(|r| r.root_metadata.is_none()));
    }
    #[test]
    fn desired_metadata_is_checked_and_log_budget_contains_six_maximum_native_name_records() {
        let mut record = fixture_record();
        record.destination_leaf = OsString::from_vec(vec![0xff; 4096]);
        let mut bytes = Vec::new();
        for (index, phase) in [
            JournalPhase::Staging,
            JournalPhase::Verified,
            JournalPhase::CommitIntent,
            JournalPhase::ContentPublished,
            JournalPhase::MetadataFinalized,
            JournalPhase::Committed,
        ]
        .into_iter()
        .enumerate()
        {
            record.phase = phase;
            if index > 0 {
                record.payload_identity = Some(NativeIdentity {
                    device: 1,
                    inode: 2,
                });
            }
            bytes.extend(encode(&record).unwrap());
        }
        assert!(bytes.len() <= MAX_LOG_BYTES);
        assert_eq!(decode(&bytes).unwrap().records.len(), 6);
        record.root_metadata.as_mut().unwrap().mode = 0o4555;
        assert!(encode(&record).is_err());
        record.root_metadata.as_mut().unwrap().mode = 0o555;
        record.root_metadata.as_mut().unwrap().accessed.1 = 1_000_000_000;
        assert!(encode(&record).is_err());
    }
}
