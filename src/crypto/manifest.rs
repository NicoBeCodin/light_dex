use super::*;

#[derive(Debug, Clone, Copy)]
pub struct FaucetTerms {
    pub max_claims: u64,
    pub usd_per_claim: u64,
    pub btc_per_claim: u64,
}

impl Default for FaucetTerms {
    fn default() -> Self {
        Self {
            max_claims: 1_000,
            usd_per_claim: 1_000_000_000 * crate::exchange::market::USD_ATOMS,
            btc_per_claim: 10_000 * crate::exchange::market::BTC_ATOMS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    pub version: u8,
    pub session_nonce: String,
    pub consensus: String,
    /// Selects the chained protocol; included in the signed session identity.
    pub chained_hotstuff: bool,
    pub initial_state_root: String,
    /// None preserves the original full catalog. A selection is authenticated
    /// through its genesis market leaves in `initial_state_root`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_ids: Option<Vec<u64>>,
    pub validators: BTreeMap<u64, String>,
    pub accounts: BTreeSet<Address>,
    pub faucet: FaucetConfig,
    pub auto_view_change_blocks: Option<u64>,
    pub chain_id: String,
}

impl SessionManifest {
    pub fn initial_exchange(&self) -> Result<ExchangeState, String> {
        ExchangeState::with_claim_faucet_market_ids(self.faucet, self.market_ids.as_deref())
    }
    pub fn chain_id_bytes(&self) -> io::Result<[u8; 32]> {
        hex_decode(&self.chain_id)
    }

    pub(super) fn calculated_id(&self) -> io::Result<[u8; 32]> {
        let mut hash = Sha256::new();
        hash.update(b"light-dex/session-manifest/v8");
        let mut encoded = Vec::new();
        encode_value(
            &serde_json::to_value((
                self.version,
                &self.session_nonce,
                &self.consensus,
                self.chained_hotstuff,
                &self.initial_state_root,
                &self.validators,
                &self.accounts,
                &self.faucet,
                self.auto_view_change_blocks,
            ))
            .map_err(io::Error::other)?,
            &mut encoded,
        )?;
        hash.update(&encoded);
        Ok(hash.finalize().into())
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.version != PROTOCOL_VERSION
            || !matches!(
                self.consensus.as_str(),
                "pbft" | "hotstuff" | "hotstuff2" | "votor"
            )
            || self.consensus != "hotstuff" && self.chained_hotstuff
            || self.validators.is_empty()
        {
            return Err(invalid("invalid session manifest"));
        }
        self.faucet.validate().map_err(io::Error::other)?;
        if hex_encode(
            &self
                .initial_exchange()
                .map_err(io::Error::other)?
                .state_digest(),
        ) != self.initial_state_root
        {
            return Err(invalid(
                "session market selection differs from genesis root",
            ));
        }
        if self.auto_view_change_blocks == Some(0) {
            return Err(invalid("automatic view-change interval must be positive"));
        }
        let _: [u8; 32] = hex_decode(&self.session_nonce)?;
        let _: [u8; 32] = hex_decode(&self.initial_state_root)?;
        if self.session_nonce != self.session_nonce.to_ascii_lowercase()
            || self.initial_state_root != self.initial_state_root.to_ascii_lowercase()
            || self.chain_id != self.chain_id.to_ascii_lowercase()
        {
            return Err(invalid("noncanonical manifest hex encoding"));
        }
        if self.chain_id_bytes()? != self.calculated_id()? {
            return Err(invalid("session chain ID mismatch"));
        }
        let mut seen = HashSet::new();
        for (expected, (id, encoded)) in self.validators.iter().enumerate() {
            if *id != expected as u64 {
                return Err(invalid("validator IDs must be contiguous"));
            }
            let key = hex_decode::<32>(encoded)?;
            if *encoded != hex_encode(&key) {
                return Err(invalid("noncanonical validator key encoding"));
            }
            let parsed = VerifyingKey::from_bytes(&key).map_err(io::Error::other)?;
            if parsed.is_weak() || !seen.insert(key) {
                return Err(invalid("weak or duplicate public key"));
            }
        }
        for address in &self.accounts {
            let key = address.bytes();
            if address.public_key().is_err() || !seen.insert(key) {
                return Err(invalid("weak or duplicate public key"));
            }
        }
        Ok(())
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let manifest: Self = serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn verifier(&self, validator: bool, id: u64) -> io::Result<VerifyingKey> {
        if !validator {
            return Err(invalid("account signers are identified by address"));
        }
        let key = self
            .validators
            .get(&id)
            .ok_or_else(|| invalid("unknown signer"))?;
        super::cache::decoded_public_key(hex_decode(key)?)
    }
}
