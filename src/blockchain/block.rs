use super::*;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct Block {
    pub sequence: u64,
    pub previous_hash: Digest,
    pub transactions: Vec<Transaction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub oracle_updates: Vec<OracleUpdate>,
    pub state_root: Digest,
    /// Missing in pre-version journals. Version 0 retains their historical
    /// block hash and accepts either historical state-root format.
    #[serde(default)]
    pub state_root_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<BlockAuthentication>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommittedBlock {
    pub block: Block,
    pub certificate: CommitCertificate,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct BlockAuthentication {
    pub version: u8,
    pub chain_id: String,
    pub signatures: Vec<String>,
}

impl Default for Block {
    fn default() -> Self {
        Self::new()
    }
}

impl Block {
    pub fn new() -> Block {
        Block {
            transactions: Vec::new(),
            oracle_updates: Vec::new(),
            state_root: ExchangeState::default().state_digest(),
            state_root_version: MERKLE_STATE_ROOT_VERSION,
            authentication: None,
            sequence: 0,
            previous_hash: Digest::default(),
        }
    }
    pub fn new_initial_block() -> Block {
        Block::new()
    }

    pub fn new_initial_block_signed(chain_id: &str, initial_root: Digest) -> Block {
        let mut block = Self::new();
        block.state_root = initial_root;
        block.state_root_version = ADDRESS_STATE_ROOT_VERSION;
        block.authentication = Some(BlockAuthentication {
            version: PROTOCOL_VERSION,
            chain_id: chain_id.into(),
            signatures: Vec::new(),
        });
        block
    }

    pub fn to_digest(&self) -> Digest {
        super::digest_cache::block(self)
    }
    pub(super) fn digest_uncached(&self) -> Digest {
        let mut hasher = Sha256::new();
        if let Some(auth) = &self.authentication {
            hasher.update(b"light-dex/block/v8");
            hasher.update([auth.version, self.state_root_version]);
            hasher.update(auth.chain_id.as_bytes());
            hasher.update(self.sequence.to_be_bytes());
            hasher.update(self.previous_hash);
            hasher.update(self.state_root);
            hasher.update((self.transactions.len() as u64).to_be_bytes());
            for (tx, signature) in self.transactions.iter().zip(&auth.signatures) {
                hasher.update(tx.to_digest());
                hasher.update(signature.as_bytes());
            }
            hasher.update((self.oracle_updates.len() as u64).to_be_bytes());
            for update in &self.oracle_updates {
                hasher.update(update.digest());
            }
            return hasher.finalize().into();
        }
        if self.state_root_version != LEGACY_STATE_ROOT_VERSION {
            hasher.update(b"light-dex/block/v1");
            hasher.update([self.state_root_version]);
        }
        hasher.update(&self.sequence.to_le_bytes());
        hasher.update(&self.previous_hash);
        hasher.update(&self.state_root);
        for tx in &self.transactions {
            hasher.update(tx.to_digest());
        }
        if !self.oracle_updates.is_empty() {
            hasher.update(b"light-dex/block/oracle/v1");
            hasher.update((self.oracle_updates.len() as u64).to_be_bytes());
            for update in &self.oracle_updates {
                hasher.update(update.digest());
            }
        }
        hasher.finalize().into()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Proposal {
    pub block: Block,
}

impl Proposal {
    pub fn to_digest(&self) -> Digest {
        self.block.to_digest()
    }

    pub fn get_block(&self) -> Block {
        self.block.clone()
    }
}
