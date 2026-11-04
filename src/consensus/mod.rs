use ed25519_dalek::SigningKey;
use std::time::Duration;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::crypto::{SessionManifest, SignedConsensusMessage, sign_consensus};
use crate::{
    blockchain::{Block, Digest, Proposal},
    consensus::{chained::ChainedMessage, hotstuff::HotStuffMessage, pbft::PbftMessage},
    node::NodeId,
};

pub mod chained;
pub mod fast;
pub mod hotstuff;
pub mod pbft;
pub mod safety_journal;

pub fn quorum_size(node_count: u64) -> u64 {
    assert!(node_count > 0, "consensus needs at least one validator");
    node_count - (node_count - 1) / 3
}

pub trait Consensus {
    fn persist_before_actions(
        &mut self,
        _actions: &[ConsensusAction],
        _durable_height: u64,
    ) -> std::io::Result<()> {
        Ok(())
    }
    fn set_chain_id(&mut self, _chain_id: String) {}
    fn set_signer(&mut self, _manifest: Arc<SessionManifest>, _key: SigningKey) {}
    fn record_verified(&mut self, _message: SignedConsensusMessage) {}
    fn commit_certificate(&mut self, _block: &Block) -> Option<CommitCertificate> {
        None
    }
    fn set_node_count(&mut self, count: u64);
    fn restore_from_height(&mut self, height: u64);

    /// Called after chain/hash/state-validated blocks are copied from a peer.
    fn catch_up_to_height(&mut self, height: u64) -> Vec<ConsensusAction> {
        self.restore_from_height(height);
        vec![]
    }

    /// Called only after catch-up advanced the durable state. A verified tip
    /// may justify the current view or its configured rotation boundary.
    fn adopt_verified_tip(
        &mut self,
        _block: &Block,
        _certificate: &CommitCertificate,
    ) -> Vec<ConsensusAction> {
        vec![]
    }

    fn supports_catch_up(&self) -> bool {
        false
    }

    fn on_start(&mut self) -> Vec<ConsensusAction>;

    fn on_event(&mut self, event: ConsensusEvent) -> Vec<ConsensusAction>;

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
    fn speculative_parent(&self) -> Option<Digest> {
        None
    }
    fn is_chained(&self) -> bool {
        false
    }
    /// A proposer may wait briefly for more ready work before starting a round.
    fn supports_batched_proposals(&self) -> bool {
        self.is_chained()
    }
    fn needs_empty_block(&self) -> bool {
        false
    }
    fn certified_branch(&self) -> Vec<chained::CertifiedBranchEntry> {
        vec![]
    }
    fn import_certified_branch_entry(
        &mut self,
        _entry: chained::CertifiedBranchEntry,
    ) -> Vec<ConsensusAction> {
        vec![]
    }
}

impl<T: Consensus + ?Sized> Consensus for Box<T> {
    fn persist_before_actions(
        &mut self,
        actions: &[ConsensusAction],
        height: u64,
    ) -> std::io::Result<()> {
        (**self).persist_before_actions(actions, height)
    }
    fn set_chain_id(&mut self, chain_id: String) {
        (**self).set_chain_id(chain_id)
    }
    fn set_signer(&mut self, manifest: Arc<SessionManifest>, key: SigningKey) {
        (**self).set_signer(manifest, key)
    }
    fn record_verified(&mut self, message: SignedConsensusMessage) {
        (**self).record_verified(message)
    }
    fn commit_certificate(&mut self, block: &Block) -> Option<CommitCertificate> {
        (**self).commit_certificate(block)
    }
    fn set_node_count(&mut self, count: u64) {
        (**self).set_node_count(count)
    }
    fn restore_from_height(&mut self, height: u64) {
        (**self).restore_from_height(height)
    }
    fn catch_up_to_height(&mut self, height: u64) -> Vec<ConsensusAction> {
        (**self).catch_up_to_height(height)
    }
    fn adopt_verified_tip(
        &mut self,
        block: &Block,
        certificate: &CommitCertificate,
    ) -> Vec<ConsensusAction> {
        (**self).adopt_verified_tip(block, certificate)
    }
    fn supports_catch_up(&self) -> bool {
        (**self).supports_catch_up()
    }
    fn on_start(&mut self) -> Vec<ConsensusAction> {
        (**self).on_start()
    }
    fn on_event(&mut self, event: ConsensusEvent) -> Vec<ConsensusAction> {
        (**self).on_event(event)
    }
    fn leader_for_view(&self, view: u64) -> NodeId {
        (**self).leader_for_view(view)
    }
    fn prepare_quorum(&self) -> u64 {
        (**self).prepare_quorum()
    }
    fn commit_quorum(&self) -> u64 {
        (**self).commit_quorum()
    }
    fn quorum(&self) -> u64 {
        (**self).quorum()
    }
    fn f(&self) -> u64 {
        (**self).f()
    }
    fn can_propose(&self) -> bool {
        (**self).can_propose()
    }
    fn consensus_started(&mut self, turn_on: bool) -> bool {
        (**self).consensus_started(turn_on)
    }
    fn current_primary(&self) -> NodeId {
        (**self).current_primary()
    }
    fn set_timeout(&mut self, timeout_ms: u64) {
        (**self).set_timeout(timeout_ms)
    }
    fn get_timeout(&self) -> u64 {
        (**self).get_timeout()
    }
    fn current_view(&self) -> u64 {
        (**self).current_view()
    }
    fn next_sequence(&self) -> u64 {
        (**self).next_sequence()
    }
    fn has_active_proposal(&self) -> bool {
        (**self).has_active_proposal()
    }
    fn speculative_parent(&self) -> Option<Digest> {
        (**self).speculative_parent()
    }
    fn is_chained(&self) -> bool {
        (**self).is_chained()
    }
    fn supports_batched_proposals(&self) -> bool {
        (**self).supports_batched_proposals()
    }
    fn needs_empty_block(&self) -> bool {
        (**self).needs_empty_block()
    }
    fn certified_branch(&self) -> Vec<chained::CertifiedBranchEntry> {
        (**self).certified_branch()
    }
    fn import_certified_branch_entry(
        &mut self,
        entry: chained::CertifiedBranchEntry,
    ) -> Vec<ConsensusAction> {
        (**self).import_certified_branch_entry(entry)
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum ConsensusMessage {
    Pbft(PbftMessage),
    Hotstuff(HotStuffMessage),
    Chained(ChainedMessage),
    Hotstuff2(fast::FastMessage),
    Votor(fast::FastMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommitCertificate {
    Pbft {
        view: u64,
        sequence: u64,
        digest: [u8; 32],
        proofs: Vec<SignedConsensusMessage>,
    },
    Hotstuff {
        qc: hotstuff::QuorumCertificate,
    },
    Chained {
        qcs: [chained::ChainedQC; 3],
    },
    Hotstuff2 {
        prepare: fast::FastQc,
        finality: fast::FastQc,
    },
    Votor {
        notar: Option<fast::FastQc>,
        finality: fast::FastQc,
    },
}

#[derive(Clone)]
pub enum ConsensusAction {
    Broadcast(ConsensusMessage),

    Send {
        peer: NodeId,
        message: ConsensusMessage,
    },

    Commit(Proposal),

    /// Retry production as soon as a view or certificate becomes usable.
    TryPropose,
    PrewarmNextLeader,

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
    /// Retry missing candidate bodies after a reordered or interrupted bulk send.
    DataRetry,

    /// Transport could not reach the current leader. This is a hint, not a vote.
    LeaderUnavailable,

    /// A locally committed block reached the configured rotation boundary.
    /// The certificate view prevents a late commit from rotating a newer view.
    AutomaticViewChange {
        committed_view: u64,
        next_sequence: u64,
    },
}

impl ConsensusMessage {
    pub fn is_bulk(&self) -> bool {
        matches!(
            self,
            Self::Chained(
                ChainedMessage::Data { .. }
                    | ChainedMessage::CompactData { .. }
                    | ChainedMessage::PrewarmBranch { .. }
                    | ChainedMessage::BranchResponse { .. }
            ) | Self::Hotstuff2(
                fast::FastMessage::Proposal { .. } | fast::FastMessage::CompactProposal { .. }
            ) | Self::Votor(fast::FastMessage::Proposal { .. })
        )
    }
    pub fn fast_height(&self) -> Option<u64> {
        let message = match self {
            Self::Hotstuff2(message) | Self::Votor(message) => message,
            _ => return None,
        };
        Some(match message {
            fast::FastMessage::Proposal { height, .. }
            | fast::FastMessage::CompactProposal { height, .. }
            | fast::FastMessage::ProposalRequest { height, .. }
            | fast::FastMessage::RotationNotice { height, .. }
            | fast::FastMessage::Vote { height, .. }
            | fast::FastMessage::NewView { height, .. }
            | fast::FastMessage::StartView { height, .. } => *height,
            fast::FastMessage::Certified { qc } => qc.height,
        })
    }
    /// Identify a future-height message without serializing its (possibly large) body.
    /// A newer retransmission from the same validator replaces the older copy.
    pub fn fast_pending_key(&self) -> Option<(u64, u64, u8, Digest)> {
        let (mode, message) = match self {
            Self::Hotstuff2(message) => (0, message),
            Self::Votor(message) => (16, message),
            _ => return None,
        };
        let kind = |vote: fast::VoteKind| match vote {
            fast::VoteKind::Prepare => 0,
            fast::VoteKind::Notarize => 1,
            fast::VoteKind::Final => 2,
            fast::VoteKind::Skip => 3,
        };
        Some(match message {
            fast::FastMessage::Proposal {
                view,
                height,
                proposal,
                ..
            } => (*view, *height, mode, proposal.to_digest()),
            fast::FastMessage::CompactProposal {
                view,
                height,
                digest,
                ..
            } => (*view, *height, mode, *digest),
            fast::FastMessage::ProposalRequest {
                view,
                height,
                digest,
            } => (*view, *height, mode + 11, *digest),
            fast::FastMessage::RotationNotice {
                view,
                height,
                previous,
            } => (*view, *height, mode + 12, previous.digest),
            fast::FastMessage::Vote {
                view,
                height,
                digest,
                kind: vote,
            } => (*view, *height, mode + 1 + kind(*vote), *digest),
            fast::FastMessage::Certified { qc } => {
                (qc.view, qc.height, mode + 5 + kind(qc.kind), qc.digest)
            }
            fast::FastMessage::NewView {
                view,
                height,
                high_qc,
                ..
            } => (
                *view,
                *height,
                mode + 9,
                high_qc.as_ref().map_or([0; 32], |qc| qc.digest),
            ),
            fast::FastMessage::StartView {
                view,
                height,
                high_qc,
                ..
            } => (
                *view,
                *height,
                mode + 10,
                high_qc.as_ref().map_or([0; 32], |qc| qc.digest),
            ),
        })
    }
    /// Candidate block for state-machine validation before consensus may vote for it.
    pub fn proposed_block(&self) -> Option<&Block> {
        match self {
            Self::Pbft(PbftMessage::PrePrepare { proposal, .. }) => Some(&proposal.block),
            Self::Hotstuff(HotStuffMessage::Prepare { proposal, .. }) => Some(&proposal.block),
            Self::Chained(ChainedMessage::Data { proposal, .. }) => Some(&proposal.block),
            Self::Chained(ChainedMessage::Prepare {
                proposal: Some(proposal),
                ..
            }) => Some(&proposal.block),
            Self::Hotstuff2(fast::FastMessage::Proposal { proposal, .. })
            | Self::Votor(fast::FastMessage::Proposal { proposal, .. }) => Some(&proposal.block),
            Self::Hotstuff2(fast::FastMessage::NewView {
                proposal: Some(proposal),
                ..
            })
            | Self::Votor(fast::FastMessage::NewView {
                proposal: Some(proposal),
                ..
            })
            | Self::Hotstuff2(fast::FastMessage::StartView {
                proposal: Some(proposal),
                ..
            })
            | Self::Votor(fast::FastMessage::StartView {
                proposal: Some(proposal),
                ..
            }) => Some(&proposal.block),
            _ => None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct EvidenceStore {
    #[serde(skip)]
    signer: Option<(Arc<SessionManifest>, SigningKey, NodeId)>,
    #[serde(with = "safety_journal::map_entries")]
    messages: HashMap<EvidenceKey, BTreeMap<NodeId, SignedConsensusMessage>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum EvidenceKey {
    PbftPrepare(u64, u64, [u8; 32]),
    PbftCommit(u64, u64, [u8; 32]),
    PbftViewChange(u64, u64),
    HotstuffVote(u64, u64, hotstuff::Phase, [u8; 32]),
    HotstuffNewView(u64),
    ChainedVote(u64, u64, Digest, Digest),
    ChainedNewView(u64),
    FastVote(bool, u64, u64, fast::VoteKind, Digest),
    FastNewView(bool, u64),
}

impl EvidenceStore {
    pub fn set_signer(&mut self, manifest: Arc<SessionManifest>, key: SigningKey, node_id: NodeId) {
        self.signer = Some((manifest, key, node_id));
    }

    pub fn manifest(&self) -> Option<&SessionManifest> {
        self.signer
            .as_ref()
            .map(|(manifest, _, _)| manifest.as_ref())
    }

    pub fn record(&mut self, signed: SignedConsensusMessage) {
        let Some(id) = evidence_key(&signed.message) else {
            return;
        };
        self.messages
            .entry(id)
            .or_default()
            .insert(signed.signer, signed);
    }

    /// Fast-mode certificates are copied into the durable block journal.
    /// Votes for older heights cannot contribute to a later certificate.
    pub fn prune_fast_before(&mut self, height: u64, view: u64) {
        self.messages.retain(|key, messages| match key {
            EvidenceKey::FastVote(_, _, sequence, _, _) => *sequence >= height,
            EvidenceKey::FastNewView(_, new_view) => {
                messages.retain(|_, signed| {
                    signed
                        .message
                        .fast_height()
                        .is_some_and(|sequence| sequence >= height)
                });
                *new_view >= view && !messages.is_empty()
            }
            _ => true,
        });
    }

    pub fn proofs(
        &mut self,
        example: &ConsensusMessage,
        voters: &[NodeId],
    ) -> Option<Vec<SignedConsensusMessage>> {
        let key = evidence_key(example)?;
        let (manifest, secret, own_id) = self.signer.as_ref()?;
        if voters.contains(own_id)
            && !self
                .messages
                .get(&key)
                .is_some_and(|found| found.contains_key(own_id))
        {
            let signed = sign_consensus(example.clone(), *own_id, manifest, secret).ok()?;
            self.record(signed);
        }
        let found = self.messages.get(&key)?;
        voters.iter().map(|id| found.get(id).cloned()).collect()
    }
}

fn evidence_key(message: &ConsensusMessage) -> Option<EvidenceKey> {
    match message {
        ConsensusMessage::Pbft(PbftMessage::Prepare {
            view,
            sequence,
            digest,
        }) => Some(EvidenceKey::PbftPrepare(*view, *sequence, *digest)),
        ConsensusMessage::Pbft(PbftMessage::Commit {
            view,
            sequence,
            digest,
        }) => Some(EvidenceKey::PbftCommit(*view, *sequence, *digest)),
        ConsensusMessage::Pbft(PbftMessage::ViewChange { new_view, sequence }) => {
            Some(EvidenceKey::PbftViewChange(*new_view, *sequence))
        }
        ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
            view,
            height,
            phase,
            digest,
        }) => Some(EvidenceKey::HotstuffVote(*view, *height, *phase, *digest)),
        ConsensusMessage::Hotstuff(HotStuffMessage::NewView { view, .. }) => {
            Some(EvidenceKey::HotstuffNewView(*view))
        }
        ConsensusMessage::Chained(ChainedMessage::Vote {
            view,
            height,
            digest,
            parent_digest,
        }) => Some(EvidenceKey::ChainedVote(
            *view,
            *height,
            *digest,
            *parent_digest,
        )),
        ConsensusMessage::Chained(ChainedMessage::NewView { view, .. }) => {
            Some(EvidenceKey::ChainedNewView(*view))
        }
        ConsensusMessage::Hotstuff2(fast::FastMessage::Vote {
            view,
            height,
            kind,
            digest,
        }) => Some(EvidenceKey::FastVote(false, *view, *height, *kind, *digest)),
        ConsensusMessage::Votor(fast::FastMessage::Vote {
            view,
            height,
            kind,
            digest,
        }) => Some(EvidenceKey::FastVote(true, *view, *height, *kind, *digest)),
        ConsensusMessage::Hotstuff2(fast::FastMessage::NewView { view, .. }) => {
            Some(EvidenceKey::FastNewView(false, *view))
        }
        ConsensusMessage::Votor(fast::FastMessage::NewView { view, .. }) => {
            Some(EvidenceKey::FastNewView(true, *view))
        }
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
