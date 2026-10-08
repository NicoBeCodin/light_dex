//! Session identities and canonical, domain-separated Ed25519 signatures.
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::{TryRngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::{
    blockchain::{Block, Transaction},
    consensus::{
        CommitCertificate, ConsensusMessage,
        hotstuff::{HotStuffMessage, Phase, QuorumCertificate},
        pbft::PbftMessage,
    },
    exchange::{ExchangeState, address::Address},
    message::{hex_decode, hex_encode},
};

pub const PROTOCOL_VERSION: u8 = 2;
const TX_DOMAIN: &[u8] = b"light-dex/transaction-signature/v2";
const CONSENSUS_DOMAIN: &[u8] = b"light-dex/consensus-signature/v2";
const PEER_DOMAIN: &[u8] = b"light-dex/peer-handshake/v2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    pub version: u8,
    pub session_nonce: String,
    pub consensus: String,
    pub initial_state_root: String,
    pub validators: BTreeMap<u64, String>,
    pub accounts: BTreeSet<Address>,
    pub chain_id: String,
}

impl SessionManifest {
    pub fn chain_id_bytes(&self) -> io::Result<[u8; 32]> {
        hex_decode(&self.chain_id)
    }

    fn calculated_id(&self) -> io::Result<[u8; 32]> {
        let mut hash = Sha256::new();
        hash.update(b"light-dex/session-manifest/v2");
        let mut encoded = Vec::new();
        encode_value(
            &serde_json::to_value((
                self.version,
                &self.session_nonce,
                &self.consensus,
                &self.initial_state_root,
                &self.validators,
                &self.accounts,
            ))
            .map_err(io::Error::other)?,
            &mut encoded,
        )?;
        hash.update(&encoded);
        Ok(hash.finalize().into())
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.version != PROTOCOL_VERSION
            || self.consensus != "pbft" && self.consensus != "hotstuff"
            || self.validators.is_empty()
            || self.accounts.is_empty()
        {
            return Err(invalid("invalid session manifest"));
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
        if !validator { return Err(invalid("account signers are identified by address")); }
        let key = self.validators.get(&id).ok_or_else(|| invalid("unknown signer"))?;
        VerifyingKey::from_bytes(&hex_decode(key)?).map_err(io::Error::other)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedTransaction {
    pub transaction: Transaction,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedConsensusMessage {
    pub signer: u64,
    pub message: ConsensusMessage,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerHandshake {
    pub stage: String,
    pub node_id: u64,
    pub nonce: String,
    pub echo: String,
    pub signature: String,
}

pub fn random_nonce() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    OsRng.try_fill_bytes(&mut bytes).map_err(io::Error::other)?;
    Ok(hex_encode(&bytes))
}

pub fn sign_handshake(
    stage: &str,
    node_id: u64,
    nonce: &str,
    echo: &str,
    manifest: &SessionManifest,
    key: &SigningKey,
) -> io::Result<PeerHandshake> {
    if key.verifying_key() != manifest.verifier(true, node_id)? {
        return Err(invalid("wrong peer key"));
    }
    let _: [u8; 32] = hex_decode(nonce)?;
    if !echo.is_empty() {
        let _: [u8; 32] = hex_decode(echo)?;
    }
    let bytes = signing_bytes(
        PEER_DOMAIN,
        manifest.chain_id_bytes()?,
        &(stage, node_id, nonce, echo),
    )?;
    Ok(PeerHandshake {
        stage: stage.into(),
        node_id,
        nonce: nonce.into(),
        echo: echo.into(),
        signature: hex_encode(&key.sign(&bytes).to_bytes()),
    })
}

pub fn verify_handshake(
    handshake: &PeerHandshake,
    stage: &str,
    manifest: &SessionManifest,
) -> io::Result<()> {
    if handshake.stage != stage {
        return Err(invalid("wrong peer handshake stage"));
    }
    let _: [u8; 32] = hex_decode(&handshake.nonce)?;
    if !handshake.echo.is_empty() {
        let _: [u8; 32] = hex_decode(&handshake.echo)?;
    }
    let key = manifest.verifier(true, handshake.node_id)?;
    let bytes = signing_bytes(
        PEER_DOMAIN,
        manifest.chain_id_bytes()?,
        &(
            handshake.stage.as_str(),
            handshake.node_id,
            handshake.nonce.as_str(),
            handshake.echo.as_str(),
        ),
    )?;
    verify(&key, &bytes, &handshake.signature)
}

pub fn new_key() -> io::Result<SigningKey> {
    let mut secret = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut secret)
        .map_err(io::Error::other)?;
    Ok(SigningKey::from_bytes(&secret))
}

pub fn write_secret(path: &Path, key: &SigningKey) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(hex_encode(&key.to_bytes()).as_bytes())?;
    file.sync_all()
}

pub fn read_secret(path: &Path) -> io::Result<SigningKey> {
    let text = fs::read_to_string(path)?;
    Ok(SigningKey::from_bytes(&hex_decode(text.trim())?))
}

pub fn create_session(
    path: &Path,
    consensus: &str,
    nodes: u64,
) -> io::Result<SessionManifest> {
    if nodes == 0 || nodes > 1_000 {
        return Err(invalid("invalid validator count"));
    }
    fs::create_dir_all(path)?;
    let keys = path.join("keys");
    fs::create_dir(&keys)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&keys, fs::Permissions::from_mode(0o700))?;
    }
    let mut nonce = [0u8; 32];
    OsRng.try_fill_bytes(&mut nonce).map_err(io::Error::other)?;
    let mut manifest = SessionManifest {
        version: PROTOCOL_VERSION,
        session_nonce: hex_encode(&nonce),
        consensus: consensus.into(),
        initial_state_root: String::new(),
        validators: BTreeMap::new(),
        accounts: BTreeSet::new(),
        chain_id: String::new(),
    };
    for id in 0..nodes {
        let key = new_key()?;
        manifest
            .validators
            .insert(id, hex_encode(&key.verifying_key().to_bytes()));
        write_secret(&keys.join(format!("validator-{id}.key")), &key)?;
    }
    let mut wallet = BTreeMap::new();
    for id in 0..100u64 {
        let key = new_key()?;
        let address = Address::from_public_key(&key.verifying_key());
        manifest.accounts.insert(address);
        wallet.insert(id, address);
        write_secret(&keys.join(format!("account-{id}.key")), &key)?;
    }
    manifest.initial_state_root = hex_encode(&ExchangeState::with_funded_addresses(manifest.accounts.iter().copied()).state_digest());
    let mut wallet_file = OpenOptions::new().write(true).create_new(true).open(path.join("wallet.json"))?;
    wallet_file.write_all(&serde_json::to_vec_pretty(&wallet).map_err(io::Error::other)?)?;
    wallet_file.sync_all()?;
    manifest.chain_id = hex_encode(&manifest.calculated_id()?);
    manifest.validate()?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join("manifest.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        fs::File::open(&keys)?.sync_all()?;
        fs::File::open(path)?.sync_all()?;
    }
    Ok(manifest)
}

pub fn sign_transaction(
    tx: Transaction,
    manifest: &SessionManifest,
    key: &SigningKey,
) -> io::Result<SignedTransaction> {
    if key.verifying_key() != tx.account_id().public_key().map_err(|error| invalid(&error))? {
        return Err(invalid("wrong account key"));
    }
    let bytes = signing_bytes(TX_DOMAIN, manifest.chain_id_bytes()?, &tx)?;
    Ok(SignedTransaction {
        transaction: tx,
        signature: hex_encode(&key.sign(&bytes).to_bytes()),
    })
}

pub fn verify_transaction(tx: &SignedTransaction, manifest: &SessionManifest) -> io::Result<()> {
    let key = tx.transaction.account_id().public_key().map_err(|error| invalid(&error))?;
    let bytes = signing_bytes(TX_DOMAIN, manifest.chain_id_bytes()?, &tx.transaction)?;
    verify(&key, &bytes, &tx.signature)
}

pub fn sign_consensus(
    message: ConsensusMessage,
    signer: u64,
    manifest: &SessionManifest,
    key: &SigningKey,
) -> io::Result<SignedConsensusMessage> {
    if key.verifying_key() != manifest.verifier(true, signer)? {
        return Err(invalid("wrong validator key"));
    }
    let bytes = signing_bytes(
        CONSENSUS_DOMAIN,
        manifest.chain_id_bytes()?,
        &(manifest.consensus.as_str(), &message),
    )?;
    Ok(SignedConsensusMessage {
        signer,
        message,
        signature: hex_encode(&key.sign(&bytes).to_bytes()),
    })
}

pub fn verify_consensus(
    signed: &SignedConsensusMessage,
    manifest: &SessionManifest,
) -> io::Result<()> {
    let matches = matches!(
        (&signed.message, manifest.consensus.as_str()),
        (ConsensusMessage::Pbft(_), "pbft") | (ConsensusMessage::Hotstuff(_), "hotstuff")
    );
    if !matches {
        return Err(invalid("wrong consensus algorithm"));
    }
    let key = manifest.verifier(true, signed.signer)?;
    let bytes = signing_bytes(
        CONSENSUS_DOMAIN,
        manifest.chain_id_bytes()?,
        &(manifest.consensus.as_str(), &signed.message),
    )?;
    verify(&key, &bytes, &signed.signature)?;
    if let ConsensusMessage::Hotstuff(message) = &signed.message {
        let qc = match message {
            HotStuffMessage::Prepare { justify, .. } => Some(justify),
            HotStuffMessage::PreCommit { qc, .. }
            | HotStuffMessage::Commit { qc, .. }
            | HotStuffMessage::Decide { qc, .. } => Some(qc),
            HotStuffMessage::NewView { high_qc, .. }
            | HotStuffMessage::StartView { high_qc, .. } => Some(high_qc),
            HotStuffMessage::Vote { .. } => None,
        };
        if let Some(qc) = qc {
            if *qc != QuorumCertificate::genesis() {
                verify_hotstuff_qc(qc, manifest)?;
            }
        }
        if let HotStuffMessage::StartView {
            view,
            high_qc,
            proofs,
        } = message
        {
            verify_hotstuff_start_view(*view, high_qc, proofs, manifest)?;
        }
    }
    if let ConsensusMessage::Pbft(PbftMessage::NewView {
        view,
        sequence,
        proofs,
    }) = &signed.message
    {
        verify_pbft_new_view(*view, *sequence, proofs, manifest)?;
    }
    Ok(())
}

pub fn verify_pbft_new_view(
    view: u64,
    sequence: u64,
    proofs: &[SignedConsensusMessage],
    manifest: &SessionManifest,
) -> io::Result<()> {
    let n = manifest.validators.len() as u64;
    let quorum = crate::consensus::quorum_size(n);
    if proofs.len() < quorum as usize
        || proofs.len() > n as usize
        || proofs
            .windows(2)
            .any(|pair| pair[0].signer >= pair[1].signer)
    {
        return Err(invalid("invalid PBFT view-change quorum"));
    }
    for proof in proofs {
        if !matches!(&proof.message, ConsensusMessage::Pbft(PbftMessage::ViewChange {
            new_view, sequence: requested_sequence,
        }) if *new_view == view && *requested_sequence == sequence)
        {
            return Err(invalid("unrelated PBFT view-change proof"));
        }
        verify_consensus(proof, manifest)?;
    }
    Ok(())
}

pub fn verify_hotstuff_qc(qc: &QuorumCertificate, manifest: &SessionManifest) -> io::Result<()> {
    if *qc == QuorumCertificate::genesis() {
        return Ok(());
    }
    let n = manifest.validators.len() as u64;
    if qc.voters.len() != qc.signatures.len()
        || (qc.voters.len() as u64) < crate::consensus::quorum_size(n)
        || qc.voters.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(invalid("invalid HotStuff certificate quorum"));
    }
    for (&signer, signature) in qc.voters.iter().zip(&qc.signatures) {
        let message = ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
            view: qc.view,
            height: qc.height,
            phase: qc.phase,
            digest: qc.digest,
        });
        let key = manifest.verifier(true, signer)?;
        let bytes = signing_bytes(
            CONSENSUS_DOMAIN,
            manifest.chain_id_bytes()?,
            &(manifest.consensus.as_str(), &message),
        )?;
        verify(&key, &bytes, signature)?;
    }
    Ok(())
}

pub fn verify_hotstuff_start_view(
    view: u64,
    high_qc: &QuorumCertificate,
    proofs: &[SignedConsensusMessage],
    manifest: &SessionManifest,
) -> io::Result<()> {
    let n = manifest.validators.len() as u64;
    if proofs.len() < crate::consensus::quorum_size(n) as usize
        || proofs.len() > n as usize
        || proofs
            .windows(2)
            .any(|pair| pair[0].signer >= pair[1].signer)
    {
        return Err(invalid("invalid HotStuff new-view quorum"));
    }
    let mut best: Option<&QuorumCertificate> = None;
    for proof in proofs {
        let ConsensusMessage::Hotstuff(HotStuffMessage::NewView {
            view: requested,
            high_qc: candidate,
        }) = &proof.message
        else {
            return Err(invalid("unrelated HotStuff new-view proof"));
        };
        if *requested != view {
            return Err(invalid("wrong HotStuff view"));
        }
        verify_consensus(proof, manifest)?;
        let rank = |qc: &QuorumCertificate| {
            (
                qc.height,
                qc.view,
                match qc.phase {
                    crate::consensus::hotstuff::Phase::Prepare => 0,
                    crate::consensus::hotstuff::Phase::PreCommit => 1,
                    crate::consensus::hotstuff::Phase::Commit => 2,
                },
                qc.digest,
            )
        };
        if best.is_none_or(|current| rank(candidate) > rank(current)) {
            best = Some(candidate);
        }
    }
    if best != Some(high_qc) {
        return Err(invalid("new-view high QC is not justified"));
    }
    Ok(())
}

pub fn verify_commit_certificate(
    block: &Block,
    certificate: &CommitCertificate,
    manifest: &SessionManifest,
) -> io::Result<()> {
    let n = manifest.validators.len() as u64;
    let quorum = crate::consensus::quorum_size(n);
    match (manifest.consensus.as_str(), certificate) {
        (
            "pbft",
            CommitCertificate::Pbft {
                view,
                sequence,
                digest,
                proofs,
            },
        ) => {
            if *sequence != block.sequence
                || *digest != block.to_digest()
                || proofs.len() < quorum as usize
                || proofs.len() > n as usize
                || proofs
                    .windows(2)
                    .any(|pair| pair[0].signer >= pair[1].signer)
            {
                return Err(invalid("invalid PBFT commit certificate"));
            }
            for proof in proofs {
                if !matches!(&proof.message, ConsensusMessage::Pbft(PbftMessage::Commit {
                    view: vote_view, sequence: vote_sequence, digest: vote_digest,
                }) if vote_view == view && vote_sequence == sequence && vote_digest == digest)
                {
                    return Err(invalid("unrelated PBFT commit vote"));
                }
                verify_consensus(proof, manifest)?;
            }
        }
        ("hotstuff", CommitCertificate::Hotstuff { qc }) => {
            if qc.phase != Phase::Commit
                || qc.height != block.sequence
                || qc.digest != block.to_digest()
                || *qc == QuorumCertificate::genesis()
            {
                return Err(invalid("invalid HotStuff commit certificate"));
            }
            verify_hotstuff_qc(qc, manifest)?;
        }
        _ => return Err(invalid("commit certificate algorithm mismatch")),
    }
    Ok(())
}

fn signing_bytes<T: Serialize>(
    domain: &[u8],
    chain_id: [u8; 32],
    value: &T,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    bytes.extend_from_slice(domain);
    bytes.extend_from_slice(&chain_id);
    encode_value(
        &serde_json::to_value(value).map_err(io::Error::other)?,
        &mut bytes,
    )?;
    Ok(bytes)
}

fn encode_value<W: Write>(value: &Value, out: &mut W) -> io::Result<()> {
    match value {
        Value::Null => out.write_all(&[0]),
        Value::Bool(value) => out.write_all(&[1, u8::from(*value)]),
        Value::Number(value) => {
            let number = value
                .as_u64()
                .ok_or_else(|| invalid("only unsigned integers are signable"))?;
            out.write_all(&[2])?;
            out.write_all(&number.to_be_bytes())
        }
        Value::String(value) => {
            out.write_all(&[3])?;
            length_bytes(out, value.as_bytes())
        }
        Value::Array(values) => {
            out.write_all(&[4])?;
            out.write_all(&(values.len() as u64).to_be_bytes())?;
            for value in values {
                encode_value(value, out)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            out.write_all(&[5])?;
            out.write_all(&(values.len() as u64).to_be_bytes())?;
            let mut sorted = values.iter().collect::<Vec<_>>();
            sorted.sort_by_key(|(key, _)| *key);
            for (key, value) in sorted {
                length_bytes(out, key.as_bytes())?;
                encode_value(value, out)?;
            }
            Ok(())
        }
    }
}

fn length_bytes<W: Write>(out: &mut W, bytes: &[u8]) -> io::Result<()> {
    out.write_all(&(bytes.len() as u64).to_be_bytes())?;
    out.write_all(bytes)
}

fn verify(key: &VerifyingKey, bytes: &[u8], hex: &str) -> io::Result<()> {
    let decoded = hex_decode::<64>(hex)?;
    if hex != hex_encode(&decoded) {
        return Err(invalid("noncanonical signature encoding"));
    }
    let signature = Signature::from_bytes(&decoded);
    key.verify_strict(bytes, &signature)
        .map_err(|_| invalid("invalid signature"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blockchain::{BlockchainState, CommittedBlock},
        consensus::{CommitCertificate, hotstuff::Phase},
    };
    use std::sync::Arc;

    fn session() -> (std::path::PathBuf, SessionManifest) {
        let path = std::env::temp_dir().join(format!(
            "light-dex-signature-test-{}",
            rand::random::<u64>()
        ));
        let manifest =
            create_session(&path, "pbft", 4).unwrap();
        (path, manifest)
    }

    fn pbft_certificate(
        path: &Path,
        manifest: &SessionManifest,
        block: &Block,
    ) -> CommitCertificate {
        let proofs = (0..3)
            .map(|id| {
                sign_consensus(
                    ConsensusMessage::Pbft(PbftMessage::Commit {
                        view: 0,
                        sequence: block.sequence,
                        digest: block.to_digest(),
                    }),
                    id,
                    manifest,
                    &read_secret(&path.join("keys").join(format!("validator-{id}.key"))).unwrap(),
                )
                .unwrap()
            })
            .collect();
        CommitCertificate::Pbft {
            view: 0,
            sequence: block.sequence,
            digest: block.to_digest(),
            proofs,
        }
    }

    #[test]
    fn canonical_encoding_has_stable_sorted_field_bytes() {
        let value = serde_json::json!({"b": true, "a": 1});
        let mut actual = Vec::new();
        encode_value(&value, &mut actual).unwrap();
        let mut expected = vec![5];
        expected.extend_from_slice(&2u64.to_be_bytes());
        expected.extend_from_slice(&1u64.to_be_bytes());
        expected.push(b'a');
        expected.push(2);
        expected.extend_from_slice(&1u64.to_be_bytes());
        expected.extend_from_slice(&1u64.to_be_bytes());
        expected.push(b'b');
        expected.extend_from_slice(&[1, 1]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn signed_transaction_and_certified_archive_reject_tampering() {
        let (path, manifest) = session();
        let manifest = Arc::new(manifest);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path.join("keys/account-0.key"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.join("keys"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let account = read_secret(&path.join("keys/account-0.key")).unwrap();
        let recipient = read_secret(&path.join("keys/account-1.key")).unwrap();
        let sender_address = Address::from_public_key(&account.verifying_key());
        let tx = Transaction::Transfer {
            from: sender_address,
            to: Address::from_public_key(&recipient.verifying_key()),
            asset: 0,
            amount: 3,
            nonce: 0,
        };
        let signed = sign_transaction(tx, &manifest, &account).unwrap();
        assert!(verify_transaction(&signed, &manifest).is_ok());
        let mut changed = signed.clone();
        if let Transaction::Transfer { amount, .. } = &mut changed.transaction {
            *amount += 1;
        }
        assert!(verify_transaction(&changed, &manifest).is_err());
        let mut other_chain = (*manifest).clone();
        other_chain.chain_id = hex_encode(&[42; 32]);
        assert!(verify_transaction(&signed, &other_chain).is_err());
        let file = path.join("node-0.json");
        let mut state = BlockchainState::load_signed(file.clone(), manifest.clone()).unwrap();
        assert!(!state.add_signed_transaction(&changed));
        let genesis = Block::new_initial_block_signed(&manifest.chain_id, hex_decode(&manifest.initial_state_root).unwrap());
        let proof = pbft_certificate(&path, &manifest, &genesis);
        state
            .commit_block_with_certificate(&genesis, proof)
            .unwrap();
        assert!(state.add_signed_transaction(&signed));
        let block = state.build_block().unwrap();
        let mut fake = pbft_certificate(&path, &manifest, &block);
        if let CommitCertificate::Pbft { proofs, .. } = &mut fake {
            let first = &proofs[0].signature[..2];
            let changed = if first == "00" { "01" } else { "00" };
            proofs[0].signature.replace_range(..2, changed);
        }
        assert!(state.commit_block_with_certificate(&block, fake).is_err());
        assert_eq!(state.blocks.len(), 1);
        let proof = pbft_certificate(&path, &manifest, &block);
        state.commit_block_with_certificate(&block, proof).unwrap();
        let replayed = BlockchainState::load_signed(file.clone(), manifest.clone()).unwrap();
        assert_eq!(replayed.blocks.len(), 2);
        assert_eq!(replayed.exchange_state.get_nonce_for_account(sender_address).unwrap(), 1);
        let journal = BlockchainState::journal_path(&file);
        let mut records = fs::read_to_string(&journal)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<CommittedBlock>(line).unwrap())
            .collect::<Vec<_>>();
        if let CommitCertificate::Pbft { proofs, .. } = &mut records[1].certificate {
            let changed = if &proofs[0].signature[..2] == "ff" {
                "fe"
            } else {
                "ff"
            };
            proofs[0].signature.replace_range(..2, changed);
        }
        let corrupt = records
            .iter()
            .map(|record| format!("{}\n", serde_json::to_string(record).unwrap()))
            .collect::<String>();
        fs::write(journal, corrupt).unwrap();
        assert!(BlockchainState::load_signed(file, manifest).is_err());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn hotstuff_qc_requires_distinct_valid_phase_votes() {
        let (path, mut manifest) = session();
        manifest.consensus = "hotstuff".into();
        manifest.chain_id = hex_encode(&manifest.calculated_id().unwrap());
        let digest = [9u8; 32];
        let mut qc = QuorumCertificate {
            view: 2,
            height: 3,
            phase: Phase::Commit,
            digest,
            voters: vec![0, 1, 2],
            signatures: Vec::new(),
        };
        for id in &qc.voters {
            let key = read_secret(&path.join("keys").join(format!("validator-{id}.key"))).unwrap();
            qc.signatures.push(
                sign_consensus(
                    ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
                        view: qc.view,
                        height: qc.height,
                        phase: qc.phase,
                        digest,
                    }),
                    *id,
                    &manifest,
                    &key,
                )
                .unwrap()
                .signature,
            );
        }
        assert!(verify_hotstuff_qc(&qc, &manifest).is_ok());
        qc.phase = Phase::Prepare;
        assert!(verify_hotstuff_qc(&qc, &manifest).is_err());
        qc.phase = Phase::Commit;
        qc.voters[1] = 0;
        assert!(verify_hotstuff_qc(&qc, &manifest).is_err());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn view_change_proofs_bind_view_and_distinct_validators() {
        let (path, manifest) = session();
        let proofs = (0..3)
            .map(|id| {
                let key =
                    read_secret(&path.join("keys").join(format!("validator-{id}.key"))).unwrap();
                sign_consensus(
                    ConsensusMessage::Pbft(PbftMessage::ViewChange {
                        new_view: 2,
                        sequence: 5,
                    }),
                    id,
                    &manifest,
                    &key,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(verify_pbft_new_view(2, 5, &proofs, &manifest).is_ok());
        assert!(verify_pbft_new_view(3, 5, &proofs, &manifest).is_err());
        let mut duplicate = proofs.clone();
        duplicate[1] = duplicate[0].clone();
        assert!(verify_pbft_new_view(2, 5, &duplicate, &manifest).is_err());
        let mut forged = proofs.clone();
        forged[2].signer = 3;
        assert!(verify_pbft_new_view(2, 5, &forged, &manifest).is_err());

        let mut hotstuff = manifest.clone();
        hotstuff.consensus = "hotstuff".into();
        hotstuff.chain_id = hex_encode(&hotstuff.calculated_id().unwrap());
        let genesis = QuorumCertificate::genesis();
        let new_views = (0..3)
            .map(|id| {
                let key =
                    read_secret(&path.join("keys").join(format!("validator-{id}.key"))).unwrap();
                sign_consensus(
                    ConsensusMessage::Hotstuff(HotStuffMessage::NewView {
                        view: 2,
                        high_qc: genesis.clone(),
                    }),
                    id,
                    &hotstuff,
                    &key,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(verify_hotstuff_start_view(2, &genesis, &new_views, &hotstuff).is_ok());
        assert!(verify_hotstuff_start_view(2, &genesis, &new_views[..2], &hotstuff).is_err());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn signed_peer_hello_cannot_claim_another_validator_id() {
        let (path, manifest) = session();
        let key = read_secret(&path.join("keys/validator-0.key")).unwrap();
        let nonce = random_nonce().unwrap();
        let hello = sign_handshake("hello", 0, &nonce, "", &manifest, &key).unwrap();
        assert!(verify_handshake(&hello, "hello", &manifest).is_ok());
        let mut forged = hello.clone();
        forged.node_id = 1;
        assert!(verify_handshake(&forged, "hello", &manifest).is_err());
        forged = hello;
        forged.nonce = random_nonce().unwrap();
        assert!(verify_handshake(&forged, "hello", &manifest).is_err());
        fs::remove_dir_all(path).unwrap();
    }
}
