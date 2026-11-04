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
use sha2::{Digest as _, Sha256};

use crate::{
    blockchain::{Block, Transaction},
    consensus::{
        CommitCertificate, ConsensusMessage,
        hotstuff::{HotStuffMessage, Phase, QuorumCertificate},
        pbft::PbftMessage,
    },
    exchange::{ExchangeState, FaucetConfig, address::Address},
    message::{hex_decode, hex_encode},
};

pub const PROTOCOL_VERSION: u8 = 18;
const TX_DOMAIN: &[u8] = light_dex_protocol_codec::TRANSACTION_DOMAIN;
const CONSENSUS_DOMAIN: &[u8] = b"light-dex/consensus-signature/v10";
const PEER_DOMAIN: &[u8] = b"light-dex/peer-handshake/v10";

mod cache;
mod encoding;
pub(crate) mod verification;
pub(crate) use cache::decoded_public_key;
mod manifest;
mod session;
mod signatures;

use encoding::{encode_value, invalid, signing_bytes, verify};
pub use manifest::{FaucetTerms, SessionManifest};
pub use session::{
    create_session, create_session_with_faucet, create_session_with_mode,
    create_session_with_options, new_key, read_secret, write_secret,
};
pub use signatures::{
    PeerHandshake, SignedConsensusMessage, SignedTransaction, random_nonce, sign_consensus,
    sign_handshake, sign_transaction, verify_chained_qc, verify_chained_start_view,
    verify_commit_certificate, verify_consensus, verify_fast_qc, verify_fast_start_view,
    verify_handshake, verify_hotstuff_qc, verify_hotstuff_start_view, verify_pbft_new_view,
    verify_transaction,
};

#[cfg(test)]
mod tests;
