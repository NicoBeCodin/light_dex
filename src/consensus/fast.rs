//! Research two-round engines. HotStuff-2 uses a prepare/final vote pair;
//! Votor uses a 60% notarize/final pair and an 80% notarize fast path.
//! Certificates, rather than local vote counts, are the only commit authority.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use super::{
    CommitCertificate, Consensus, ConsensusAction, ConsensusEvent, ConsensusMessage, EvidenceStore,
};
use crate::{
    blockchain::{Block, Digest, Proposal},
    crypto::{SessionManifest, SignedConsensusMessage, verify_fast_qc, verify_fast_start_view},
    node::NodeId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastMode {
    Hotstuff2,
    Votor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VoteKind {
    Prepare,
    Notarize,
    Final,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FastQc {
    pub view: u64,
    pub height: u64,
    pub digest: Digest,
    pub kind: VoteKind,
    pub voters: Vec<NodeId>,
    pub signatures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FastMessage {
    Proposal {
        view: u64,
        height: u64,
        proposal: Proposal,
        justify: Option<FastQc>,
    },
    CompactProposal {
        view: u64,
        height: u64,
        digest: Digest,
        compact: super::chained::CompactProposal,
        justify: Option<FastQc>,
    },
    ProposalRequest {
        view: u64,
        height: u64,
        digest: Digest,
    },
    /// A durable final certificate for the preceding height permits the
    /// deterministic four-block handoff without a NewView quorum. Node checks
    /// that its durable predecessor has this digest before passing it here.
    RotationNotice {
        view: u64,
        height: u64,
        previous: FastQc,
    },
    Vote {
        view: u64,
        height: u64,
        digest: Digest,
        kind: VoteKind,
    },
    Certified {
        qc: FastQc,
    },
    NewView {
        view: u64,
        height: u64,
        high_qc: Option<FastQc>,
        proposal: Option<Proposal>,
    },
    StartView {
        view: u64,
        height: u64,
        high_qc: Option<FastQc>,
        proposal: Option<Proposal>,
        proofs: Vec<SignedConsensusMessage>,
    },
}

#[derive(Clone)]
pub enum FastProof {
    Hotstuff2 {
        prepare: FastQc,
        finality: FastQc,
    },
    Votor {
        notar: Option<FastQc>,
        finality: FastQc,
    },
}

pub struct FastConsensus {
    mode: FastMode,
    // Let every HotStuff2 replica collect authenticated votes. A QC formed
    // near the submitting client can be relayed while the proposer waits on
    // the WAN. The leader-only collector remains available for A/B tests.
    gossip_votes: bool,
    certificate_handoff: bool,
    automatic_rotation_interval: Option<u64>,
    rotation_qc: Option<FastQc>,
    node_id: NodeId,
    node_count: u64,
    view: u64,
    height: u64,
    timeout_ms: u64,
    started: bool,
    ready: bool,
    active: Option<Digest>,
    armed: Option<(u64, u64)>,
    proposals: HashMap<Digest, Proposal>,
    pending_proposals: HashMap<(u64, u64), (NodeId, Proposal, Option<FastQc>)>,
    high_qc: Option<FastQc>,
    locked_qc: Option<FastQc>,
    votes: HashMap<(u64, u64, VoteKind, Digest), HashSet<NodeId>>,
    voted: HashMap<(u64, u64, VoteKind), Digest>,
    // Until a height is finalized, a Votor first-round vote constrains every
    // later view. Otherwise a delayed 80% certificate could conflict with a
    // different block decided after a view change.
    votor_choice: HashMap<u64, Digest>,
    certified: HashSet<(u64, u64, VoteKind, Digest, usize)>,
    new_views: HashMap<u64, BTreeMap<NodeId, (Option<FastQc>, Option<Proposal>)>>,
    proof: HashMap<Digest, FastProof>,
    committed: HashSet<Digest>,
    pending_qcs: HashMap<Digest, Vec<FastQc>>,
    evidence: EvidenceStore,
}

impl FastConsensus {
    fn trace_stage(&self, stage: &'static str, height: u64, digest: Digest) {
        if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_none() {
            return;
        }
        let unix_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        eprintln!(
            "FAST_STAGE node={} view={} height={} stage={} digest={} unix_us={}",
            self.node_id,
            self.view,
            height,
            stage,
            crate::message::hex_encode(&digest),
            unix_us
        );
    }
    fn trace_vote(
        &self,
        event: &'static str,
        view: u64,
        height: u64,
        digest: Digest,
        kind: VoteKind,
        voter: NodeId,
    ) {
        if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_none() {
            return;
        }
        let unix_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        eprintln!(
            "FAST_VOTE node={} view={} height={} event={} kind={kind:?} voter={} digest={} unix_us={}",
            self.node_id,
            view,
            height,
            event,
            voter,
            crate::message::hex_encode(&digest),
            unix_us
        );
    }
    pub fn new(mode: FastMode, node_id: NodeId, node_count: u64) -> Self {
        Self {
            mode,
            gossip_votes: mode == FastMode::Hotstuff2
                && std::env::var_os("LIGHT_DEX_FAST_LEADER_COLLECTOR").is_none(),
            certificate_handoff: mode == FastMode::Hotstuff2
                && std::env::var_os("LIGHT_DEX_FAST_CERT_HANDOFF").is_some(),
            automatic_rotation_interval: None,
            rotation_qc: None,
            node_id,
            node_count,
            view: 0,
            height: 0,
            timeout_ms: 1_000,
            started: false,
            ready: true,
            active: None,
            armed: None,
            proposals: HashMap::new(),
            pending_proposals: HashMap::new(),
            high_qc: None,
            locked_qc: None,
            votes: HashMap::new(),
            voted: HashMap::new(),
            votor_choice: HashMap::new(),
            certified: HashSet::new(),
            new_views: HashMap::new(),
            proof: HashMap::new(),
            committed: HashSet::new(),
            pending_qcs: HashMap::new(),
            evidence: EvidenceStore::default(),
        }
    }

    fn message(&self, message: FastMessage) -> ConsensusMessage {
        match self.mode {
            FastMode::Hotstuff2 => ConsensusMessage::Hotstuff2(message),
            FastMode::Votor => ConsensusMessage::Votor(message),
        }
    }

    fn threshold(&self, kind: VoteKind) -> u64 {
        if self.mode == FastMode::Votor && kind != VoteKind::Skip {
            (self.node_count * 3 + 4) / 5
        } else if self.mode == FastMode::Votor {
            (self.node_count * 3 + 4) / 5
        } else {
            super::quorum_size(self.node_count)
        }
    }

    fn fast_threshold(&self) -> u64 {
        (self.node_count * 4 + 4) / 5
    }

    fn timer(&mut self) -> Vec<ConsensusAction> {
        let key = (self.view, self.height);
        if self.armed == Some(key) {
            return vec![];
        }
        self.armed = Some(key);
        vec![ConsensusAction::ScheduleTimeout {
            view: key.0,
            sequence: key.1,
            duration: Duration::from_millis(self.timeout_ms),
        }]
    }

    fn valid_qc(&self, qc: &FastQc) -> bool {
        if let Some(manifest) = self.evidence.manifest() {
            verify_fast_qc(qc, manifest, self.mode).is_ok()
        } else {
            qc.voters.len() == qc.signatures.len()
                && qc.voters.len() as u64 >= self.threshold(qc.kind)
                && qc.voters.windows(2).all(|ids| ids[0] < ids[1])
        }
    }

    fn high_qc(&mut self, qc: FastQc) {
        if self
            .high_qc
            .as_ref()
            .is_none_or(|old| (qc.height, qc.view) > (old.height, old.view))
        {
            self.high_qc = Some(qc);
        }
    }

    fn vote(&mut self, kind: VoteKind, digest: Digest) -> Vec<ConsensusAction> {
        if self.mode == FastMode::Votor && kind == VoteKind::Notarize {
            if self
                .votor_choice
                .get(&self.height)
                .is_some_and(|previous| *previous != digest)
            {
                return vec![];
            }
            self.votor_choice.insert(self.height, digest);
        }
        let key = (self.view, self.height, kind);
        if let Some(previous) = self.voted.get(&key) {
            if *previous == digest {
                return vec![];
            }
            return vec![];
        }
        if kind == VoteKind::Skip
            && self
                .voted
                .contains_key(&(self.view, self.height, VoteKind::Notarize))
        {
            return vec![];
        }
        if kind == VoteKind::Notarize
            && self
                .voted
                .contains_key(&(self.view, self.height, VoteKind::Skip))
        {
            return vec![];
        }
        self.voted.insert(key, digest);
        self.votes
            .entry((self.view, self.height, kind, digest))
            .or_default()
            .insert(self.node_id);
        self.trace_vote("issued", self.view, self.height, digest, kind, self.node_id);
        let message = self.message(FastMessage::Vote {
            view: self.view,
            height: self.height,
            digest,
            kind,
        });
        if self.mode == FastMode::Votor || self.gossip_votes {
            vec![ConsensusAction::Broadcast(message)]
        } else if self.node_id == self.current_primary() {
            // The QC collector signs its own vote when building the proof.
            vec![]
        } else {
            vec![ConsensusAction::Send {
                peer: self.current_primary(),
                message,
            }]
        }
    }

    fn form_qc(
        &mut self,
        kind: VoteKind,
        view: u64,
        height: u64,
        digest: Digest,
        needed: u64,
    ) -> Option<FastQc> {
        let key = (view, height, kind, digest);
        let voters = self.votes.get(&key)?;
        if (voters.len() as u64) < needed
            || !self
                .certified
                .insert((view, height, kind, digest, needed as usize))
        {
            return None;
        }
        let mut voters = voters.iter().copied().collect::<Vec<_>>();
        voters.sort_unstable();
        let signatures = if self.evidence.manifest().is_some() {
            let message = self.message(FastMessage::Vote {
                view,
                height,
                digest,
                kind,
            });
            self.evidence
                .proofs(&message, &voters)?
                .into_iter()
                .map(|proof| proof.signature)
                .collect()
        } else {
            vec![String::new(); voters.len()]
        };
        self.trace_stage(
            match kind {
                VoteKind::Prepare => "prepare_qc_formed",
                VoteKind::Notarize => "notarize_qc_formed",
                VoteKind::Final => "final_qc_formed",
                VoteKind::Skip => "skip_qc_formed",
            },
            height,
            digest,
        );
        Some(FastQc {
            view,
            height,
            digest,
            kind,
            voters,
            signatures,
        })
    }

    fn commit(&mut self, digest: Digest, proof: FastProof) -> Vec<ConsensusAction> {
        if self.committed.contains(&digest) {
            return vec![];
        }
        let Some(proposal) = self.proposals.get(&digest).cloned() else {
            return vec![];
        };
        if proposal.block.sequence != self.height {
            return vec![];
        }
        let rotation_qc = match (&proof, self.automatic_rotation_interval) {
            (FastProof::Hotstuff2 { finality, .. }, Some(interval))
                if self.certificate_handoff
                    && interval > 0
                    && self.height > 0
                    && self.height % interval == 0 =>
            {
                Some(finality.clone())
            }
            _ => None,
        };
        self.trace_stage("commit_decided", self.height, digest);
        self.proof.insert(digest, proof);
        self.committed.insert(digest);
        self.height += 1;
        self.rotation_qc = rotation_qc;
        // A leader must not start the next height in the old view while the
        // certified rotation waits for this block's durable journal write.
        if self.rotation_qc.is_some() {
            self.ready = false;
        }
        self.votor_choice.remove(&(self.height - 1));
        self.active = None;
        self.armed = None;
        self.proposals.clear();
        self.votes.clear();
        self.voted
            .retain(|(_, height, _), _| *height >= self.height);
        self.certified.clear();
        self.pending_qcs.clear();
        self.committed.clear();
        self.new_views.retain(|view, _| *view >= self.view);
        self.evidence.prune_fast_before(self.height, self.view);
        self.high_qc = None;
        self.locked_qc = None;
        self.pending_proposals
            .retain(|(_, height), _| *height >= self.height);
        let mut actions = vec![ConsensusAction::Commit(proposal)];
        if self.ready {
            actions.push(ConsensusAction::TryPropose);
        }
        actions
    }

    fn on_certificate(&mut self, qc: FastQc) -> Vec<ConsensusAction> {
        if !self.valid_qc(&qc) || qc.height != self.height || qc.view != self.view {
            return vec![];
        }
        self.trace_stage(
            match qc.kind {
                VoteKind::Prepare => "prepare_qc_received",
                VoteKind::Notarize => "notarize_qc_received",
                VoteKind::Final => "final_qc_received",
                VoteKind::Skip => "skip_qc_received",
            },
            qc.height,
            qc.digest,
        );
        if qc.kind != VoteKind::Skip && !self.proposals.contains_key(&qc.digest) {
            self.defer_qc(qc);
            return vec![];
        }
        match qc.kind {
            VoteKind::Prepare | VoteKind::Notarize => {
                if !self.proposals.contains_key(&qc.digest) {
                    return vec![];
                }
                self.high_qc(qc.clone());
                self.locked_qc = Some(qc.clone());
                let mut actions = Vec::new();
                if self.mode == FastMode::Votor
                    && qc.kind == VoteKind::Notarize
                    && qc.voters.len() as u64 >= self.fast_threshold()
                {
                    actions.extend(self.commit(
                        qc.digest,
                        FastProof::Votor {
                            notar: None,
                            finality: qc.clone(),
                        },
                    ));
                } else {
                    actions.extend(self.vote(VoteKind::Final, qc.digest));
                    if let Some(pending) = self.pending_qcs.remove(&qc.digest) {
                        for delayed in pending {
                            actions.extend(self.on_certificate(delayed));
                        }
                    }
                }
                actions
            }
            VoteKind::Final => {
                let notar = self
                    .locked_qc
                    .as_ref()
                    .filter(|locked| locked.digest == qc.digest && locked.height == qc.height)
                    .cloned();
                // A final certificate can arrive before the first-round
                // certificate on a different network path. Keep it until
                // the matching lock is available instead of losing finality.
                let Some(notar) = notar else {
                    self.defer_qc(qc);
                    return vec![];
                };
                let proof = match self.mode {
                    FastMode::Hotstuff2 => FastProof::Hotstuff2 {
                        prepare: notar,
                        finality: qc.clone(),
                    },
                    FastMode::Votor => FastProof::Votor {
                        notar: Some(notar),
                        finality: qc.clone(),
                    },
                };
                self.commit(qc.digest, proof)
            }
            VoteKind::Skip if self.mode == FastMode::Votor => self.advance_view(qc.view + 1),
            _ => vec![],
        }
    }

    fn defer_qc(&mut self, qc: FastQc) {
        let pending = self.pending_qcs.entry(qc.digest).or_default();
        if let Some(old) = pending
            .iter_mut()
            .find(|old| old.kind == qc.kind && old.view == qc.view && old.height == qc.height)
        {
            if qc.voters.len() > old.voters.len() {
                *old = qc;
            }
            return;
        }
        // There are only two useful phase certificates for one digest in one
        // view. Repeated authenticated retransmissions must not grow storage.
        if pending.len() < 2 {
            pending.push(qc);
        }
    }

    fn on_vote(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        digest: Digest,
        kind: VoteKind,
    ) -> Vec<ConsensusAction> {
        if from >= self.node_count || view != self.view || height != self.height {
            return vec![];
        }
        if self.mode == FastMode::Hotstuff2
            && !self.gossip_votes
            && self.node_id != self.current_primary()
        {
            return vec![];
        }
        if kind == VoteKind::Skip && self.mode != FastMode::Votor {
            return vec![];
        }
        let inserted = self
            .votes
            .entry((view, height, kind, digest))
            .or_default()
            .insert(from);
        if inserted {
            self.trace_vote("received", view, height, digest, kind, from);
        }
        if kind != VoteKind::Skip && !self.proposals.contains_key(&digest) {
            return vec![];
        }
        let mut actions = Vec::new();
        if self.mode == FastMode::Votor && kind == VoteKind::Notarize {
            if let Some(qc) = self.form_qc(kind, view, height, digest, self.fast_threshold()) {
                actions.push(ConsensusAction::Broadcast(
                    self.message(FastMessage::Certified { qc: qc.clone() }),
                ));
                actions.extend(self.on_certificate(qc));
                return actions;
            }
        }
        if let Some(qc) = self.form_qc(kind, view, height, digest, self.threshold(kind)) {
            actions.push(ConsensusAction::Broadcast(
                self.message(FastMessage::Certified { qc: qc.clone() }),
            ));
            actions.extend(self.on_certificate(qc));
        }
        actions
    }

    fn propose(&mut self, block: Block) -> Vec<ConsensusAction> {
        if !self.can_propose() || block.sequence != self.height {
            return vec![];
        }
        if let Some(lock) = &self.locked_qc {
            if lock.height == self.height && lock.digest != block.to_digest() {
                return vec![];
            }
        }
        if self.mode == FastMode::Votor
            && self
                .votor_choice
                .get(&self.height)
                .is_some_and(|choice| *choice != block.to_digest())
        {
            return vec![];
        }
        let proposal = Proposal { block };
        let digest = proposal.to_digest();
        self.trace_stage("proposal_sent", self.height, digest);
        self.proposals.insert(digest, proposal.clone());
        self.active = Some(digest);
        let data =
            if self.mode == FastMode::Hotstuff2 && block_is_compact_candidate(&proposal.block) {
                FastMessage::CompactProposal {
                    view: self.view,
                    height: self.height,
                    digest,
                    compact: super::chained::CompactProposal::from_block(&proposal.block),
                    justify: self.high_qc.clone(),
                }
            } else {
                FastMessage::Proposal {
                    view: self.view,
                    height: self.height,
                    proposal,
                    justify: self.high_qc.clone(),
                }
            };
        let mut actions = vec![ConsensusAction::Broadcast(self.message(data))];
        actions.extend(self.vote(self.first_kind(), digest));
        actions.extend(self.timer());
        actions
    }

    fn first_kind(&self) -> VoteKind {
        match self.mode {
            FastMode::Hotstuff2 => VoteKind::Prepare,
            FastMode::Votor => VoteKind::Notarize,
        }
    }

    fn on_proposal(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        proposal: Proposal,
        justify: Option<FastQc>,
    ) -> Vec<ConsensusAction> {
        if height != self.height
            || proposal.block.sequence != height
            || from != self.leader_for_view(view)
            || view < self.view
        {
            return vec![];
        }
        if view > self.view || !self.ready {
            if self.pending_proposals.len() < 16
                || self.pending_proposals.contains_key(&(view, height))
            {
                self.pending_proposals
                    .insert((view, height), (from, proposal, justify));
            }
            return vec![];
        }
        if justify
            .as_ref()
            .is_some_and(|qc| !self.valid_qc(qc) || qc.height > height)
        {
            return vec![];
        }
        let digest = proposal.to_digest();
        self.trace_stage("proposal_received", height, digest);
        if self
            .locked_qc
            .as_ref()
            .is_some_and(|qc| qc.height == height && qc.digest != digest)
        {
            return vec![];
        }
        if self.mode == FastMode::Votor
            && self
                .votor_choice
                .get(&height)
                .is_some_and(|choice| *choice != digest)
        {
            return vec![];
        }
        self.proposals.entry(digest).or_insert(proposal);
        let mut actions = self.vote(self.first_kind(), digest);
        let mut pending = self.pending_qcs.remove(&digest).unwrap_or_default();
        pending.sort_by_key(|qc| qc.kind == VoteKind::Final);
        for qc in pending {
            actions.extend(self.on_certificate(qc));
        }
        let first = self.first_kind();
        if let Some(qc) = self.form_qc(first, view, height, digest, self.threshold(first)) {
            actions.push(ConsensusAction::Broadcast(
                self.message(FastMessage::Certified { qc: qc.clone() }),
            ));
            actions.extend(self.on_certificate(qc));
        }
        actions.extend(self.timer());
        actions
    }

    fn advance_view(&mut self, view: u64) -> Vec<ConsensusAction> {
        if view <= self.view {
            return vec![];
        }
        self.rotation_qc = None;
        super::hotstuff::handoff_trace(self.node_id, view, "new_view_sent");
        self.view = view;
        self.ready = false;
        self.active = None;
        self.armed = None;
        self.pending_proposals
            .retain(|(candidate_view, height), _| {
                *candidate_view >= view && *height == self.height
            });
        self.new_views
            .retain(|candidate_view, _| *candidate_view >= view);
        self.votes.retain(|(candidate_view, height, _, _), _| {
            *candidate_view >= view && *height == self.height
        });
        self.voted.retain(|(candidate_view, height, _), _| {
            *candidate_view >= view && *height == self.height
        });
        self.certified.retain(|(candidate_view, height, _, _, _)| {
            *candidate_view >= view && *height == self.height
        });
        self.pending_qcs.retain(|_, qcs| {
            qcs.retain(|qc| qc.view >= view && qc.height == self.height);
            !qcs.is_empty()
        });
        self.evidence.prune_fast_before(self.height, view);
        let proposal = self
            .high_qc
            .as_ref()
            .map(|qc| qc.digest)
            .or_else(|| {
                (self.mode == FastMode::Votor)
                    .then(|| self.votor_choice.get(&self.height).copied())
                    .flatten()
            })
            .and_then(|digest| self.proposals.get(&digest).cloned());
        self.new_views
            .entry(view)
            .or_default()
            .insert(self.node_id, (self.high_qc.clone(), proposal.clone()));
        let mut actions = vec![ConsensusAction::Broadcast(self.message(
            FastMessage::NewView {
                view,
                height: self.height,
                high_qc: self.high_qc.clone(),
                proposal,
            },
        ))];
        actions.extend(self.timer());
        actions
    }

    fn certified_rotation(
        &mut self,
        committed_view: u64,
        next_sequence: u64,
    ) -> Option<Vec<ConsensusAction>> {
        if !self.certificate_handoff
            || self.mode != FastMode::Hotstuff2
            || self.view != committed_view
            || self.height != next_sequence
        {
            return None;
        }
        let qc = self.rotation_qc.take()?;
        if qc.kind != VoteKind::Final
            || qc.view != committed_view
            || qc.height.checked_add(1) != Some(next_sequence)
            || !self.valid_qc(&qc)
        {
            return None;
        }
        // The local durable block and its signed final certificate are the
        // handoff proof. This path is used only at the configured rotation
        // boundary; a timeout still uses the NewView quorum protocol.
        let mut actions = self.activate_certified_view(committed_view.checked_add(1)?);
        actions.push(ConsensusAction::Broadcast(self.message(
            FastMessage::RotationNotice {
                view: self.view,
                height: self.height,
                previous: qc,
            },
        )));
        Some(actions)
    }

    fn activate_certified_view(&mut self, view: u64) -> Vec<ConsensusAction> {
        self.view = view;
        self.ready = true;
        self.active = None;
        self.armed = None;
        self.new_views.retain(|candidate, _| *candidate >= view);
        self.votes
            .retain(|(candidate, height, _, _), _| *candidate >= view && *height == self.height);
        self.voted
            .retain(|(candidate, height, _), _| *candidate >= view && *height == self.height);
        self.evidence.prune_fast_before(self.height, view);
        super::hotstuff::handoff_trace(self.node_id, view, "certified_rotation");
        let mut actions = self.timer();
        if let Some((from, pending, justify)) =
            self.pending_proposals.remove(&(self.view, self.height))
        {
            actions.extend(self.on_proposal(from, self.view, self.height, pending, justify));
        }
        if self.node_id == self.current_primary() {
            actions.push(ConsensusAction::TryPropose);
        }
        actions
    }

    fn on_rotation_notice(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        previous: FastQc,
    ) -> Vec<ConsensusAction> {
        if !self.certificate_handoff
            || self.mode != FastMode::Hotstuff2
            || from >= self.node_count
            || view <= self.view
            || previous.view.checked_add(1) != Some(view)
            || height != self.height
            || previous.height.checked_add(1) != Some(height)
            || !self.automatic_rotation_interval.is_some_and(|interval| {
                interval > 0 && previous.height > 0 && previous.height % interval == 0
            })
            || previous.kind != VoteKind::Final
            || !self.valid_qc(&previous)
        {
            return vec![];
        }
        self.rotation_qc = None;
        self.activate_certified_view(view)
    }

    fn on_new_view(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        high_qc: Option<FastQc>,
        proposal: Option<Proposal>,
    ) -> Vec<ConsensusAction> {
        if from >= self.node_count
            || height != self.height
            || view < self.view
            || high_qc
                .as_ref()
                .is_some_and(|qc| !self.valid_qc(qc) || qc.height > height)
            || proposal.as_ref().is_some_and(|body| {
                body.block.sequence != height
                    || high_qc
                        .as_ref()
                        .is_some_and(|qc| qc.digest != body.to_digest())
                    || high_qc.is_none() && self.mode != FastMode::Votor
            })
        {
            return vec![];
        }
        let mut actions = if view > self.view {
            self.advance_view(view)
        } else {
            vec![]
        };
        if self.node_id != self.leader_for_view(view) || self.ready {
            return actions;
        }
        self.new_views
            .entry(view)
            .or_default()
            .insert(from, (high_qc, proposal));
        let messages = &self.new_views[&view];
        if messages.len() < self.quorum() as usize {
            return actions;
        }
        super::hotstuff::handoff_trace(self.node_id, view, "new_view_quorum");
        let mut signers = messages.keys().copied().collect::<Vec<_>>();
        signers.sort_unstable();
        let example = self.message(FastMessage::NewView {
            view,
            height,
            high_qc: self.high_qc.clone(),
            proposal: self
                .high_qc
                .as_ref()
                .map(|qc| qc.digest)
                .or_else(|| self.votor_choice.get(&height).copied())
                .and_then(|digest| self.proposals.get(&digest).cloned()),
        });
        let Some(proofs) = self.evidence.proofs(&example, &signers) else {
            return actions;
        };
        let best = messages
            .values()
            .filter_map(|(qc, _)| qc.clone())
            .max_by_key(|qc| (qc.height, qc.view));
        let candidate = if let Some(qc) = &best {
            messages
                .values()
                .filter_map(|(_, proposal)| proposal.as_ref())
                .find(|proposal| proposal.to_digest() == qc.digest)
                .cloned()
        } else if self.mode == FastMode::Votor {
            let mut counts = BTreeMap::<Digest, (usize, Proposal)>::new();
            for (_, proposal) in messages.values() {
                if let Some(proposal) = proposal {
                    let entry = counts
                        .entry(proposal.to_digest())
                        .or_insert((0, proposal.clone()));
                    entry.0 += 1;
                }
            }
            counts
                .into_iter()
                .max_by_key(|(digest, (count, _))| (*count, *digest))
                .map(|(_, (_, body))| body)
        } else {
            None
        };
        if best.as_ref().is_some_and(|qc| qc.height == height) && candidate.is_none() {
            return actions;
        }
        if let Some(qc) = &best {
            self.high_qc(qc.clone());
            self.locked_qc = Some(qc.clone());
        }
        self.ready = true;
        super::hotstuff::handoff_trace(self.node_id, view, "start_view_sent");
        actions.push(ConsensusAction::Broadcast(self.message(
            FastMessage::StartView {
                view,
                height,
                high_qc: best,
                proposal: candidate.clone(),
                proofs,
            },
        )));
        if let Some(candidate) = candidate {
            self.proposals
                .insert(candidate.to_digest(), candidate.clone());
            actions.extend(self.propose(candidate.block));
        } else {
            actions.push(ConsensusAction::TryPropose);
        }
        actions
    }

    fn on_start_view(
        &mut self,
        from: NodeId,
        view: u64,
        height: u64,
        high_qc: Option<FastQc>,
        proposal: Option<Proposal>,
        proofs: Vec<SignedConsensusMessage>,
    ) -> Vec<ConsensusAction> {
        if from != self.leader_for_view(view) || view < self.view || height != self.height {
            return vec![];
        }
        if let Some(manifest) = self.evidence.manifest() {
            if verify_fast_start_view(
                view,
                height,
                &high_qc,
                proposal.as_ref(),
                &proofs,
                manifest,
                self.mode,
            )
            .is_err()
            {
                return vec![];
            }
        }
        self.view = view;
        self.ready = true;
        super::hotstuff::handoff_trace(self.node_id, view, "start_view_received");
        self.armed = None;
        self.high_qc = high_qc.clone();
        self.locked_qc = high_qc;
        let candidate = proposal.as_ref().map(Proposal::to_digest);
        if let Some(proposal) = proposal {
            self.proposals.insert(proposal.to_digest(), proposal);
        }
        let mut actions = self.timer();
        if let Some((from, pending, justify)) = self.pending_proposals.remove(&(view, height)) {
            actions.extend(self.on_proposal(from, view, height, pending, justify));
        } else if let Some(candidate) = candidate {
            actions.extend(self.vote(self.first_kind(), candidate));
        }
        if self.node_id == self.current_primary() {
            actions.push(ConsensusAction::TryPropose);
        }
        actions
    }
}

impl Consensus for FastConsensus {
    fn set_signer(&mut self, manifest: Arc<SessionManifest>, key: SigningKey) {
        self.automatic_rotation_interval = manifest.auto_view_change_blocks;
        self.evidence.set_signer(manifest, key, self.node_id);
    }
    fn record_verified(&mut self, message: SignedConsensusMessage) {
        self.evidence.record(message);
    }
    fn commit_certificate(&mut self, block: &Block) -> Option<CommitCertificate> {
        match self.proof.remove(&block.to_digest())? {
            FastProof::Hotstuff2 { prepare, finality } => {
                Some(CommitCertificate::Hotstuff2 { prepare, finality })
            }
            FastProof::Votor { notar, finality } => {
                Some(CommitCertificate::Votor { notar, finality })
            }
        }
    }
    fn set_node_count(&mut self, count: u64) {
        self.node_count = count;
    }
    fn restore_from_height(&mut self, height: u64) {
        self.height = height;
        self.armed = None;
        self.rotation_qc = None;
    }
    fn catch_up_to_height(&mut self, height: u64) -> Vec<ConsensusAction> {
        if height <= self.height {
            return vec![];
        }
        self.height = height;
        self.rotation_qc = None;
        self.armed = None;
        self.active = None;
        self.proposals.clear();
        self.votes.clear();
        self.voted.retain(|(_, sequence, _), _| *sequence >= height);
        self.certified.clear();
        self.pending_qcs.clear();
        self.pending_proposals
            .retain(|(_, sequence), _| *sequence >= height);
        self.votor_choice.retain(|sequence, _| *sequence >= height);
        self.new_views.retain(|view, _| *view >= self.view);
        self.proof.clear();
        self.committed.clear();
        self.high_qc = self.high_qc.take().filter(|qc| qc.height >= height);
        self.locked_qc = self.locked_qc.take().filter(|qc| qc.height >= height);
        self.evidence.prune_fast_before(height, self.view);
        self.timer()
    }
    fn supports_catch_up(&self) -> bool {
        true
    }
    fn on_start(&mut self) -> Vec<ConsensusAction> {
        self.started = true;
        if self.height == 0 && self.node_id == self.current_primary() {
            let block = self
                .evidence
                .manifest()
                .map(|manifest| {
                    Block::new_initial_block_signed(
                        &manifest.chain_id,
                        crate::message::hex_decode(&manifest.initial_state_root)
                            .expect("validated root"),
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
            ConsensusEvent::Message { from, message } => {
                let message = match (self.mode, message) {
                    (FastMode::Hotstuff2, ConsensusMessage::Hotstuff2(message))
                    | (FastMode::Votor, ConsensusMessage::Votor(message)) => message,
                    _ => return vec![],
                };
                match message {
                    FastMessage::Proposal {
                        view,
                        height,
                        proposal,
                        justify,
                    } => self.on_proposal(from, view, height, proposal, justify),
                    // Compact bodies are expanded and state-validated by Node before
                    // they reach consensus. A raw compact message cannot cast a vote.
                    FastMessage::CompactProposal { .. } => vec![],
                    FastMessage::ProposalRequest {
                        view,
                        height,
                        digest,
                    } => {
                        if view != self.view || height != self.height {
                            vec![]
                        } else if let Some(proposal) = self.proposals.get(&digest).cloned() {
                            vec![ConsensusAction::Send {
                                peer: from,
                                message: self.message(FastMessage::Proposal {
                                    view,
                                    height,
                                    proposal,
                                    justify: self.high_qc.clone(),
                                }),
                            }]
                        } else {
                            vec![]
                        }
                    }
                    FastMessage::RotationNotice {
                        view,
                        height,
                        previous,
                    } => self.on_rotation_notice(from, view, height, previous),
                    FastMessage::Vote {
                        view,
                        height,
                        digest,
                        kind,
                    } => self.on_vote(from, view, height, digest, kind),
                    FastMessage::Certified { qc } => self.on_certificate(qc),
                    FastMessage::NewView {
                        view,
                        height,
                        high_qc,
                        proposal,
                    } => self.on_new_view(from, view, height, high_qc, proposal),
                    FastMessage::StartView {
                        view,
                        height,
                        high_qc,
                        proposal,
                        proofs,
                    } => self.on_start_view(from, view, height, high_qc, proposal, proofs),
                }
            }
            ConsensusEvent::Proposal(block) => self.propose(block),
            ConsensusEvent::Timeout { view, sequence }
                if view == self.view && sequence == self.height =>
            {
                self.armed = None;
                if self.mode == FastMode::Votor
                    && !self
                        .voted
                        .contains_key(&(view, sequence, VoteKind::Notarize))
                {
                    let mut actions = self.vote(VoteKind::Skip, [0; 32]);
                    actions.extend(self.timer());
                    actions
                } else {
                    self.advance_view(view + 1)
                }
            }
            ConsensusEvent::LeaderUnavailable => self.advance_view(self.view + 1),
            ConsensusEvent::AutomaticViewChange {
                committed_view,
                next_sequence,
            } if self.view == committed_view && self.height == next_sequence => self
                .certified_rotation(committed_view, next_sequence)
                .unwrap_or_else(|| self.advance_view(self.view + 1)),
            ConsensusEvent::TransactionAvailable if self.node_id != self.current_primary() => {
                self.timer()
            }
            _ => vec![],
        }
    }
    fn leader_for_view(&self, view: u64) -> NodeId {
        view % self.node_count.max(1)
    }
    fn prepare_quorum(&self) -> u64 {
        self.threshold(self.first_kind())
    }
    fn commit_quorum(&self) -> u64 {
        self.threshold(VoteKind::Final)
    }
    fn quorum(&self) -> u64 {
        self.threshold(self.first_kind())
    }
    fn f(&self) -> u64 {
        if self.mode == FastMode::Votor {
            (self.node_count - 1) / 5
        } else {
            (self.node_count - 1) / 3
        }
    }
    fn can_propose(&self) -> bool {
        self.started
            && self.ready
            && self.active.is_none()
            && self.current_primary() == self.node_id
    }
    fn supports_batched_proposals(&self) -> bool {
        true
    }
    fn consensus_started(&mut self, on: bool) -> bool {
        if on {
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
        self.height
    }
    fn has_active_proposal(&self) -> bool {
        self.active.is_some()
    }
}

fn block_is_compact_candidate(block: &Block) -> bool {
    block.authentication.is_some()
        && block.transactions.len() >= 32
        && std::env::var_os("LIGHT_DEX_COMPACT_PROPOSALS").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lagging_replica_adopts_only_a_boundary_final_certificate() {
        let mut follower = FastConsensus::new(FastMode::Hotstuff2, 2, 4);
        follower.certificate_handoff = true;
        follower.automatic_rotation_interval = Some(4);
        follower.started = true;
        follower.catch_up_to_height(5);
        let qc = FastQc {
            view: 0,
            height: 4,
            digest: [7; 32],
            kind: VoteKind::Final,
            voters: vec![0, 1, 2],
            signatures: vec![String::new(); 3],
        };
        let actions = follower.on_event(ConsensusEvent::Message {
            from: 1,
            message: ConsensusMessage::Hotstuff2(FastMessage::RotationNotice {
                view: 1,
                height: 5,
                previous: qc,
            }),
        });
        assert_eq!(follower.view, 1);
        assert!(follower.ready);
        assert!(!actions.iter().any(|action| matches!(
            action,
            ConsensusAction::Broadcast(ConsensusMessage::Hotstuff2(FastMessage::NewView { .. }))
        )));

        let mut other = FastConsensus::new(FastMode::Hotstuff2, 2, 4);
        other.certificate_handoff = true;
        other.automatic_rotation_interval = Some(4);
        other.height = 4;
        let invalid = FastQc {
            view: 0,
            height: 3,
            digest: [8; 32],
            kind: VoteKind::Final,
            voters: vec![0, 1, 2],
            signatures: vec![String::new(); 3],
        };
        assert!(
            other
                .on_event(ConsensusEvent::Message {
                    from: 1,
                    message: ConsensusMessage::Hotstuff2(FastMessage::RotationNotice {
                        view: 1,
                        height: 4,
                        previous: invalid,
                    }),
                })
                .is_empty()
        );
        assert_eq!(other.view, 0);
    }

    #[test]
    fn certified_rotation_waits_for_durable_commit_and_blocks_old_leader() {
        let mut engine = FastConsensus::new(FastMode::Hotstuff2, 0, 4);
        engine.started = true;
        engine.certificate_handoff = true;
        engine.automatic_rotation_interval = Some(4);
        engine.height = 4;
        let mut block = Block::new();
        block.sequence = 4;
        let digest = block.to_digest();
        engine.proposals.insert(digest, Proposal { block });
        let qc = FastQc {
            view: 0,
            height: 4,
            digest,
            kind: VoteKind::Final,
            voters: vec![0, 1, 2],
            signatures: vec![String::new(); 3],
        };
        let actions = engine.commit(
            digest,
            FastProof::Hotstuff2 {
                prepare: FastQc {
                    kind: VoteKind::Prepare,
                    ..qc.clone()
                },
                finality: qc,
            },
        );
        assert!(matches!(actions.as_slice(), [ConsensusAction::Commit(_)]));
        assert!(!engine.can_propose());
        assert_eq!(engine.view, 0);
        assert!(
            engine
                .on_event(ConsensusEvent::AutomaticViewChange {
                    committed_view: 1,
                    next_sequence: 5,
                })
                .is_empty()
        );
        assert_eq!(engine.view, 0);
        let actions = engine.on_event(ConsensusEvent::AutomaticViewChange {
            committed_view: 0,
            next_sequence: 5,
        });
        assert_eq!(engine.view, 1);
        assert!(engine.ready);
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, ConsensusAction::ScheduleTimeout { .. }))
        );
        assert!(!actions.iter().any(|action| matches!(
            action,
            ConsensusAction::Broadcast(ConsensusMessage::Hotstuff2(FastMessage::NewView { .. }))
        )));
    }

    #[test]
    fn compact_proposal_cannot_vote_before_full_body_validation() {
        let mut leader = FastConsensus::new(FastMode::Hotstuff2, 0, 4);
        leader.started = true;
        let block = Block::new();
        let digest = block.to_digest();
        let proposal = Proposal {
            block: block.clone(),
        };
        leader.proposals.insert(digest, proposal.clone());

        let mut follower = FastConsensus::new(FastMode::Hotstuff2, 1, 4);
        follower.started = true;
        let compact = super::super::chained::CompactProposal::from_block(&block);
        assert!(
            follower
                .on_event(ConsensusEvent::Message {
                    from: 0,
                    message: ConsensusMessage::Hotstuff2(FastMessage::CompactProposal {
                        view: 0,
                        height: 0,
                        digest,
                        compact,
                        justify: None,
                    }),
                })
                .is_empty()
        );
        assert!(follower.voted.is_empty());

        let reply = leader.on_event(ConsensusEvent::Message {
            from: 1,
            message: ConsensusMessage::Hotstuff2(FastMessage::ProposalRequest {
                view: 0,
                height: 0,
                digest,
            }),
        });
        assert!(matches!(reply.as_slice(), [ConsensusAction::Send { peer: 1,
            message: ConsensusMessage::Hotstuff2(FastMessage::Proposal { proposal: body, .. })
        }] if body.to_digest() == digest));
    }

    #[test]
    fn hotstuff2_gossip_votes_commit_with_matching_certificates() {
        let mut nodes = (0..4)
            .map(|id| {
                let mut node = FastConsensus::new(FastMode::Hotstuff2, id, 4);
                node.gossip_votes = true;
                node
            })
            .collect::<Vec<_>>();
        let mut queue = std::collections::VecDeque::new();
        for id in 0..4 {
            for action in nodes[id].on_start() {
                queue.push_back((id, action));
            }
        }
        let mut committed = Vec::new();
        let mut deliveries = 0;
        while let Some((from, action)) = queue.pop_front() {
            deliveries += 1;
            assert!(deliveries < 256, "consensus broadcast loop");
            match action {
                ConsensusAction::Broadcast(message) => {
                    for to in 0..4 {
                        if to == from {
                            continue;
                        }
                        let actions = nodes[to].on_event(ConsensusEvent::Message {
                            from: from as NodeId,
                            message: message.clone(),
                        });
                        for action in actions {
                            queue.push_back((to, action));
                        }
                    }
                }
                ConsensusAction::Commit(proposal) => committed.push(proposal.to_digest()),
                ConsensusAction::Send { .. } => panic!("gossip mode sent a unicast vote"),
                _ => {}
            }
        }
        assert_eq!(committed.len(), 4);
        assert!(committed.iter().all(|digest| *digest == committed[0]));
        assert!(nodes.iter().all(|node| node.height == 1));
    }

    #[test]
    fn votor_does_not_revote_for_conflicting_block_after_view_change() {
        let mut engine = FastConsensus::new(FastMode::Votor, 1, 7);
        engine.started = true;
        let first = Proposal {
            block: Block::new(),
        };
        let first_digest = first.to_digest();
        let actions = engine.on_proposal(0, 0, 0, first, None);
        assert!(actions.iter().any(|action| matches!(action, ConsensusAction::Broadcast(ConsensusMessage::Votor(FastMessage::Vote {
            kind: VoteKind::Notarize, digest, ..
        })) if *digest == first_digest)));
        engine.advance_view(1);
        engine.ready = true;
        let mut conflicting = Block::new();
        conflicting.state_root[0] ^= 1;
        let actions = engine.on_proposal(1, 1, 0, Proposal { block: conflicting }, None);
        assert!(actions.is_empty());
    }

    #[test]
    fn votor_thresholds_are_distinct_from_hotstuff_two() {
        let votor = FastConsensus::new(FastMode::Votor, 0, 7);
        let hotstuff2 = FastConsensus::new(FastMode::Hotstuff2, 0, 7);
        assert_eq!(votor.threshold(VoteKind::Notarize), 5);
        assert_eq!(votor.fast_threshold(), 6);
        assert_eq!(hotstuff2.threshold(VoteKind::Prepare), 5);
    }

    #[test]
    fn final_certificate_waits_for_first_round_certificate() {
        let mut engine = FastConsensus::new(FastMode::Votor, 1, 7);
        engine.started = true;
        let proposal = Proposal {
            block: Block::new(),
        };
        let digest = proposal.to_digest();
        engine.on_proposal(0, 0, 0, proposal, None);
        let make_qc = |kind| FastQc {
            view: 0,
            height: 0,
            digest,
            kind,
            voters: vec![0, 1, 2, 3, 4],
            signatures: vec![String::new(); 5],
        };
        assert!(engine.on_certificate(make_qc(VoteKind::Final)).is_empty());
        assert_eq!(engine.height, 0);
        let actions = engine.on_certificate(make_qc(VoteKind::Notarize));
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, ConsensusAction::Commit(_)))
        );
        assert_eq!(engine.height, 1);
    }

    #[test]
    fn proposal_before_start_view_is_voted_after_view_starts() {
        let mut engine = FastConsensus::new(FastMode::Hotstuff2, 2, 7);
        engine.started = true;
        engine.advance_view(1);
        let proposal = Proposal {
            block: Block::new(),
        };
        let digest = proposal.to_digest();
        assert!(engine.on_proposal(1, 1, 0, proposal, None).is_empty());
        let actions = engine.on_start_view(1, 1, 0, None, None, vec![]);
        assert!(actions.iter().any(|action| matches!(action,
            ConsensusAction::Broadcast(ConsensusMessage::Hotstuff2(
                FastMessage::Vote { kind: VoteKind::Prepare, digest: voted, .. }))
            if *voted == digest)));
    }

    #[test]
    fn hotstuff2_can_use_legacy_leader_collector_for_comparison() {
        let mut engine = FastConsensus::new(FastMode::Hotstuff2, 1, 4);
        engine.gossip_votes = false;
        engine.started = true;
        let proposal = Proposal {
            block: Block::new(),
        };
        let digest = proposal.to_digest();
        let actions = engine.on_proposal(0, 0, 0, proposal, None);
        assert!(actions.iter().any(|action| matches!(action,
            ConsensusAction::Send { peer: 0, message: ConsensusMessage::Hotstuff2(
                FastMessage::Vote { kind: VoteKind::Prepare, digest: voted, .. }) }
            if *voted == digest)));
    }

    #[test]
    fn catch_up_drops_old_height_lock_and_vote_choice() {
        let mut engine = FastConsensus::new(FastMode::Votor, 1, 7);
        engine.started = true;
        let old = FastQc {
            view: 0,
            height: 0,
            digest: [7; 32],
            kind: VoteKind::Notarize,
            voters: vec![0, 1, 2, 3, 4],
            signatures: vec![String::new(); 5],
        };
        engine.high_qc = Some(old.clone());
        engine.locked_qc = Some(old);
        engine.votor_choice.insert(0, [7; 32]);
        engine.catch_up_to_height(1);
        assert!(engine.high_qc.is_none());
        assert!(engine.locked_qc.is_none());
        assert!(engine.votor_choice.is_empty());
        assert_eq!(engine.next_sequence(), 1);
    }

    #[test]
    fn repeated_pending_qcs_do_not_grow_and_stronger_qc_wins() {
        let mut engine = FastConsensus::new(FastMode::Votor, 1, 7);
        let mut qc = FastQc {
            view: 0,
            height: 0,
            digest: [9; 32],
            kind: VoteKind::Notarize,
            voters: vec![0, 1, 2, 3, 4],
            signatures: vec![String::new(); 5],
        };
        for _ in 0..10 {
            engine.defer_qc(qc.clone());
        }
        assert_eq!(engine.pending_qcs[&qc.digest].len(), 1);
        qc.voters.push(5);
        qc.signatures.push(String::new());
        engine.defer_qc(qc.clone());
        assert_eq!(engine.pending_qcs[&qc.digest][0].voters, qc.voters);
    }
}
