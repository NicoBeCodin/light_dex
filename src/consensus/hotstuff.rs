use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ed25519_dalek::SigningKey;
use crate::crypto::{SessionManifest, SignedConsensusMessage, verify_hotstuff_qc};

use crate::{
    blockchain::{Block, Digest, Proposal},
    consensus::{CommitCertificate, Consensus, ConsensusAction, ConsensusEvent, ConsensusMessage, EvidenceStore},
    node::NodeId,
};

pub const DEFAULT_TIMEOUT_MS: u64 = 1_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuorumCertificate {
    pub view: u64,
    pub height: u64,
    pub phase: Phase,
    pub digest: Digest,
    pub voters: Vec<NodeId>,
    #[serde(default)]
    pub signatures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy, PartialEq, Hash, Eq)]
pub enum Phase {
    Prepare,
    PreCommit,
    Commit,
}

impl Phase {
    fn rank(self) -> u8 {
        match self {
            Phase::Prepare => 0,
            Phase::PreCommit => 1,
            Phase::Commit => 2,
        }
    }
}
fn qc_is_newer(a: &QuorumCertificate, b: &QuorumCertificate) -> bool {
    (a.height, a.view, a.phase.rank()) > (b.height, b.view, b.phase.rank())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HotStuffMessage {
    Prepare {
        view: u64,
        height: u64,
        proposal: Proposal,
        digest: Digest,
        justify: QuorumCertificate,
    },
    Vote {
        view: u64,
        height: u64,
        phase: Phase,
        digest: Digest,
    },
    PreCommit {
        view: u64,
        height: u64,
        digest: Digest,
        qc: QuorumCertificate,
    },
    Commit {
        view: u64,
        height: u64,
        digest: Digest,
        qc: QuorumCertificate,
    },
    Decide {
        view: u64,
        height: u64,
        digest: Digest,
        qc: QuorumCertificate,
    },
    NewView {
        view: u64,
        high_qc: QuorumCertificate,
    },
    StartView {
        view: u64,
        high_qc: QuorumCertificate,
        proofs: Vec<SignedConsensusMessage>,
    },
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Hash)]
pub struct VoteKey {
    view: u64,
    height: u64,
    phase: Phase,
    digest: Digest,
}

pub struct HotStuff {
    chain_id: Option<String>,
    evidence: EvidenceStore,
    node_id: NodeId,
    node_count: u64,
    view: u64,
    next_height: u64,
    last_committed_height: Option<u64>,

    //Highest/Latest certificate I know exists
    high_qc: QuorumCertificate,

    //Certificate that constrains what I'm allowed to vote for wihtout risking a conflicting decision
    locked_qc: QuorumCertificate,
    proposals: HashMap<Digest, Proposal>,
    votes: HashMap<VoteKey, HashSet<NodeId>>,
    formed_qcs: HashSet<VoteKey>,
    committed: HashSet<Digest>,
    committed_qcs: HashMap<Digest, QuorumCertificate>,
    new_view_message: HashMap<u64, HashMap<NodeId, QuorumCertificate>>,
    active_proposal: Option<Digest>,
    consensus_started: bool,
    timeout_ms: u64,
    view_ready: bool,
    voted: HashMap<(u64, u64, Phase), Digest>,
    armed_timer: Option<(u64, u64)>,
}

impl QuorumCertificate {
    pub fn genesis() -> Self {
        QuorumCertificate {
            view: 0,
            height: 0,
            phase: Phase::Commit,
            digest: [0; 32],
            voters: Vec::new(),
            signatures: Vec::new(),
        }
    }
}

impl HotStuff {
    pub fn new(node_id: NodeId, node_count: u64) -> Self {
        HotStuff {
            chain_id: None,
            evidence: EvidenceStore::default(),
            node_id,
            node_count,
            view: 0,
            next_height: 0,
            high_qc: QuorumCertificate::genesis(),
            locked_qc: QuorumCertificate::genesis(),
            proposals: HashMap::new(),
            votes: HashMap::new(),
            formed_qcs: HashSet::new(),
            new_view_message: HashMap::new(),
            active_proposal: None,
            consensus_started: false,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            view_ready: true,
            voted: HashMap::new(),
            last_committed_height: None,
            committed: HashSet::new(),
            committed_qcs: HashMap::new(),
            armed_timer: None,
        }
    }

    fn try_form_qc(&mut self, key: &VoteKey) -> Option<QuorumCertificate> {
        if self.formed_qcs.contains(key) {
            return None;
        }

        let voters = self.votes.get(key)?;

        if (voters.len() as u64) < self.quorum() {
            return None;
        }

        let mut ids: Vec<_> = voters.iter().copied().collect();
        ids.sort_unstable();
        let signatures = if self.evidence.manifest().is_some() {
            let example = ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
                view: key.view, height: key.height, phase: key.phase, digest: key.digest,
            });
            self.evidence.proofs(&example, &ids)?.into_iter().map(|proof| proof.signature).collect()
        } else { Vec::new() };
        let qc = QuorumCertificate {
            view: key.view,
            height: key.height,
            phase: key.phase,
            digest: key.digest,
            voters: ids,
            signatures,
        };

        self.formed_qcs.insert(key.clone());

        Some(qc)
    }

    fn valid_qc(&self, qc: &QuorumCertificate) -> bool {
        if qc == &QuorumCertificate::genesis() {
            return true;
        }
        if let Some(manifest) = self.evidence.manifest() {
            return verify_hotstuff_qc(qc, manifest).is_ok();
        }
        let unique_voters: HashSet<NodeId> = qc.voters.iter().copied().collect();
        if unique_voters.len() != qc.voters.len() {
            return false;
        }
        if (unique_voters.len() as u64) < self.quorum()
            || unique_voters.iter().any(|id| *id >= self.node_count) {
            return false;
        }

        true
    }
    fn vote_once(&mut self, view: u64, height: u64, phase: Phase, digest: Digest) -> bool {
        match self.voted.entry((view, height, phase)) {
            std::collections::hash_map::Entry::Vacant(entry) => { entry.insert(digest); true }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }
    fn timer(&mut self) -> Vec<ConsensusAction> {
        let key = (self.view, self.next_height);
        if self.armed_timer == Some(key) { return vec![]; }
        self.armed_timer = Some(key);
        vec![ConsensusAction::ScheduleTimeout {
            view: self.view, sequence: self.next_height,
            duration: Duration::from_millis(self.timeout_ms),
        }]
    }
    fn advance_view(&mut self, view: u64) -> Vec<ConsensusAction> {
        if view <= self.view { return vec![]; }
        self.view = view;
        self.armed_timer = None;
        self.view_ready = false;
        self.active_proposal = None;
        self.votes.clear();
        self.formed_qcs.clear();
        self.new_view_message.entry(view).or_default().insert(self.node_id, self.high_qc.clone());
        let mut actions = vec![
            ConsensusAction::Broadcast(ConsensusMessage::Hotstuff(HotStuffMessage::NewView {
                view, high_qc: self.high_qc.clone(),
            })),
        ];
        actions.extend(self.timer());
        actions
    }
    fn handle_new_view(&mut self, from: NodeId, view: u64, high_qc: QuorumCertificate) -> Vec<ConsensusAction> {
        if from >= self.node_count || view < self.view || !self.valid_qc(&high_qc) { return vec![]; }
        let mut actions = if view > self.view { self.advance_view(view) } else { vec![] };
        if self.node_id != self.leader_for_view(view) || self.view_ready { return actions; }
        self.new_view_message.entry(view).or_default().insert(from, high_qc);
        let votes = &self.new_view_message[&view];
        if votes.len() as u64 >= self.quorum() {
            let mut voters = votes.keys().copied().collect::<Vec<_>>();
            voters.sort_unstable();
            let proofs = if self.evidence.manifest().is_some() {
                let example = ConsensusMessage::Hotstuff(HotStuffMessage::NewView { view, high_qc: self.high_qc.clone() });
                let Some(proofs) = self.evidence.proofs(&example, &voters) else { return actions; };
                proofs
            } else { Vec::new() };
            let best = votes.values().max_by_key(|qc| (qc.height, qc.view, qc.phase.rank(), qc.digest)).cloned().unwrap();
            self.update_high_qc(best.clone());
            self.view_ready = true;
            actions.push(ConsensusAction::Broadcast(ConsensusMessage::Hotstuff(HotStuffMessage::StartView {
                view, high_qc: best.clone(),
                proofs,
            })));
            // If a phase certificate refers to this still-uncommitted height,
            // carry that proposal into the new view instead of rebuilding a
            // conflicting block from a different local mempool order.
            if best != QuorumCertificate::genesis() && best.height == self.next_height {
                if let Some(proposal) = self.proposals.get(&best.digest).cloned() {
                    actions.extend(self.propose(proposal.block));
                }
            }
        }
        actions
    }
    fn handle_start_view(&mut self, from: NodeId, view: u64, high_qc: QuorumCertificate, proofs: Vec<SignedConsensusMessage>) -> Vec<ConsensusAction> {
        if from != self.leader_for_view(view) || view < self.view || !self.valid_qc(&high_qc) { return vec![]; }
        if self.evidence.manifest().is_some() &&
            crate::crypto::verify_hotstuff_start_view(view, &high_qc, &proofs, self.evidence.manifest().unwrap()).is_err() {
            return vec![];
        }
        if view > self.view {
            self.view = view;
            self.active_proposal = None;
        }
        self.update_high_qc(high_qc);
        self.view_ready = true;
        self.timer()
    }
    fn update_high_qc(&mut self, qc: QuorumCertificate) {
        if qc_is_newer(&qc, &self.high_qc) {
            self.high_qc = qc;
        }
    }
    fn update_locked_qc(&mut self, qc: QuorumCertificate) {
        if qc_is_newer(&qc, &self.locked_qc) {
            self.locked_qc = qc;
        }
    }

    pub fn handle_prepare(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
        proposal: Proposal,
        justify: QuorumCertificate,
    ) -> Vec<ConsensusAction> {
        if view != self.view || !self.view_ready {
            eprintln!("Invalid view for Prepare");
            return vec![];
        }
        if from != self.leader_for_view(view) {
            eprintln!("Invalid sender for view");
            return vec![];
        }
        if height != self.next_height {
            eprintln!("invalid height for prepare");
            return vec![];
        }
        if proposal.to_digest() != digest {
            eprintln!("Invalid digest");
            return vec![];
        }

        if !self.valid_qc(&justify) {
            eprintln!("Invalid QC");
            return vec![];
        }
        if proposal.block.sequence != height || !self.safe_to_vote(&proposal, &justify) {
            return vec![];
        }
        if !self.vote_once(view, height, Phase::Prepare, digest) { return vec![]; }
        self.proposals.entry(digest).or_insert(proposal);
        let leader = self.leader_for_view(view);

        let mut actions = vec![
            ConsensusAction::Send {
                peer: leader,
                message: ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
                    view,
                    height: height,
                    phase: Phase::Prepare,
                    digest,
                }),
            },
        ];
        actions.extend(self.timer());
        actions
        // if justify
    }

    pub fn handle_prepare_vote(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
    ) -> Vec<ConsensusAction> {
        if view != self.view {
            eprintln!("Invalid view for prepare vote");
            return vec![];
        }
        if self.node_id != self.leader_for_view(view) {
            eprintln!("Invalid leader for prepare vote");
            return vec![];
        }
        if !self.proposals.contains_key(&digest) {
            eprintln!("Invalid digest in prepare vote");
            return vec![];
        }
        let key = VoteKey {
            view,
            phase: Phase::Prepare,
            height: height,
            digest,
        };

        if from >= self.node_count || height != self.next_height || self.active_proposal != Some(digest) { return vec![]; }
        self.votes.entry(key.clone()).or_default().insert(from);
        let Some(qc) = self.try_form_qc(&key) else {
            return vec![];
        };
        self.update_high_qc(qc.clone());

        let next_key = VoteKey {
            view,
            height,
            phase: Phase::PreCommit,
            digest,
        };

        self.votes.entry(next_key).or_default().insert(self.node_id);

        vec![ConsensusAction::Broadcast(ConsensusMessage::Hotstuff(
            HotStuffMessage::PreCommit {view,height,qc, digest },
        ))]
    }
    fn handle_pre_commit(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
        qc: QuorumCertificate,
    ) -> Vec<ConsensusAction> {
        if view != self.view {
            eprintln!("Invalid view in pre commit");
            return vec![];
        }
        if from != self.leader_for_view(view) {
            eprintln!("Invalid leader for view in precommit");
            return vec![];
        }
        if qc.phase != Phase::Prepare {
            return vec![];
        }
        if qc.view != view || qc.height != height || qc.digest != digest {
            eprintln!("error with QC in precommit");
            return vec![];
        }
        if !self.valid_qc(&qc) {
            eprintln!("Invalid QC in precommit");
            return vec![];
        }
        if !self.proposals.contains_key(&digest) {
            eprintln!("Unseen proposal in precommit");
            return vec![];
        }
        self.update_high_qc(qc);
        if !self.vote_once(view, height, Phase::PreCommit, digest) { return vec![]; }
        vec![ConsensusAction::Send {
            peer: self.leader_for_view(view),
            message: ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
                view,
                height,
                phase: Phase::PreCommit,
                digest,
            }),
        }]
    }

    fn handle_pre_commit_vote(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
    ) -> Vec<ConsensusAction> {
        if view != self.view {
            eprintln!("invalid view in pre commit vote");
            return vec![];
        }
        if self.node_id != self.leader_for_view(view) {
            eprintln!("Invalid recipient in pre commit vote");
            return vec![];
        }
        let key = VoteKey {
            view,
            height,
            phase: Phase::PreCommit,
            digest,
        };
        if from >= self.node_count || height != self.next_height || self.active_proposal != Some(digest) { return vec![]; }
        self.votes.entry(key.clone()).or_default().insert(from);
        let Some(qc) = self.try_form_qc(&key) else {
            return vec![];
        };
        self.update_high_qc(qc.clone());
        let next_key = VoteKey {
            view,
            height,
            phase: Phase::Commit,
            digest,
        };

        self.votes.entry(next_key).or_default().insert(self.node_id);
        vec![ConsensusAction::Broadcast(ConsensusMessage::Hotstuff(
            HotStuffMessage::Commit {view,qc,height, digest },
        ))]
    }

    fn handle_commit(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
        qc: QuorumCertificate,
    ) -> Vec<ConsensusAction> {
        if view != self.view {
            return vec![];
        }

        if from != self.leader_for_view(view) {
            return vec![];
        }

        if qc.phase != Phase::PreCommit {
            return vec![];
        }

        if qc.view != view || qc.height != height || qc.digest != digest {
            return vec![];
        }

        if !self.valid_qc(&qc) {
            return vec![];
        }

        if !self.proposals.contains_key(&digest) {
            return vec![];
        }

        // This is an important HotStuff step.
        self.update_locked_qc(qc.clone());

        self.update_high_qc(qc);
        if !self.vote_once(view, height, Phase::Commit, digest) { return vec![]; }

        vec![ConsensusAction::Send {
            peer: self.leader_for_view(view),

            message: ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
                view,
                height,
                phase: Phase::Commit,
                digest,
            }),
        }]
    }

    fn handle_commit_vote(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
    ) -> Vec<ConsensusAction> {
        if view != self.view {
            return vec![];
        }

        if self.node_id != self.leader_for_view(view) {
            return vec![];
        }

        let key = VoteKey {
            view,
            height,
            phase: Phase::Commit,
            digest,
        };

        if from >= self.node_count || height != self.next_height || self.active_proposal != Some(digest) { return vec![]; }
        self.votes.entry(key.clone()).or_default().insert(from);

        let Some(qc) = self.try_form_qc(&key) else {
            return vec![];
        };

        self.update_high_qc(qc.clone());

        let Some(proposal) = self.proposals.get(&digest).cloned() else {
            return vec![];
        };

        self.committed_qcs.insert(digest, qc.clone());
        let mut actions = vec![ConsensusAction::Broadcast(ConsensusMessage::Hotstuff(
            HotStuffMessage::Decide {
                view,
                height,
                digest,
                qc,
            },
        ))];

        if self.mark_committed(digest, height) {
            actions.push(ConsensusAction::Commit(proposal));
        }

        actions
    }

    fn handle_decide(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
        qc: QuorumCertificate,
    ) -> Vec<ConsensusAction> {
        if view > self.view || height != self.next_height || from != self.leader_for_view(view) {
            return vec![];
        }

        if qc.phase != Phase::Commit {
            return vec![];
        }

        if qc.view != view || qc.height != height || qc.digest != digest {
            return vec![];
        }

        if !self.valid_qc(&qc) {
            return vec![];
        }

        let Some(proposal) = self.proposals.get(&digest).cloned() else {
            return vec![];
        };

        self.committed_qcs.insert(digest, qc.clone());
        self.update_high_qc(qc);

        if !self.mark_committed(digest, height) {
            return vec![];
        }

        vec![ConsensusAction::Commit(proposal)]
    }

    fn mark_committed(&mut self, digest: Digest, height: u64) -> bool {
        if self.committed.contains(&digest) {
            return false;
        }

        self.committed.insert(digest);

        self.last_committed_height = Some(height);

        self.next_height = self.next_height.max(height + 1);
        self.armed_timer = None;

        self.active_proposal = None;
        self.proposals.clear();
        self.votes.retain(|key, _| key.height >= self.next_height);

        true
    }

    fn propose(&mut self, block: Block) -> Vec<ConsensusAction> {
        if !self.view_ready || self.node_id != self.current_primary() ||
            self.active_proposal.is_some() || block.sequence != self.next_height {
            return vec![];
        }
        let proposal = Proposal { block };
        let digest = proposal.to_digest();
        self.proposals.insert(digest, proposal.clone());
        self.active_proposal = Some(digest);
        self.votes.entry(VoteKey {
            view: self.view, height: self.next_height, phase: Phase::Prepare, digest,
        }).or_default().insert(self.node_id);
        let mut actions = vec![
            ConsensusAction::Broadcast(ConsensusMessage::Hotstuff(HotStuffMessage::Prepare {
                view: self.view, height: self.next_height, proposal, digest,
                justify: self.high_qc.clone(),
            })),
        ];
        actions.extend(self.timer());
        actions
    }

    fn safe_to_vote(&self, _proposal: &Proposal, _justify: &QuorumCertificate) -> bool {
        self.locked_qc == QuorumCertificate::genesis()
            || self.locked_qc.height < _proposal.block.sequence
            || self.locked_qc.digest == _proposal.to_digest()
            || _justify.view > self.locked_qc.view
    }
}

impl Consensus for HotStuff {
    fn set_chain_id(&mut self, chain_id: String) { self.chain_id = Some(chain_id); }
    fn set_signer(&mut self, manifest: Arc<SessionManifest>, key: SigningKey) {
        self.evidence.set_signer(manifest, key, self.node_id);
    }
    fn record_verified(&mut self, message: SignedConsensusMessage) { self.evidence.record(message); }
    fn commit_certificate(&mut self, block: &Block) -> Option<CommitCertificate> {
        let qc = self.committed_qcs.get(&block.to_digest())?.clone();
        Some(CommitCertificate::Hotstuff { qc })
    }
    fn f(&self) -> u64 {
        (self.node_count - 1) / 3
    }
    fn quorum(&self) -> u64 {
        crate::consensus::quorum_size(self.node_count)
    }

    fn leader_for_view(&self, view: u64) -> NodeId {
        view % self.node_count.max(1)
    }

    fn set_node_count(&mut self, count: u64) {
        self.node_count = count;
    }

    fn restore_from_height(&mut self, height: u64) {
        self.next_height = height;
        self.last_committed_height = height.checked_sub(1);
        self.armed_timer = None;
    }

    fn catch_up_to_height(&mut self, height: u64) -> Vec<ConsensusAction> {
        if height <= self.next_height { return vec![]; }
        self.next_height = height;
        self.last_committed_height = height.checked_sub(1);
        self.armed_timer = None;
        self.active_proposal = None;
        self.proposals.clear();
        self.votes.clear();
        self.formed_qcs.clear();
        // Keep voted/locked QC: forgetting either could permit an unsafe vote
        // in the current view. A later view change will carry the high QC.
        self.timer()
    }

    fn supports_catch_up(&self) -> bool { true }

    fn on_start(&mut self) -> Vec<super::ConsensusAction> {
        if self.next_height == 0 && self.node_id == self.current_primary() {
            self.propose(self.evidence.manifest().map(|manifest| Block::new_initial_block_signed(
                &manifest.chain_id,
                crate::message::hex_decode(&manifest.initial_state_root).expect("validated initial state root"),
            )).unwrap_or_else(Block::new_initial_block))
        } else if self.node_id != self.current_primary() {
            self.timer()
        } else {
            vec![]
        }
    }

    fn on_event(&mut self, event: super::ConsensusEvent) -> Vec<ConsensusAction> {
        match event {
            ConsensusEvent::Message { from, message } => match message {
                ConsensusMessage::Pbft(_) => {
                    eprintln!("WRONG PROTOCOL");
                    vec![]
                }
                ConsensusMessage::Hotstuff(hot_stuff_message) => match hot_stuff_message {
                    HotStuffMessage::Prepare {
                        view,
                        proposal,
                        justify,
                        height,
                        digest,
                    } => self.handle_prepare(from, view, height, digest, proposal, justify),
                    HotStuffMessage::Vote {
                        view,
                        phase,
                        digest,
                        height,
                    } => match phase {
                        Phase::Prepare => self.handle_prepare_vote(from, view, height, digest),
                        Phase::PreCommit => self.handle_pre_commit_vote(from, view, height, digest),
                        Phase::Commit => self.handle_commit_vote(from, view, height, digest),
                    },
                    HotStuffMessage::PreCommit { view, qc, height, digest } => self.handle_pre_commit(from, view, height, digest, qc),
                    HotStuffMessage::Commit { view, qc, height, digest } => self.handle_commit(from, view, height, digest, qc),
                    HotStuffMessage::Decide {
                        view,
                        qc,
                        height,
                        digest,
                    } => self.handle_decide(from, view, height, digest, qc),
                    HotStuffMessage::NewView { view, high_qc } => self.handle_new_view(from, view, high_qc),
                    HotStuffMessage::StartView { view, high_qc, proofs } => self.handle_start_view(from, view, high_qc, proofs),
                },
            },
            ConsensusEvent::Timeout { view, sequence } => {
                if view == self.view && sequence == self.next_height {
                    self.armed_timer = None;
                    self.advance_view(view + 1)
                } else { vec![] }
            }
            ConsensusEvent::LeaderUnavailable => self.advance_view(self.view + 1),
            ConsensusEvent::TransactionAvailable => {
                if self.node_id == self.current_primary() { vec![] } else { self.timer() }
            }
            ConsensusEvent::Proposal(block) => self.propose(block),
        }
    }

    fn prepare_quorum(&self) -> u64 {
        self.quorum()
    }

    fn commit_quorum(&self) -> u64 {
        self.quorum()
    }

    fn can_propose(&self) -> bool {
        self.consensus_started && self.view_ready && self.leader_for_view(self.view) == self.node_id
            && self.active_proposal.is_none()
    }

    fn consensus_started(&mut self, turn_on: bool) -> bool {
        if turn_on {
            self.consensus_started = true;
        }
        self.consensus_started
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
}
