//! Durable consensus evidence storage. This primitive does not enable restart:
//! callers must restore locks/votes and reconcile decided but uncommitted blocks.
//! A record must be synchronized before its dependent messages leave the node.
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

const MAGIC: &[u8; 8] = b"LDEXSJ01";
const MAX_RECORD: usize = 8 * 1024 * 1024;

pub struct SafetyJournal {
    file: File,
    previous: [u8; 32],
    poisoned: bool,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn checksum(previous: &[u8; 32], bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"light-dex/safety-record/v1");
    hash.update(previous);
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
    hash.finalize().into()
}

impl SafetyJournal {
    /// Bind storage to a chain and validator. Hold the file lock for its lifetime.
    /// Incomplete final records are discarded: append never authorizes sending
    /// until the entire record is synchronized. Complete corrupt records fail.
    pub fn open<T: DeserializeOwned>(
        path: &Path,
        chain_id: [u8; 32],
        validator_id: u64,
    ) -> io::Result<(Self, Vec<T>)> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.try_lock().map_err(io::Error::other)?;
        let mut header = MAGIC.to_vec();
        header.extend_from_slice(&chain_id);
        header.extend_from_slice(&validator_id.to_be_bytes());
        if file.metadata()?.len() == 0 {
            file.write_all(&header)?;
            file.sync_all()?;
            File::open(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?
            .sync_all()?;
        } else {
            let mut actual = vec![0; header.len()];
            file.read_exact(&mut actual)?;
            if actual != header {
                return Err(invalid("safety journal identity/version mismatch"));
            }
        }
        let mut previous = checksum(&[0; 32], &header);
        let mut records = Vec::new();
        loop {
            let boundary = file.stream_position()?;
            let remaining = file.metadata()?.len().saturating_sub(boundary);
            if remaining == 0 {
                break;
            }
            if remaining < 4 {
                file.set_len(boundary)?;
                file.sync_all()?;
                break;
            }
            let mut length = [0; 4];
            file.read_exact(&mut length)?;
            let length = u32::from_be_bytes(length) as usize;
            if length == 0 || length > MAX_RECORD {
                return Err(invalid("invalid safety journal record size"));
            }
            if remaining < 4 + length as u64 + 32 {
                file.set_len(boundary)?;
                file.sync_all()?;
                break;
            }
            let mut bytes = vec![0; length];
            file.read_exact(&mut bytes)?;
            let mut stored = [0; 32];
            file.read_exact(&mut stored)?;
            let expected = checksum(&previous, &bytes);
            if stored != expected {
                return Err(invalid("safety journal checksum mismatch"));
            }
            records.push(serde_json::from_slice(&bytes).map_err(io::Error::other)?);
            previous = expected;
        }
        file.seek(SeekFrom::End(0))?;
        Ok((
            Self {
                file,
                previous,
                poisoned: false,
            },
            records,
        ))
    }

    pub fn append<T: Serialize>(&mut self, record: &T) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("safety journal requires recovery"));
        }
        let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
        if bytes.is_empty() || bytes.len() > MAX_RECORD {
            return Err(invalid("safety journal record exceeds limit"));
        }
        let next = checksum(&self.previous, &bytes);
        self.poisoned = true;
        self.file.write_all(&(bytes.len() as u32).to_be_bytes())?;
        self.file.write_all(&bytes)?;
        self.file.write_all(&next)?;
        self.file.sync_all()?;
        self.previous = next;
        self.poisoned = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ldex-safety-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path.join("safety.wal"))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }
    #[test]
    fn recovery_identity_and_exclusive_lock() {
        let f = Fixture::new();
        let (mut journal, records) = SafetyJournal::open::<u64>(&f.0, [7; 32], 2).unwrap();
        assert!(records.is_empty());
        assert!(SafetyJournal::open::<u64>(&f.0, [7; 32], 2).is_err());
        journal.append(&42u64).unwrap();
        drop(journal);
        assert!(SafetyJournal::open::<u64>(&f.0, [8; 32], 2).is_err());
        assert!(SafetyJournal::open::<u64>(&f.0, [7; 32], 3).is_err());
        let (_, records) = SafetyJournal::open::<u64>(&f.0, [7; 32], 2).unwrap();
        assert_eq!(records, [42]);
    }
    #[test]
    fn every_incomplete_tail_recovers_only_complete_records() {
        let f = Fixture::new();
        let (mut journal, _) = SafetyJournal::open::<u64>(&f.0, [7; 32], 2).unwrap();
        journal.append(&11u64).unwrap();
        let boundary = journal.file.metadata().unwrap().len() as usize;
        journal.append(&22u64).unwrap();
        drop(journal);
        let full = std::fs::read(&f.0).unwrap();
        for length in boundary..full.len() {
            std::fs::write(&f.0, &full[..length]).unwrap();
            let (mut recovered, records) = SafetyJournal::open::<u64>(&f.0, [7; 32], 2).unwrap();
            assert_eq!(records, [11]);
            recovered.append(&33u64).unwrap();
            drop(recovered);
            let (_, records) = SafetyJournal::open::<u64>(&f.0, [7; 32], 2).unwrap();
            assert_eq!(records, [11, 33]);
        }
    }
    #[test]
    fn complete_corruption_fails_closed() {
        let f = Fixture::new();
        let (mut journal, _) = SafetyJournal::open::<u64>(&f.0, [7; 32], 2).unwrap();
        journal.append(&11u64).unwrap();
        drop(journal);
        let mut bytes = std::fs::read(&f.0).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&f.0, bytes).unwrap();
        assert!(SafetyJournal::open::<u64>(&f.0, [7; 32], 2).is_err());
    }
}

/// JSON cannot encode tuple map keys. Preserve their typed representation as pairs.
pub(super) mod map_entries {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::{collections::HashMap, hash::Hash};
    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        map: &HashMap<K, V>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        map.iter().collect::<Vec<_>>().serialize(s)
    }
    pub fn deserialize<
        'de,
        K: Deserialize<'de> + Eq + Hash,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    >(
        d: D,
    ) -> Result<HashMap<K, V>, D::Error> {
        Ok(Vec::<(K, V)>::deserialize(d)?.into_iter().collect())
    }
}

/// Atomic replacement bounds safety storage independently of chain lifetime.
/// The separate locked journal inode remains stable across snapshot replacement.
pub(super) struct SafetyStore {
    _lease: SafetyJournal,
    path: std::path::PathBuf,
    identity: [u8; 32],
}
impl SafetyStore {
    pub fn open(path: &Path, chain: [u8; 32], validator: u64) -> io::Result<Self> {
        let (lease, _) = SafetyJournal::open::<serde_json::Value>(
            &path.with_extension("lease"),
            chain,
            validator,
        )?;
        let identity = checksum(&chain, &validator.to_be_bytes());
        Ok(Self {
            _lease: lease,
            path: path.to_owned(),
            identity,
        })
    }
    pub fn read<T: DeserializeOwned>(&self) -> io::Result<Option<T>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if bytes.len() < 32 || bytes.len() > MAX_RECORD + 32 {
            return Err(invalid("invalid consensus snapshot size"));
        }
        let (digest, payload) = bytes.split_at(32);
        if digest != checksum(&self.identity, payload) {
            return Err(invalid("consensus snapshot checksum mismatch"));
        }
        serde_json::from_slice(payload)
            .map(Some)
            .map_err(io::Error::other)
    }
    pub fn write<T: Serialize>(&self, value: &T) -> io::Result<()> {
        let payload = serde_json::to_vec(value).map_err(io::Error::other)?;
        if payload.len() > MAX_RECORD {
            return Err(invalid("consensus snapshot exceeds size bound"));
        }
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let tmp = self.path.with_extension("pending");
        let mut file = options.open(&tmp)?;
        file.write_all(&checksum(&self.identity, &payload))?;
        file.write_all(&payload)?;
        file.sync_all()?;
        std::fs::rename(tmp, &self.path)?;
        File::open(
            self.path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()
    }
}
