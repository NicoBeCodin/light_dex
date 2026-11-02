//! Three-chain HotStuff. A QC for a new block advances the certification of
//! its ancestors; three consecutive certified blocks commit the oldest one.
//! This is deliberately separate from the older, three-phase-per-block engine.
use crate::{
    blockchain::{Block, Digest, Proposal},
    consensus::{
        CommitCertificate, Consensus, ConsensusAction, ConsensusEvent, ConsensusMessage,
        EvidenceStore,
    },
    crypto::{
        SessionManifest, SignedConsensusMessage, verify_chained_qc, verify_chained_start_view,
    },
    node::NodeId,
};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_CERTIFIED_BRANCH_ANCESTORS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainedQC {
    pub view: u64,
    pub height: u64,
    pub digest: Digest,
    pub parent_digest: Digest,
    pub voters: Vec<NodeId>,
    pub signatures: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertifiedBranchEntry {
    pub proposal: Proposal,
    pub qc: ChainedQC,
}
/// A signed new-view statement without its proposal bodies. The signature
/// commits to `branch_digest`, so a leader cannot substitute a different
/// branch when assembling a StartView quorum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainedNewViewProof {
    pub signer: NodeId,
    pub view: u64,
    pub high_qc: ChainedQC,
    pub branch_digest: Digest,
    pub signature: String,
}

/// A proposal whose signed transactions were already disseminated to peers.
/// The receiver reconstructs the exact block and checks its digest before
/// passing it to consensus. Missing transactions trigger a full-body fetch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactProposal {
    pub header: Block,
    pub transaction_digests: Vec<Digest>,
}

impl CompactProposal {
    pub fn from_block(block: &Block) -> Self {
        let mut header = block.clone();
        let transaction_digests = header
            .transactions
            .iter()
            .map(|tx| tx.to_digest())
            .collect();
        header.transactions.clear();
        if let Some(auth) = &mut header.authentication {
            auth.signatures.clear();
        }
        Self {
            header,
            transaction_digests,
        }
    }

    pub fn reconstruct(
        &self,
        digest: Digest,
        mut lookup: impl FnMut(&Digest) -> Option<(crate::blockchain::Transaction, String)>,
    ) -> Result<Option<Proposal>, &'static str> {
        let mut block = self.header.clone();
        if !block.transactions.is_empty()
            || self.transaction_digests.len()
                > crate::blockchain::BlockchainState::MAX_PROPOSAL_TRANSACTIONS
        {
            return Err("invalid compact proposal shape");
        }
        let Some(authentication) = &mut block.authentication else {
            return Err("unsigned compact proposal");
        };
        if !authentication.signatures.is_empty() {
            return Err("compact proposal contains signatures");
        }
        for transaction_digest in &self.transaction_digests {
            let Some((transaction, signature)) = lookup(transaction_digest) else {
                return Ok(None);
            };
            if transaction.to_digest() != *transaction_digest {
                return Err("compact transaction digest mismatch");
            }
            block.transactions.push(transaction);
            authentication.signatures.push(signature);
        }
        if block.to_digest() != digest {
            return Err("compact proposal digest mismatch");
        }
        Ok(Some(Proposal { block }))
    }
}

pub fn branch_digest(branch: &[Proposal]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(b"light-dex/chained-branch/v1");
    hasher.update((branch.len() as u64).to_be_bytes());
    for proposal in branch {
        hasher.update(proposal.to_digest());
    }
    hasher.finalize().into()
}
impl ChainedQC {
    pub fn genesis() -> Self {
        Self {
            view: 0,
            height: 0,
            digest: [0; 32],
            parent_digest: [0; 32],
            voters: Vec::new(),
            signatures: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChainedMessage {
    /// Candidate body travels separately from the small ordering message.
    Data {
        view: u64,
        height: u64,
        digest: Digest,
        proposal: Proposal,
    },
    CompactData {
        view: u64,
        height: u64,
        digest: Digest,
        compact: CompactProposal,
    },
    DataRequest {
        view: u64,
        digest: Digest,
    },
    Prepare {
        view: u64,
        height: u64,
        digest: Digest,
        proposal: Option<Proposal>,
        justify: ChainedQC,
    },
    Vote {
        view: u64,
        height: u64,
        digest: Digest,
        parent_digest: Digest,
    },
    Certified {
        qc: ChainedQC,
    },
    /// Small view notification for every validator. Only the next leader
    /// needs the proposal bodies in NewView.
    ViewNotice {
        view: u64,
        high_qc: ChainedQC,
    },
    PrewarmBranch {
        view: u64,
        high_qc: ChainedQC,
        branch: Vec<Proposal>,
    },
    NewView {
        view: u64,
        high_qc: ChainedQC,
        branch_digest: Digest,
        /// Usually empty: the next leader reconstructs the prewarmed branch.
        branch: Vec<Proposal>,
    },
    BranchRequest {
        view: u64,
        branch_digest: Digest,
    },
    BranchResponse {
        view: u64,
        branch_digest: Digest,
        branch: Vec<Proposal>,
    },
    StartView {
        view: u64,
        high_qc: ChainedQC,
        branch_digest: Digest,
        branch: Vec<Proposal>,
        proofs: Vec<ChainedNewViewProof>,
    },
}

pub struct ChainedHotStuff {
    node_id: NodeId,
    node_count: u64,
    view: u64,
    next_height: u64,
    last_committed: Option<u64>,
    high_qc: ChainedQC,
    locked_qc: ChainedQC,
    proposals: HashMap<Digest, Proposal>,
    pending_prepares: HashMap<Digest, (NodeId, u64, u64, ChainedQC)>,
    pending_data_requests: HashMap<Digest, Instant>,
    pending_qcs: HashMap<Digest, ChainedQC>,
    qcs: HashMap<Digest, ChainedQC>,
    votes: HashMap<(u64, u64, Digest), HashSet<NodeId>>,
    voted: HashMap<(u64, u64), Digest>,
    certified: HashSet<Digest>,
    commit_proofs: HashMap<Digest, [ChainedQC; 3]>,
    new_views: HashMap<u64, HashMap<NodeId, (ChainedQC, Vec<Proposal>)>>,
    sent_branches: HashMap<(u64, Digest), Vec<Proposal>>,
    pending_new_views: HashMap<(u64, NodeId), (ChainedQC, Digest)>,
    pending_start_views: HashMap<u64, (ChainedQC, Digest, Vec<ChainedNewViewProof>)>,
    next_branch_retry_at: Instant,
    active_proposal: Option<Digest>,
    view_ready: bool,
    started: bool,
    timeout_ms: u64,
    armed_timer: Option<(u64, u64)>,
    rotation_interval: Option<u64>,
    evidence: EvidenceStore,
}

impl ChainedHotStuff {
    pub fn new(node_id: NodeId, node_count: u64) -> Self {
        Self {
            node_id,
            node_count,
            view: 0,
            next_height: 0,
            last_committed: None,
            high_qc: ChainedQC::genesis(),
            locked_qc: ChainedQC::genesis(),
            proposals: HashMap::new(),
            pending_prepares: HashMap::new(),
            pending_data_requests: HashMap::new(),
            pending_qcs: HashMap::new(),
            qcs: HashMap::new(),
            votes: HashMap::new(),
            voted: HashMap::new(),
            certified: HashSet::new(),
            commit_proofs: HashMap::new(),
            new_views: HashMap::new(),
            sent_branches: HashMap::new(),
            pending_new_views: HashMap::new(),
            pending_start_views: HashMap::new(),
            next_branch_retry_at: Instant::now(),
            active_proposal: None,
            view_ready: true,
            started: false,
            timeout_ms: 1_000,
            armed_timer: None,
            rotation_interval: None,
            evidence: EvidenceStore::default(),
        }
    }
    fn valid_qc(&self, qc: &ChainedQC) -> bool {
        if *qc == ChainedQC::genesis() {
            return true;
        }
        if let Some(manifest) = self.evidence.manifest() {
            return verify_chained_qc(qc, manifest).is_ok();
        }
        qc.voters.len() as u64 >= self.quorum() && qc.voters.windows(2).all(|p| p[0] < p[1])
    }
    fn branch(&self, qc: &ChainedQC) -> Vec<Proposal> {
        let mut digest = qc.digest;
        let mut branch = Vec::new();
        // A remote replica can be several committed blocks behind when the
        // leader changes. Include the bounded retained ancestry so its first
        // branch block can attach to an older local anchor without an extra
        // cross-region fetch during the handoff.
        for _ in 0..MAX_CERTIFIED_BRANCH_ANCESTORS {
            let Some(proposal) = self.proposals.get(&digest) else {
                break;
            };
            branch.push(proposal.clone());
            digest = proposal.block.previous_hash;
        }
        branch.reverse();
        branch
    }
    fn extends(&self, mut digest: Digest, ancestor: Digest) -> bool {
        for _ in 0..32 {
            if digest == ancestor {
                return true;
            }
            let Some(parent) = self.proposals.get(&digest) else {
                return false;
            };
            digest = parent.block.previous_hash;
        }
        false
    }
    fn timer(&mut self) -> Vec<ConsensusAction> {
        let key = (self.view, self.next_height);
        if self.armed_timer == Some(key) {
            return vec![];
        }
        self.armed_timer = Some(key);
        vec![ConsensusAction::ScheduleTimeout {
            view: key.0,
            sequence: key.1,
            duration: Duration::from_millis(self.timeout_ms),
        }]
    }
    fn advance_view(&mut self, view: u64) -> Vec<ConsensusAction> {
        if view <= self.view {
            return vec![];
        }
        super::hotstuff::handoff_trace(self.node_id, view, "new_view_sent");
        self.view = view;
        self.view_ready = false;
        self.active_proposal = None;
        self.armed_timer = None;
        self.new_views
            .retain(|old, _| old.saturating_add(2) >= view);
        self.sent_branches
            .retain(|(old, _), _| old.saturating_add(2) >= view);
        self.pending_new_views
            .retain(|(old, _), _| old.saturating_add(2) >= view);
        self.pending_start_views
            .retain(|old, _| old.saturating_add(2) >= view);
        self.pending_prepares
            .retain(|_, (_, old, _, _)| old.saturating_add(2) >= view);
        self.pending_data_requests
            .retain(|digest, _| self.pending_prepares.contains_key(digest));
        let branch = self.branch(&self.high_qc);
        let digest = branch_digest(&branch);
        self.sent_branches.insert((view, digest), branch.clone());
        self.new_views
            .entry(view)
            .or_default()
            .insert(self.node_id, (self.high_qc.clone(), branch.clone()));
        let mut actions = vec![ConsensusAction::Broadcast(ConsensusMessage::Chained(
            ChainedMessage::ViewNotice {
                view,
                high_qc: self.high_qc.clone(),
            },
        ))];
        let next_leader = self.leader_for_view(view);
        if next_leader != self.node_id {
            actions.push(ConsensusAction::Send {
                peer: next_leader,
                message: ConsensusMessage::Chained(ChainedMessage::NewView {
                    view,
                    high_qc: self.high_qc.clone(),
                    branch_digest: digest,
                    branch: Vec::new(),
                }),
            });
        }
        actions.extend(self.timer());
        actions
    }
    fn propose(&mut self, block: Block) -> Vec<ConsensusAction> {
        if !self.can_propose()
            || block.sequence != self.next_height
            || block.previous_hash != self.high_qc.digest
        {
            return vec![];
        }
        let digest = block.to_digest();
        let height = block.sequence;
        self.proposals.insert(
            digest,
            Proposal {
                block: block.clone(),
            },
        );
        self.active_proposal = Some(digest);
        self.voted.insert((self.view, height), digest);
        self.votes
            .entry((self.view, height, digest))
            .or_default()
            .insert(self.node_id);
        let data = if block.authentication.is_some()
            && block.transactions.len() >= 32
            && std::env::var_os("LIGHT_DEX_COMPACT_PROPOSALS").is_some()
        {
            ChainedMessage::CompactData {
                view: self.view,
                height,
                digest,
                compact: CompactProposal::from_block(&block),
            }
        } else {
            ChainedMessage::Data {
                view: self.view,
                height,
                digest,
                proposal: Proposal { block },
            }
        };
        let mut actions = vec![ConsensusAction::Broadcast(ConsensusMessage::Chained(data))];
        actions.push(ConsensusAction::Broadcast(ConsensusMessage::Chained(
            ChainedMessage::Prepare {
                view: self.view,
                height,
                digest,
                proposal: None,
                justify: self.high_qc.clone(),
            },
        )));
        actions.extend(self.timer());
        actions
    }
    fn handle_data(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
        proposal: Proposal,
    ) -> Vec<ConsensusAction> {
        if from != self.leader_for_view(view)
            || proposal.block.sequence != height
            || proposal.to_digest() != digest
            || view.saturating_add(2) < self.view
        {
            return vec![];
        }
        self.proposals.entry(digest).or_insert(proposal);
        self.pending_data_requests.remove(&digest);
        let mut actions = Vec::new();
        if let Some(qc) = self.pending_qcs.remove(&digest) {
            actions.extend(self.handle_certified(qc));
        }
        if let Some((from, view, height, justify)) = self.pending_prepares.remove(&digest) {
            actions.extend(self.handle_prepare(
                from,
                view,
                height,
                digest,
                self.proposals.get(&digest).cloned(),
                justify,
            ));
        }
        actions
    }

    fn retry_data(&mut self) -> Vec<ConsensusAction> {
        let now = Instant::now();
        let mut actions = Vec::new();
        for (digest, (from, view, _, _)) in &self.pending_prepares {
            if *view != self.view {
                continue;
            }
            let Some(ready) = self.pending_data_requests.get_mut(digest) else {
                continue;
            };
            if now < *ready {
                continue;
            }
            *ready = now + Duration::from_millis(500);
            actions.push(ConsensusAction::Send {
                peer: *from,
                message: ConsensusMessage::Chained(ChainedMessage::DataRequest {
                    view: *view,
                    digest: *digest,
                }),
            });
        }
        if now >= self.next_branch_retry_at {
            self.next_branch_retry_at = now + Duration::from_millis(500);
            actions.extend(
                self.pending_new_views
                    .iter()
                    .filter(|((view, _), _)| *view == self.view)
                    .map(|((view, from), (_, digest))| ConsensusAction::Send {
                        peer: *from,
                        message: ConsensusMessage::Chained(ChainedMessage::BranchRequest {
                            view: *view,
                            branch_digest: *digest,
                        }),
                    }),
            );
            for (view, (_, digest, _)) in self
                .pending_start_views
                .iter()
                .filter(|(view, _)| **view >= self.view)
            {
                actions.push(ConsensusAction::Send {
                    peer: self.leader_for_view(*view),
                    message: ConsensusMessage::Chained(ChainedMessage::BranchRequest {
                        view: *view,
                        branch_digest: *digest,
                    }),
                });
            }
        }
        actions
    }

    fn handle_prepare(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
        proposal: Option<Proposal>,
        justify: ChainedQC,
    ) -> Vec<ConsensusAction> {
        if view != self.view
            || !self.view_ready
            || from != self.leader_for_view(view)
            || !self.valid_qc(&justify)
            || justify.view > view
            || (justify != ChainedQC::genesis() && justify.height + 1 != height)
        {
            return vec![];
        }
        let proposal = proposal.or_else(|| self.proposals.get(&digest).cloned());
        let Some(proposal) = proposal else {
            if self.pending_prepares.len() >= 64 {
                self.pending_prepares.clear();
                self.pending_data_requests.clear();
            }
            self.pending_prepares
                .insert(digest, (from, view, height, justify));
            self.pending_data_requests
                .entry(digest)
                .or_insert_with(|| Instant::now() + Duration::from_millis(150));
            return vec![];
        };
        if proposal.block.sequence != height
            || proposal.to_digest() != digest
            || proposal.block.previous_hash != justify.digest
        {
            return vec![];
        }
        self.pending_prepares.remove(&digest);
        self.pending_data_requests.remove(&digest);
        // A Prepare carries the parent QC. A replica can miss the separate
        // Certified broadcast yet still catch up and vote for this child.
        let mut actions =
            if height > self.next_height && self.proposals.contains_key(&justify.digest) {
                self.handle_certified(justify.clone())
            } else {
                Vec::new()
            };
        if view != self.view
            || height != self.next_height
            || (self.locked_qc != ChainedQC::genesis()
                && justify.view <= self.locked_qc.view
                && !self.extends(justify.digest, self.locked_qc.digest))
        {
            return actions;
        }
        match self.voted.entry((view, height)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(digest);
            }
            std::collections::hash_map::Entry::Occupied(_) => return actions,
        }
        self.proposals.insert(digest, proposal);
        if justify != ChainedQC::genesis() {
            self.qcs.insert(justify.digest, justify);
        }
        actions.push(ConsensusAction::Send {
            peer: from,
            message: ConsensusMessage::Chained(ChainedMessage::Vote {
                view,
                height,
                digest,
                parent_digest: self.proposals[&digest].block.previous_hash,
            }),
        });
        actions.extend(self.timer());
        actions
    }
    fn handle_vote(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
        parent_digest: Digest,
    ) -> Vec<ConsensusAction> {
        if view != self.view
            || self.node_id != self.leader_for_view(view)
            || height != self.next_height
            || from >= self.node_count
            || self.active_proposal != Some(digest)
            || self
                .proposals
                .get(&digest)
                .is_none_or(|p| p.block.previous_hash != parent_digest)
        {
            return vec![];
        }
        let quorum = self.quorum();
        let voters = self.votes.entry((view, height, digest)).or_default();
        voters.insert(from);
        if (voters.len() as u64) < quorum || self.certified.contains(&digest) {
            return vec![];
        }
        let mut ids = voters.iter().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        let signatures = if self.evidence.manifest().is_some() {
            let example = ConsensusMessage::Chained(ChainedMessage::Vote {
                view,
                height,
                digest,
                parent_digest,
            });
            let Some(proofs) = self.evidence.proofs(&example, &ids) else {
                return vec![];
            };
            proofs.into_iter().map(|proof| proof.signature).collect()
        } else {
            Vec::new()
        };
        let qc = ChainedQC {
            view,
            height,
            digest,
            parent_digest,
            voters: ids,
            signatures,
        };
        super::hotstuff::handoff_trace(self.node_id, view, "qc_formed");
        let mut actions = vec![ConsensusAction::Broadcast(ConsensusMessage::Chained(
            ChainedMessage::Certified { qc: qc.clone() },
        ))];
        actions.extend(self.handle_certified(qc));
        actions
    }
    fn drain_commits(&mut self) -> Vec<ConsensusAction> {
        let mut actions = Vec::new();
        loop {
            let next = self.last_committed.map_or(0, |height| height + 1);
            let ready = self
                .qcs
                .values()
                .filter(|oldest| oldest.height == next)
                .find_map(|oldest| {
                    let middle = self.qcs.values().find(|middle| {
                        middle.height == next + 1 && middle.parent_digest == oldest.digest
                    })?;
                    let newest = self.qcs.values().find(|newest| {
                        newest.height == next + 2 && newest.parent_digest == middle.digest
                    })?;
                    let proposal = self.proposals.get(&oldest.digest)?.clone();
                    Some((proposal, [oldest.clone(), middle.clone(), newest.clone()]))
                });
            let Some((proposal, proof)) = ready else {
                break;
            };
            if (proof[1].height, proof[1].view) > (self.locked_qc.height, self.locked_qc.view) {
                self.locked_qc = proof[1].clone();
            }
            self.last_committed = Some(next);
            self.commit_proofs.insert(proof[0].digest, proof.clone());
            super::hotstuff::handoff_trace(self.node_id, proof[2].view, "commit_decision");
            actions.push(ConsensusAction::Commit(proposal));
        }
        if let Some(last) = self.last_committed {
            let floor = last.saturating_sub(3);
            self.proposals
                .retain(|_, proposal| proposal.block.sequence >= floor);
            self.qcs
                .retain(|_, certificate| certificate.height >= floor);
            self.certified
                .retain(|digest| self.qcs.contains_key(digest));
            self.votes.retain(|(_, height, _), _| *height >= floor);
            self.voted.retain(|(_, height), _| *height >= floor);
        }
        actions
    }
    fn handle_certified(&mut self, qc: ChainedQC) -> Vec<ConsensusAction> {
        if !self.valid_qc(&qc)
            || qc == ChainedQC::genesis()
            || qc.height < self.last_committed.unwrap_or(0)
        {
            return vec![];
        }
        let Some(proposal) = self.proposals.get(&qc.digest) else {
            if self.pending_qcs.len() >= 64 {
                self.pending_qcs.clear();
            }
            self.pending_qcs.insert(qc.digest, qc);
            return vec![];
        };
        if proposal.block.sequence != qc.height || proposal.block.previous_hash != qc.parent_digest
        {
            return vec![];
        }
        if !self.certified.insert(qc.digest) {
            return vec![];
        }
        super::hotstuff::handoff_trace(self.node_id, qc.view, "certified");
        self.qcs.insert(qc.digest, qc.clone());
        // Lock as soon as a certified child extends a certified parent.
        // Waiting for the third QC would allow a higher view to abandon a
        // two-chain that is already one certificate away from commitment.
        if let Some(parent) = self.qcs.get(&qc.parent_digest) {
            if parent.height + 1 == qc.height
                && (parent.height, parent.view) > (self.locked_qc.height, self.locked_qc.view)
            {
                self.locked_qc = parent.clone();
            }
        }
        let advances_head = self.high_qc == ChainedQC::genesis()
            || (qc.height, qc.view) > (self.high_qc.height, self.high_qc.view);
        if advances_head {
            self.high_qc = qc.clone();
            self.next_height = qc.height + 1;
            self.active_proposal = None;
            self.armed_timer = None;
        }
        let mut actions = Vec::new();
        if advances_head
            && qc.view == self.view
            && self
                .rotation_interval
                .is_some_and(|interval| qc.height > 0 && (qc.height + 1) % interval == 0)
        {
            actions.push(ConsensusAction::PrewarmNextLeader);
            let next_leader = self.leader_for_view(self.view + 1);
            if next_leader != self.node_id {
                actions.push(ConsensusAction::Send {
                    peer: next_leader,
                    message: ConsensusMessage::Chained(ChainedMessage::PrewarmBranch {
                        view: self.view + 1,
                        high_qc: qc.clone(),
                        branch: self.branch(&qc),
                    }),
                });
            }
        }
        actions.extend(self.drain_commits());
        if advances_head
            && qc.view == self.view
            && self
                .rotation_interval
                .is_some_and(|interval| interval > 0 && qc.height > 0 && qc.height % interval == 0)
        {
            actions.extend(self.advance_view(self.view + 1));
        } else if self.node_id == self.current_primary() && self.view_ready {
            actions.push(ConsensusAction::TryPropose);
        }
        actions
    }
    fn handle_new_view(
        &mut self,
        from: NodeId,
        view: u64,
        high_qc: ChainedQC,
        expected_branch_digest: Digest,
        mut branch: Vec<Proposal>,
    ) -> Vec<ConsensusAction> {
        if from >= self.node_count
            || view < self.view
            || !self.valid_qc(&high_qc)
            || branch.len() > MAX_CERTIFIED_BRANCH_ANCESTORS
        {
            return vec![];
        }
        let mut actions = if view > self.view {
            self.advance_view(view)
        } else {
            vec![]
        };
        if self.node_id != self.leader_for_view(view) || self.view_ready {
            return actions;
        }
        if branch.is_empty() && high_qc != ChainedQC::genesis() {
            branch = self.branch(&high_qc);
        }
        if branch_digest(&branch) != expected_branch_digest {
            self.pending_new_views
                .insert((view, from), (high_qc, expected_branch_digest));
            actions.push(ConsensusAction::Send {
                peer: from,
                message: ConsensusMessage::Chained(ChainedMessage::BranchRequest {
                    view,
                    branch_digest: expected_branch_digest,
                }),
            });
            return actions;
        }
        if high_qc != ChainedQC::genesis()
            && branch
                .last()
                .is_none_or(|p| p.to_digest() != high_qc.digest)
        {
            return actions;
        }
        self.pending_new_views.remove(&(view, from));
        for p in &branch {
            self.proposals.entry(p.to_digest()).or_insert(p.clone());
        }
        self.new_views
            .entry(view)
            .or_default()
            .insert(from, (high_qc, branch));
        let votes = &self.new_views[&view];
        if (votes.len() as u64) < self.quorum() {
            return actions;
        }
        super::hotstuff::handoff_trace(self.node_id, view, "new_view_quorum");
        let mut voters = votes.keys().copied().collect::<Vec<_>>();
        voters.sort_unstable();
        let proofs = if self.evidence.manifest().is_some() {
            let example = ConsensusMessage::Chained(ChainedMessage::NewView {
                view,
                high_qc: self.high_qc.clone(),
                branch_digest: branch_digest(&self.branch(&self.high_qc)),
                branch: Vec::new(),
            });
            let Some(proofs) = self.evidence.proofs(&example, &voters) else {
                return actions;
            };
            proofs
                .into_iter()
                .filter_map(|proof| {
                    let ConsensusMessage::Chained(ChainedMessage::NewView {
                        view,
                        high_qc,
                        branch_digest,
                        ..
                    }) = proof.message
                    else {
                        return None;
                    };
                    Some(ChainedNewViewProof {
                        signer: proof.signer,
                        view,
                        high_qc,
                        branch_digest,
                        signature: proof.signature,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let (best, branch) = votes
            .values()
            .max_by_key(|(qc, _)| (qc.height, qc.view, qc.digest))
            .cloned()
            .unwrap();
        if best != ChainedQC::genesis() {
            self.high_qc = best.clone();
            self.next_height = best.height + 1;
            self.qcs.insert(best.digest, best.clone());
            for p in &branch {
                self.proposals.entry(p.to_digest()).or_insert(p.clone());
            }
        }
        self.view_ready = true;
        super::hotstuff::handoff_trace(self.node_id, view, "start_view_sent");
        let digest = branch_digest(&branch);
        self.sent_branches.insert((view, digest), branch);
        actions.push(ConsensusAction::Broadcast(ConsensusMessage::Chained(
            ChainedMessage::StartView {
                view,
                high_qc: best,
                branch_digest: digest,
                branch: Vec::new(),
                proofs,
            },
        )));
        actions.push(ConsensusAction::TryPropose);
        actions
    }
    fn handle_start_view(
        &mut self,
        from: NodeId,
        view: u64,
        high_qc: ChainedQC,
        expected_branch_digest: Digest,
        mut branch: Vec<Proposal>,
        proofs: Vec<ChainedNewViewProof>,
    ) -> Vec<ConsensusAction> {
        if from != self.leader_for_view(view)
            || view < self.view
            || !self.valid_qc(&high_qc)
            || branch.len() > MAX_CERTIFIED_BRANCH_ANCESTORS
            || self.evidence.manifest().is_some_and(|manifest| {
                verify_chained_start_view(
                    view,
                    &high_qc,
                    &expected_branch_digest,
                    &proofs,
                    manifest,
                )
                .is_err()
            })
        {
            return vec![];
        }
        if branch.is_empty() && high_qc != ChainedQC::genesis() {
            branch = self.branch(&high_qc);
        }
        if branch_digest(&branch) != expected_branch_digest {
            let mut actions = if view > self.view {
                self.advance_view(view)
            } else {
                vec![]
            };
            self.pending_start_views
                .insert(view, (high_qc, expected_branch_digest, proofs));
            actions.push(ConsensusAction::Send {
                peer: from,
                message: ConsensusMessage::Chained(ChainedMessage::BranchRequest {
                    view,
                    branch_digest: expected_branch_digest,
                }),
            });
            return actions;
        }
        if high_qc != ChainedQC::genesis()
            && branch
                .last()
                .is_none_or(|p| p.to_digest() != high_qc.digest)
        {
            return vec![];
        }
        self.pending_start_views.remove(&view);
        self.view = view;
        self.view_ready = true;
        self.active_proposal = None;
        super::hotstuff::handoff_trace(self.node_id, view, "start_view_received");
        if high_qc != ChainedQC::genesis()
            && (self.high_qc == ChainedQC::genesis() || high_qc.height >= self.high_qc.height)
        {
            self.next_height = high_qc.height + 1;
            self.qcs.insert(high_qc.digest, high_qc.clone());
            self.high_qc = high_qc;
        }
        for p in branch {
            self.proposals.entry(p.to_digest()).or_insert(p);
        }
        self.timer()
    }
}

impl Consensus for ChainedHotStuff {
    fn set_signer(&mut self, manifest: Arc<SessionManifest>, key: SigningKey) {
        self.rotation_interval = manifest.auto_view_change_blocks;
        self.evidence.set_signer(manifest, key, self.node_id);
    }
    fn record_verified(&mut self, message: SignedConsensusMessage) {
        self.evidence.record(message);
    }
    fn commit_certificate(&mut self, block: &Block) -> Option<CommitCertificate> {
        Some(CommitCertificate::Chained {
            qcs: self.commit_proofs.remove(&block.to_digest())?,
        })
    }
    fn set_node_count(&mut self, count: u64) {
        self.node_count = count;
    }
    fn restore_from_height(&mut self, height: u64) {
        self.last_committed = height.checked_sub(1);
        self.next_height = height;
    }
    fn catch_up_to_height(&mut self, height: u64) -> Vec<ConsensusAction> {
        if height > self.last_committed.map_or(0, |last| last + 1) {
            self.last_committed = height.checked_sub(1);
            self.next_height = self.next_height.max(height);
            self.active_proposal = None;
            self.armed_timer = None;
        }
        let mut actions = self.drain_commits();
        actions.extend(self.timer());
        actions
    }
    fn supports_catch_up(&self) -> bool {
        true
    }
    fn on_start(&mut self) -> Vec<ConsensusAction> {
        self.started = true;
        if self.next_height == 0 && self.node_id == self.current_primary() {
            let block = self
                .evidence
                .manifest()
                .map(|manifest| {
                    Block::new_initial_block_signed(
                        &manifest.chain_id,
                        crate::message::hex_decode(&manifest.initial_state_root)
                            .expect("validated initial state root"),
                    )
                })
                .unwrap_or_else(Block::new_initial_block);
            self.propose(block)
        } else {
            self.timer()
        }
    }
    fn on_event(&mut self, event: ConsensusEvent) -> Vec<ConsensusAction> {
        match event {
            ConsensusEvent::Message {
                from,
                message: ConsensusMessage::Chained(message),
            } => match message {
                ChainedMessage::Data {
                    view,
                    height,
                    digest,
                    proposal,
                } => self.handle_data(from, view, height, digest, proposal),
                ChainedMessage::CompactData { .. } => vec![],
                ChainedMessage::DataRequest { view, digest } => {
                    if view > self.view
                        || self.node_id != self.leader_for_view(view)
                        || from >= self.node_count
                    {
                        vec![]
                    } else if let Some(proposal) = self.proposals.get(&digest) {
                        vec![ConsensusAction::Send {
                            peer: from,
                            message: ConsensusMessage::Chained(ChainedMessage::Data {
                                view,
                                height: proposal.block.sequence,
                                digest,
                                proposal: proposal.clone(),
                            }),
                        }]
                    } else {
                        vec![]
                    }
                }
                ChainedMessage::Prepare {
                    view,
                    height,
                    digest,
                    proposal,
                    justify,
                } => self.handle_prepare(from, view, height, digest, proposal, justify),
                ChainedMessage::Vote {
                    view,
                    height,
                    digest,
                    parent_digest,
                } => self.handle_vote(from, view, height, digest, parent_digest),
                ChainedMessage::Certified { qc } => self.handle_certified(qc),
                ChainedMessage::ViewNotice { view, high_qc } => {
                    if from < self.node_count && view > self.view && self.valid_qc(&high_qc) {
                        self.advance_view(view)
                    } else {
                        vec![]
                    }
                }
                ChainedMessage::PrewarmBranch {
                    view,
                    high_qc,
                    branch,
                } => {
                    if from >= self.node_count
                        || self.node_id != self.leader_for_view(view)
                        || view < self.view
                        || view > self.view.saturating_add(1)
                        || !self.valid_qc(&high_qc)
                        || branch.len() > MAX_CERTIFIED_BRANCH_ANCESTORS
                        || branch
                            .last()
                            .is_none_or(|p| p.to_digest() != high_qc.digest)
                    {
                        vec![]
                    } else {
                        for proposal in branch {
                            self.proposals
                                .entry(proposal.to_digest())
                                .or_insert(proposal);
                        }
                        vec![]
                    }
                }
                ChainedMessage::NewView {
                    view,
                    high_qc,
                    branch_digest,
                    branch,
                } => self.handle_new_view(from, view, high_qc, branch_digest, branch),
                ChainedMessage::BranchRequest {
                    view,
                    branch_digest,
                } => {
                    if from >= self.node_count {
                        vec![]
                    } else if let Some(branch) = self.sent_branches.get(&(view, branch_digest)) {
                        vec![ConsensusAction::Send {
                            peer: from,
                            message: ConsensusMessage::Chained(ChainedMessage::BranchResponse {
                                view,
                                branch_digest,
                                branch: branch.clone(),
                            }),
                        }]
                    } else {
                        vec![]
                    }
                }
                ChainedMessage::BranchResponse {
                    view,
                    branch_digest,
                    branch,
                } => {
                    if crate::consensus::chained::branch_digest(&branch) != branch_digest {
                        return vec![];
                    }
                    if let Some((high_qc, expected)) =
                        self.pending_new_views.get(&(view, from)).cloned()
                    {
                        if expected != branch_digest {
                            vec![]
                        } else {
                            self.handle_new_view(from, view, high_qc, expected, branch)
                        }
                    } else if from == self.leader_for_view(view) {
                        let Some((high_qc, expected, proofs)) =
                            self.pending_start_views.get(&view).cloned()
                        else {
                            return vec![];
                        };
                        if expected != branch_digest {
                            vec![]
                        } else {
                            self.handle_start_view(from, view, high_qc, expected, branch, proofs)
                        }
                    } else {
                        vec![]
                    }
                }
                ChainedMessage::StartView {
                    view,
                    high_qc,
                    branch_digest,
                    branch,
                    proofs,
                } => self.handle_start_view(from, view, high_qc, branch_digest, branch, proofs),
            },
            ConsensusEvent::Timeout { view, sequence }
                if view == self.view && sequence == self.next_height =>
            {
                self.advance_view(view + 1)
            }
            ConsensusEvent::LeaderUnavailable => self.advance_view(self.view + 1),
            ConsensusEvent::Proposal(block) => self.propose(block),
            ConsensusEvent::DataRetry => self.retry_data(),
            ConsensusEvent::TransactionAvailable => self.timer(),
            _ => vec![],
        }
    }
    fn leader_for_view(&self, view: u64) -> NodeId {
        view % self.node_count.max(1)
    }
    fn prepare_quorum(&self) -> u64 {
        self.quorum()
    }
    fn commit_quorum(&self) -> u64 {
        self.quorum()
    }
    fn quorum(&self) -> u64 {
        crate::consensus::quorum_size(self.node_count)
    }
    fn f(&self) -> u64 {
        (self.node_count - 1) / 3
    }
    fn can_propose(&self) -> bool {
        self.started
            && self.view_ready
            && self.node_id == self.current_primary()
            && self.active_proposal.is_none()
    }
    fn consensus_started(&mut self, turn_on: bool) -> bool {
        if turn_on {
            self.started = true;
        }
        self.started
    }
    fn current_primary(&self) -> NodeId {
        self.leader_for_view(self.view)
    }
    fn set_timeout(&mut self, timeout_ms: u64) {
        self.timeout_ms = timeout_ms;
    }
    fn get_timeout(&self) -> u64 {
        self.timeout_ms
    }
    fn current_view(&self) -> u64 {
        self.view
    }
    fn next_sequence(&self) -> u64 {
        self.next_height
    }
    fn has_active_proposal(&self) -> bool {
        self.active_proposal.is_some()
    }
    fn speculative_parent(&self) -> Option<Digest> {
        (self.high_qc != ChainedQC::genesis()).then_some(self.high_qc.digest)
    }
    fn is_chained(&self) -> bool {
        true
    }
    fn needs_empty_block(&self) -> bool {
        self.proposals.values().any(|p| {
            self.last_committed
                .is_none_or(|last| p.block.sequence > last)
                && (p.block.sequence == 0
                    || !p.block.transactions.is_empty()
                    || !p.block.oracle_updates.is_empty())
        })
    }
    fn certified_branch(&self) -> Vec<CertifiedBranchEntry> {
        // A branch-sync response must reach the committed anchor. The short
        // three-proposal handoff branch can omit a parent when durability on
        // the receiver lags several certified proposals behind.
        let mut digest = self.high_qc.digest;
        let mut entries = Vec::new();
        for _ in 0..MAX_CERTIFIED_BRANCH_ANCESTORS {
            let Some(proposal) = self.proposals.get(&digest).cloned() else {
                break;
            };
            let Some(qc) = self.qcs.get(&digest).cloned() else {
                break;
            };
            digest = proposal.block.previous_hash;
            let anchored = proposal.block.sequence <= self.last_committed.unwrap_or(0);
            entries.push(CertifiedBranchEntry { proposal, qc });
            if anchored {
                break;
            }
        }
        entries.reverse();
        entries
    }
    fn import_certified_branch_entry(
        &mut self,
        entry: CertifiedBranchEntry,
    ) -> Vec<ConsensusAction> {
        self.proposals
            .insert(entry.proposal.to_digest(), entry.proposal);
        self.handle_certified(entry.qc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_proposal_requires_exact_transaction_and_signature_bytes() {
        let transaction = crate::blockchain::Transaction::ClaimAirdrop {
            account_id: 7.into(),
            nonce: 0,
        };
        let mut block = Block::new_initial_block();
        block.transactions.push(transaction.clone());
        block.authentication = Some(crate::blockchain::BlockAuthentication {
            version: crate::crypto::PROTOCOL_VERSION,
            chain_id: "compact-test".into(),
            signatures: vec!["signature".into()],
        });
        let digest = block.to_digest();
        let compact = CompactProposal::from_block(&block);
        assert!(compact.header.transactions.is_empty());
        assert!(compact.reconstruct(digest, |_| None).unwrap().is_none());
        let rebuilt = compact
            .reconstruct(digest, |_| Some((transaction.clone(), "signature".into())))
            .unwrap()
            .unwrap();
        assert_eq!(rebuilt.block, block);
        assert!(
            compact
                .reconstruct(digest, |_| Some((transaction.clone(), "different".into())))
                .is_err()
        );
        assert!(
            compact
                .reconstruct([0; 32], |_| Some((transaction.clone(), "signature".into())))
                .is_err()
        );
    }

    #[test]
    fn missing_compact_transactions_can_fetch_the_full_signed_body() {
        let mut leader = ChainedHotStuff::new(0, 4);
        leader.consensus_started(true);
        let mut block = Block::new_initial_block();
        block.transactions = (0..32)
            .map(|index| crate::blockchain::Transaction::ClaimAirdrop {
                account_id: index.into(),
                nonce: 0,
            })
            .collect();
        block.authentication = Some(crate::blockchain::BlockAuthentication {
            version: crate::crypto::PROTOCOL_VERSION,
            chain_id: "compact-test".into(),
            signatures: vec!["signature".into(); 32],
        });
        let digest = block.to_digest();
        let actions = leader.propose(block.clone());
        if std::env::var_os("LIGHT_DEX_COMPACT_PROPOSALS").is_some() {
            assert!(actions.iter().any(|action| matches!(
                action,
                ConsensusAction::Broadcast(ConsensusMessage::Chained(
                    ChainedMessage::CompactData { .. }
                ))
            )));
        }
        let response = leader.on_event(ConsensusEvent::Message {
            from: 1,
            message: ConsensusMessage::Chained(ChainedMessage::DataRequest { view: 0, digest }),
        });
        assert!(response.iter().any(|action| matches!(action,
            ConsensusAction::Send { peer: 1,
                message: ConsensusMessage::Chained(ChainedMessage::Data { proposal, .. })
            } if proposal.block == block
        )));
    }

    #[test]
    fn compact_prepare_waits_for_a_validated_body_then_votes() {
        let mut leader = ChainedHotStuff::new(0, 4);
        let mut follower = ChainedHotStuff::new(1, 4);
        leader.consensus_started(true);
        follower.consensus_started(true);
        let actions = leader.propose(Block::new_initial_block());
        let data = actions
            .iter()
            .find_map(|action| match action {
                ConsensusAction::Broadcast(ConsensusMessage::Chained(
                    message @ ChainedMessage::Data { .. },
                )) => Some(message.clone()),
                _ => None,
            })
            .unwrap();
        let prepare = actions
            .iter()
            .find_map(|action| match action {
                ConsensusAction::Broadcast(ConsensusMessage::Chained(
                    message @ ChainedMessage::Prepare { .. },
                )) => Some(message.clone()),
                _ => None,
            })
            .unwrap();
        let immediate = follower.on_event(ConsensusEvent::Message {
            from: 0,
            message: ConsensusMessage::Chained(prepare),
        });
        assert!(!immediate.iter().any(|action| matches!(
            action,
            ConsensusAction::Send {
                peer: 0,
                message: ConsensusMessage::Chained(ChainedMessage::DataRequest { .. })
            }
        )));
        assert!(follower.retry_data().is_empty());
        std::thread::sleep(Duration::from_millis(160));
        assert!(follower.retry_data().iter().any(|action| matches!(
            action,
            ConsensusAction::Send {
                peer: 0,
                message: ConsensusMessage::Chained(ChainedMessage::DataRequest { .. })
            }
        )));
        let vote = follower.on_event(ConsensusEvent::Message {
            from: 0,
            message: ConsensusMessage::Chained(data),
        });
        assert!(vote.iter().any(|action| matches!(
            action,
            ConsensusAction::Send {
                peer: 0,
                message: ConsensusMessage::Chained(ChainedMessage::Vote { .. })
            }
        )));
    }

    #[test]
    fn compact_new_view_fetches_missing_certified_branch() {
        let mut sender = ChainedHotStuff::new(0, 4);
        let mut receiver = ChainedHotStuff::new(1, 4);
        let block = Block::new_initial_block();
        let digest = block.to_digest();
        sender.proposals.insert(digest, Proposal { block });
        sender.high_qc = ChainedQC {
            view: 0,
            height: 0,
            digest,
            parent_digest: [0; 32],
            voters: vec![0, 1, 2],
            signatures: Vec::new(),
        };
        let actions = sender.advance_view(1);
        let new_view = actions
            .into_iter()
            .find_map(|action| match action {
                ConsensusAction::Send {
                    peer: 1,
                    message: ConsensusMessage::Chained(message @ ChainedMessage::NewView { .. }),
                } => Some(message),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(new_view, ChainedMessage::NewView { ref branch, .. } if branch.is_empty())
        );
        let requests = receiver.on_event(ConsensusEvent::Message {
            from: 0,
            message: ConsensusMessage::Chained(new_view),
        });
        let request = requests
            .into_iter()
            .find_map(|action| match action {
                ConsensusAction::Send {
                    peer: 0,
                    message:
                        ConsensusMessage::Chained(message @ ChainedMessage::BranchRequest { .. }),
                } => Some(message),
                _ => None,
            })
            .unwrap();
        let responses = sender.on_event(ConsensusEvent::Message {
            from: 1,
            message: ConsensusMessage::Chained(request),
        });
        let response = responses
            .into_iter()
            .find_map(|action| match action {
                ConsensusAction::Send {
                    peer: 1,
                    message:
                        ConsensusMessage::Chained(message @ ChainedMessage::BranchResponse { .. }),
                } => Some(message),
                _ => None,
            })
            .unwrap();
        receiver.on_event(ConsensusEvent::Message {
            from: 0,
            message: ConsensusMessage::Chained(response),
        });
        assert!(receiver.new_views[&1].contains_key(&0));
        assert!(!receiver.pending_new_views.contains_key(&(1, 0)));
    }

    #[test]
    fn compact_start_view_fetches_branch_before_becoming_ready() {
        let mut leader = ChainedHotStuff::new(1, 4);
        let mut follower = ChainedHotStuff::new(2, 4);
        let proposal = Proposal {
            block: Block::new_initial_block(),
        };
        let qc = ChainedQC {
            view: 0,
            height: 0,
            digest: proposal.to_digest(),
            parent_digest: [0; 32],
            voters: vec![0, 1, 2],
            signatures: Vec::new(),
        };
        let branch = vec![proposal];
        let digest = branch_digest(&branch);
        leader.sent_branches.insert((1, digest), branch);
        let actions = follower.handle_start_view(1, 1, qc, digest, Vec::new(), Vec::new());
        assert!(!follower.view_ready);
        let request = actions
            .into_iter()
            .find_map(|action| match action {
                ConsensusAction::Send {
                    peer: 1,
                    message:
                        ConsensusMessage::Chained(message @ ChainedMessage::BranchRequest { .. }),
                } => Some(message),
                _ => None,
            })
            .unwrap();
        let response = leader
            .on_event(ConsensusEvent::Message {
                from: 2,
                message: ConsensusMessage::Chained(request),
            })
            .into_iter()
            .find_map(|action| match action {
                ConsensusAction::Send {
                    peer: 2,
                    message:
                        ConsensusMessage::Chained(message @ ChainedMessage::BranchResponse { .. }),
                } => Some(message),
                _ => None,
            })
            .unwrap();
        follower.on_event(ConsensusEvent::Message {
            from: 1,
            message: ConsensusMessage::Chained(response),
        });
        assert!(follower.view_ready);
        assert!(!follower.pending_start_views.contains_key(&1));
    }

    #[test]
    fn handoff_notifies_everyone_but_sends_branch_only_to_next_leader() {
        let mut follower = ChainedHotStuff::new(0, 4);
        let actions = follower.advance_view(1);
        assert!(actions.iter().any(|action| matches!(
            action,
            ConsensusAction::Broadcast(ConsensusMessage::Chained(ChainedMessage::ViewNotice {
                view: 1,
                ..
            }))
        )));
        assert!(actions.iter().any(|action| matches!(
            action,
            ConsensusAction::Send {
                peer: 1,
                message: ConsensusMessage::Chained(ChainedMessage::NewView { view: 1, .. })
            }
        )));
        assert!(!actions.iter().any(|action| matches!(
            action,
            ConsensusAction::Broadcast(ConsensusMessage::Chained(ChainedMessage::NewView { .. }))
        )));
        let mut leader = ChainedHotStuff::new(1, 4);
        let leader_actions = leader.on_event(ConsensusEvent::Message {
            from: 0,
            message: ConsensusMessage::Chained(ChainedMessage::ViewNotice {
                view: 1,
                high_qc: ChainedQC::genesis(),
            }),
        });
        assert_eq!(leader.current_view(), 1);
        assert!(
            !leader_actions
                .iter()
                .any(|action| matches!(action, ConsensusAction::Send { .. }))
        );
        assert!(leader.new_views[&1].contains_key(&1));
    }

    #[test]
    fn handoff_branch_includes_retained_committed_ancestors() {
        let mut consensus = ChainedHotStuff::new(0, 7);
        let mut parent_digest = [0; 32];
        for height in 0..6 {
            let mut block = Block::new_initial_block();
            block.sequence = height;
            block.previous_hash = parent_digest;
            let digest = block.to_digest();
            consensus.proposals.insert(digest, Proposal { block });
            consensus.high_qc = ChainedQC {
                view: 0,
                height,
                digest,
                parent_digest,
                voters: vec![0, 1, 2, 3, 4],
                signatures: Vec::new(),
            };
            parent_digest = digest;
        }
        consensus.last_committed = Some(3);
        let branch = consensus.branch(&consensus.high_qc);
        assert_eq!(
            branch
                .iter()
                .map(|proposal| proposal.block.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn branch_sync_includes_ancestors_beyond_three_proposals() {
        let mut consensus = ChainedHotStuff::new(0, 4);
        let mut parent_digest = [0; 32];
        for height in 0..6 {
            let mut block = Block::new_initial_block();
            block.sequence = height;
            block.previous_hash = parent_digest;
            let digest = block.to_digest();
            consensus.proposals.insert(digest, Proposal { block });
            let qc = ChainedQC {
                view: 0,
                height,
                digest,
                parent_digest,
                voters: vec![0, 1, 2],
                signatures: Vec::new(),
            };
            consensus.qcs.insert(digest, qc.clone());
            consensus.high_qc = qc;
            parent_digest = digest;
        }
        consensus.last_committed = Some(1);
        let branch = consensus.certified_branch();
        assert_eq!(
            branch
                .iter()
                .map(|entry| entry.proposal.block.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn late_certified_parent_drains_commits_in_height_order() {
        let mut consensus = ChainedHotStuff::new(3, 4);
        let mut parent_digest = [0; 32];
        let mut entries = Vec::new();
        for height in 0..5 {
            let mut block = Block::new_initial_block();
            block.sequence = height;
            block.previous_hash = parent_digest;
            let digest = block.to_digest();
            entries.push(CertifiedBranchEntry {
                proposal: Proposal { block },
                qc: ChainedQC {
                    view: 0,
                    height,
                    digest,
                    parent_digest,
                    voters: vec![0, 1, 2],
                    signatures: Vec::new(),
                },
            });
            parent_digest = digest;
        }
        for index in [0, 1, 3, 4] {
            assert!(
                consensus
                    .import_certified_branch_entry(entries[index].clone())
                    .iter()
                    .all(|action| !matches!(action, ConsensusAction::Commit(_)))
            );
        }
        let committed = consensus
            .import_certified_branch_entry(entries[2].clone())
            .into_iter()
            .filter_map(|action| match action {
                ConsensusAction::Commit(proposal) => Some(proposal.block.sequence),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(committed, vec![0, 1, 2]);
    }

    #[test]
    fn prepare_imports_missed_parent_certificate() {
        let mut consensus = ChainedHotStuff::new(2, 4);
        let genesis = Block::new_initial_block();
        let parent_digest = genesis.to_digest();
        consensus
            .proposals
            .insert(parent_digest, Proposal { block: genesis });
        let parent_qc = ChainedQC {
            view: 0,
            height: 0,
            digest: parent_digest,
            parent_digest: [0; 32],
            voters: vec![0, 1, 2],
            signatures: Vec::new(),
        };
        let mut child = Block::new_initial_block();
        child.sequence = 1;
        child.previous_hash = parent_digest;
        let digest = child.to_digest();
        let actions =
            consensus.handle_prepare(0, 0, 1, digest, Some(Proposal { block: child }), parent_qc);
        assert_eq!(consensus.next_height, 1);
        assert!(actions.iter().any(|action| matches!(
            action,
            ConsensusAction::Send {
                peer: 0,
                message: ConsensusMessage::Chained(ChainedMessage::Vote { height: 1, .. }),
            }
        )));
    }
}
