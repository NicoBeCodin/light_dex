use std::time::Duration;
use std::{collections::{BTreeMap, HashMap}, sync::Arc};
use ed25519_dalek::SigningKey;

use serde::{Deserialize, Serialize};

use crate::{blockchain::{Block, Proposal}, consensus::{hotstuff::HotStuffMessage, pbft::PbftMessage}, node::NodeId};
use crate::crypto::{SessionManifest, SignedConsensusMessage, sign_consensus};

pub mod pbft;
pub mod hotstuff;

pub fn quorum_size(node_count: u64) -> u64 {
    assert!(node_count > 0, "consensus needs at least one validator");
    node_count - (node_count - 1) / 3
}

pub trait Consensus {
    fn set_chain_id(&mut self, _chain_id: String) {}
    fn set_signer(&mut self, _manifest: Arc<SessionManifest>, _key: SigningKey) {}
    fn record_verified(&mut self, _message: SignedConsensusMessage) {}
    fn commit_certificate(&mut self, _block: &Block) -> Option<CommitCertificate> { None }
    fn set_node_count(&mut self, count: u64);
    fn restore_from_height(&mut self, height: u64);

    /// Called after chain/hash/state-validated blocks are copied from a peer.
    fn catch_up_to_height(&mut self, height: u64) -> Vec<ConsensusAction> {
        self.restore_from_height(height);
        vec![]
    }

    fn supports_catch_up(&self) -> bool { false }

    fn on_start(&mut self) -> Vec<ConsensusAction>;

    fn on_event(
        &mut self,
        event: ConsensusEvent,
    ) -> Vec<ConsensusAction>;

    fn leader_for_view(&self, view: u64) -> NodeId;

    fn prepare_quorum(&self) -> u64;

    fn commit_quorum(&self) -> u64;
    fn quorum(&self) -> u64;


    fn f(&self) -> u64;

    fn can_propose(&self) -> bool;

    fn consensus_started(&mut self, turn_on: bool) -> bool;

    fn current_primary(&self) -> NodeId;

    fn set_timeout(&mut self, timeout_ms: u64);

    fn get_timeout(&self) -> u64;

    //
    // Monitoring API
    //
    fn current_view(&self) -> u64;

    fn next_sequence(&self) -> u64;

    fn has_active_proposal(&self) -> bool;
}

impl<T: Consensus + ?Sized> Consensus for Box<T> {
    fn set_chain_id(&mut self, chain_id: String) { (**self).set_chain_id(chain_id) }
    fn set_signer(&mut self, manifest: Arc<SessionManifest>, key: SigningKey) { (**self).set_signer(manifest, key) }
    fn record_verified(&mut self, message: SignedConsensusMessage) { (**self).record_verified(message) }
    fn commit_certificate(&mut self, block: &Block) -> Option<CommitCertificate> { (**self).commit_certificate(block) }
    fn set_node_count(&mut self, count: u64) { (**self).set_node_count(count) }
    fn restore_from_height(&mut self, height: u64) { (**self).restore_from_height(height) }
    fn catch_up_to_height(&mut self, height: u64) -> Vec<ConsensusAction> { (**self).catch_up_to_height(height) }
    fn supports_catch_up(&self) -> bool { (**self).supports_catch_up() }
    fn on_start(&mut self) -> Vec<ConsensusAction> { (**self).on_start() }
    fn on_event(&mut self, event: ConsensusEvent) -> Vec<ConsensusAction> { (**self).on_event(event) }
    fn leader_for_view(&self, view: u64) -> NodeId { (**self).leader_for_view(view) }
    fn prepare_quorum(&self) -> u64 { (**self).prepare_quorum() }
    fn commit_quorum(&self) -> u64 { (**self).commit_quorum() }
    fn quorum(&self) -> u64 { (**self).quorum() }
    fn f(&self) -> u64 { (**self).f() }
    fn can_propose(&self) -> bool { (**self).can_propose() }
    fn consensus_started(&mut self, turn_on: bool) -> bool { (**self).consensus_started(turn_on) }
    fn current_primary(&self) -> NodeId { (**self).current_primary() }
    fn set_timeout(&mut self, timeout_ms: u64) { (**self).set_timeout(timeout_ms) }
    fn get_timeout(&self) -> u64 { (**self).get_timeout() }
    fn current_view(&self) -> u64 { (**self).current_view() }
    fn next_sequence(&self) -> u64 { (**self).next_sequence() }
    fn has_active_proposal(&self) -> bool { (**self).has_active_proposal() }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum ConsensusMessage {
    Pbft(PbftMessage),
    Hotstuff(HotStuffMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommitCertificate {
    Pbft { view: u64, sequence: u64, digest: [u8; 32], proofs: Vec<SignedConsensusMessage> },
    Hotstuff { qc: hotstuff::QuorumCertificate },
}

pub enum ConsensusAction {
    Broadcast(ConsensusMessage),

    Send {
        peer: NodeId,
        message: ConsensusMessage,
    },

    Commit(Proposal),

    ScheduleTimeout {
        view: u64,
        sequence: u64,
        duration: Duration,
    },
}

pub enum ConsensusEvent {
    Message {
        from: NodeId,
        message: ConsensusMessage,
    },

    Timeout {
        view: u64,
        sequence: u64,
    },

    Proposal(Block),

    /// A transaction is waiting at a follower. The algorithm owns its timer policy.
    TransactionAvailable,

    /// Transport could not reach the current leader. This is a hint, not a vote.
    LeaderUnavailable,
}

impl ConsensusMessage {
    /// Candidate block for state-machine validation before consensus may vote for it.
    pub fn proposed_block(&self) -> Option<&Block> {
        match self {
            Self::Pbft(PbftMessage::PrePrepare { proposal, .. }) => Some(&proposal.block),
            Self::Hotstuff(HotStuffMessage::Prepare { proposal, .. }) => Some(&proposal.block),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct EvidenceStore {
    signer: Option<(Arc<SessionManifest>, SigningKey, NodeId)>,
    messages: HashMap<EvidenceKey, BTreeMap<NodeId, SignedConsensusMessage>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum EvidenceKey {
    PbftPrepare(u64, u64, [u8; 32]),
    PbftCommit(u64, u64, [u8; 32]),
    PbftViewChange(u64, u64),
    HotstuffVote(u64, u64, hotstuff::Phase, [u8; 32]),
    HotstuffNewView(u64),
}

impl EvidenceStore {
    pub fn set_signer(&mut self, manifest: Arc<SessionManifest>, key: SigningKey, node_id: NodeId) {
        self.signer = Some((manifest, key, node_id));
    }

    pub fn manifest(&self) -> Option<&SessionManifest> { self.signer.as_ref().map(|(manifest, _, _)| manifest.as_ref()) }

    pub fn record(&mut self, signed: SignedConsensusMessage) {
        let Some(id) = evidence_key(&signed.message) else { return; };
        self.messages.entry(id).or_default().insert(signed.signer, signed);
    }

    pub fn proofs(&mut self, example: &ConsensusMessage, voters: &[NodeId]) -> Option<Vec<SignedConsensusMessage>> {
        let key = evidence_key(example)?;
        let (manifest, secret, own_id) = self.signer.as_ref()?;
        if voters.contains(own_id) && !self.messages.get(&key).is_some_and(|found| found.contains_key(own_id)) {
            let signed = sign_consensus(example.clone(), *own_id, manifest, secret).ok()?;
            self.record(signed);
        }
        let found = self.messages.get(&key)?;
        voters.iter().map(|id| found.get(id).cloned()).collect()
    }
}

fn evidence_key(message: &ConsensusMessage) -> Option<EvidenceKey> {
    match message {
        ConsensusMessage::Pbft(PbftMessage::Prepare { view, sequence, digest }) => Some(EvidenceKey::PbftPrepare(*view, *sequence, *digest)),
        ConsensusMessage::Pbft(PbftMessage::Commit { view, sequence, digest }) => Some(EvidenceKey::PbftCommit(*view, *sequence, *digest)),
        ConsensusMessage::Pbft(PbftMessage::ViewChange { new_view, sequence }) => Some(EvidenceKey::PbftViewChange(*new_view, *sequence)),
        ConsensusMessage::Hotstuff(HotStuffMessage::Vote { view, height, phase, digest }) => Some(EvidenceKey::HotstuffVote(*view, *height, *phase, *digest)),
        ConsensusMessage::Hotstuff(HotStuffMessage::NewView { view, .. }) => Some(EvidenceKey::HotstuffNewView(*view)),
        _ => None,
    }
}

#[cfg(test)]
mod quorum_tests {
    use super::quorum_size;

    #[test]
    fn arbitrary_node_counts_keep_byzantine_quorum_intersection() {
        for (nodes, expected) in [(4, 3), (7, 5), (13, 9), (20, 14)] {
            let faulty = (nodes - 1) / 3;
            let quorum = quorum_size(nodes);
            assert_eq!(quorum, expected);
            assert!(2 * quorum - nodes > faulty);
        }
    }
}
