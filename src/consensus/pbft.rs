use serde::{Deserialize, Serialize};

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use crate::{
    blockchain::{Block, Digest, Proposal},
    node::NodeId,
};

const DEFAULT_TIMEOUT_MS: u64 = 1000;

pub trait Consensus {
    fn set_node_count(&mut self, count: u64);
    fn restore_from_height(&mut self, height: u64);

    fn on_start(&mut self) -> Vec<ConsensusAction>;

    fn on_event(
        &mut self,
        event: ConsensusEvent,
    ) -> Vec<ConsensusAction>;

    fn primary_for_view(&self, view: u64) -> NodeId;

    fn prepare_quorum(&self) -> u64;

    fn commit_quorum(&self) -> u64;

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

#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum ConsensusMessage {
    Pbft(PbftMessage),
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
}

#[derive(Debug)]
pub struct Pbft {
    view: u64,

    next_sequence: u64,

    last_committed_sequence: Option<u64>,

    node_id: NodeId,

    node_count: u64,

    proposals: HashMap<ProposalKey, Proposal>,

    prepares: HashMap<ProposalKey, HashSet<NodeId>>,

    commits: HashMap<ProposalKey, HashSet<NodeId>>,

    committed: HashSet<ProposalKey>,

    active_proposal: Option<ProposalKey>,

    consensus_started: bool,

    view_changes: HashMap<u64, HashSet<NodeId>>,

    timeout_ms: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum PbftMessage {
    PrePrepare {
        view: u64,
        sequence: u64,
        digest: Digest,
        proposal: Proposal,
    },

    Prepare {
        view: u64,
        sequence: u64,
        digest: Digest,
    },

    Commit {
        view: u64,
        sequence: u64,
        digest: Digest,
    },

    ViewChange {
        new_view: u64,
        sequence: u64,
    },

    NewView {
        view: u64,
        sequence: u64,
    },
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ProposalKey {
    view: u64,
    sequence: u64,
    digest: Digest,
}

impl Pbft {
    pub fn new(
        node_id: NodeId,
        node_count: u64,
    ) -> Self {
        Pbft {
            view: 0,

            next_sequence: 0,

            last_committed_sequence: None,

            node_id,

            node_count,

            proposals: HashMap::new(),

            prepares: HashMap::new(),

            commits: HashMap::new(),

            committed: HashSet::new(),

            active_proposal: None,

            consensus_started: false,

            view_changes: HashMap::new(),

            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    fn handle_preprepare(
        &mut self,
        from: NodeId,
        view: u64,
        sequence: u64,
        digest: Digest,
        proposal: Proposal,
    ) -> Vec<ConsensusAction> {
        //
        // Check correct view + correct primary.
        //
        if sequence != self.next_sequence
            || view != self.view
            || from != self.primary_for_view(view)
        {
            return vec![];
        }

        //
        // Validate proposed block hash.
        //
        if proposal.to_digest() != digest {
            return vec![];
        }

        let key = ProposalKey {
            view,
            sequence,
            digest,
        };

        //
        // Exact duplicate PRE-PREPARE.
        //
        if self.proposals.contains_key(&key) {
            return vec![];
        }

        self.proposals.insert(
            key.clone(),
            proposal,
        );

        //
        // Record our own PREPARE before broadcasting.
        //
        let already_sent_prepare = self
            .prepares
            .get(&key)
            .is_some_and(|votes| {
                votes.contains(&self.node_id)
            });

        let mut actions = Vec::new();

        if !already_sent_prepare {
            self.prepares
                .entry(key.clone())
                .or_default()
                .insert(self.node_id);

            actions.push(ConsensusAction::Broadcast(
                ConsensusMessage::Pbft(
                    PbftMessage::Prepare {
                        view,
                        sequence,
                        digest,
                    },
                ),
            ));

            actions.push(ConsensusAction::ScheduleTimeout {
                view,
                sequence,

                duration: Duration::from_millis(
                    self.timeout_ms,
                ),
            });
        }

        actions.extend(self.reevaluate(&key));

        actions
    }

    fn handle_prepare(
        &mut self,
        from: NodeId,
        view: u64,
        sequence: u64,
        digest: Digest,
    ) -> Vec<ConsensusAction> {
        if view != self.view {
            return vec![];
        }

        let key = ProposalKey {
            view,
            sequence,
            digest,
        };

        self.prepares
            .entry(key.clone())
            .or_default()
            .insert(from);

        self.reevaluate(&key)
    }

    fn handle_commit(
        &mut self,
        from: NodeId,
        view: u64,
        sequence: u64,
        digest: Digest,
    ) -> Vec<ConsensusAction> {
        if view != self.view {
            return vec![];
        }

        let key = ProposalKey {
            view,
            sequence,
            digest,
        };

        self.commits
            .entry(key.clone())
            .or_default()
            .insert(from);

        self.reevaluate(&key)
    }

    fn reevaluate(
        &mut self,
        key: &ProposalKey,
    ) -> Vec<ConsensusAction> {
        let mut actions = Vec::new();

        let have_proposal = self.proposals.contains_key(key);

        let prepare_count = self
            .prepares
            .get(key)
            .map(|votes| votes.len())
            .unwrap_or(0) as u64;

        let prepared =
            have_proposal && prepare_count >= self.prepare_quorum();

        let already_sent_commit = self
            .commits
            .get(key)
            .is_some_and(|votes| {
                votes.contains(&self.node_id)
            });

        //
        // Prepared -> send COMMIT once.
        //
        if prepared && !already_sent_commit {
            self.commits
                .entry(key.clone())
                .or_default()
                .insert(self.node_id);

            actions.push(ConsensusAction::Broadcast(
                ConsensusMessage::Pbft(
                    PbftMessage::Commit {
                        view: key.view,
                        sequence: key.sequence,
                        digest: key.digest,
                    },
                ),
            ));
        }

        //
        // Re-read after inserting our own vote.
        //
        let commit_count = self
            .commits
            .get(key)
            .map(|votes| votes.len())
            .unwrap_or(0) as u64;

        let committed =
            prepared && commit_count >= self.commit_quorum();

        if committed && !self.committed.contains(key) {
            self.committed.insert(key.clone());

            self.last_committed_sequence =
                Some(key.sequence);

            self.next_sequence =
                self.next_sequence.max(key.sequence + 1);

            let proposal = self
                .proposals
                .get(key)
                .expect("committed proposal missing")
                .clone();

            self.active_proposal = None;

            actions.push(
                ConsensusAction::Commit(proposal)
            );
        }

        actions
    }

    fn handle_timeout(
        &mut self,
        view: u64,
        sequence: u64,
    ) -> Vec<ConsensusAction> {
        //
        // Stale timer from an older view.
        //
        if view != self.view {
            return vec![];
        }

        //
        // Sequence already committed.
        //
        if self
            .last_committed_sequence
            .is_some_and(|last| last >= sequence)
        {
            return vec![];
        }

        let new_view = self.view + 1;

        //
        // Don't send repeated view-change votes
        // if several timeout threads fire.
        //
        let already_voted = self
            .view_changes
            .get(&new_view)
            .is_some_and(|votes| {
                votes.contains(&self.node_id)
            });

        if already_voted {
            return vec![];
        }

        self.view_changes
            .entry(new_view)
            .or_default()
            .insert(self.node_id);

        println!(
            "Node {} timeout view {} -> requesting view {}",
            self.node_id,
            self.view,
            new_view
        );

        vec![ConsensusAction::Broadcast(
            ConsensusMessage::Pbft(
                PbftMessage::ViewChange {
                    new_view,
                    sequence,
                },
            ),
        )]
    }

    fn handle_view_change(
        &mut self,
        from: NodeId,
        new_view: u64,
        sequence: u64,
    ) -> Vec<ConsensusAction> {
        if new_view <= self.view {
            return vec![];
        }

        self.view_changes
            .entry(new_view)
            .or_default()
            .insert(from);

        let count = self
            .view_changes
            .get(&new_view)
            .map(|votes| votes.len())
            .unwrap_or(0) as u64;

        if count < self.commit_quorum() {
            return vec![];
        }

        //
        // Only the next primary announces NEW_VIEW.
        //
        if self.node_id != self.primary_for_view(new_view) {
            return vec![];
        }

        self.view = new_view;

        self.active_proposal = None;

        println!(
            "Node {} became primary for view {}",
            self.node_id, self.view
        );

        vec![ConsensusAction::Broadcast(
            ConsensusMessage::Pbft(
                PbftMessage::NewView {
                    view: new_view,
                    sequence,
                },
            ),
        )]
    }

    fn handle_new_view(
        &mut self,
        from: NodeId,
        view: u64,
        _sequence: u64,
    ) -> Vec<ConsensusAction> {
        if from != self.primary_for_view(view) {
            return vec![];
        }

        if view <= self.view {
            return vec![];
        }

        self.view = view;

        self.active_proposal = None;

        println!(
            "Node {} entering view {}. Primary = {}",
            self.node_id,
            view,
            self.primary_for_view(view)
        );

        vec![]
    }
}

impl Consensus for Pbft {
    fn set_node_count(&mut self, count: u64) {
        self.node_count = count;
    }

    fn restore_from_height(&mut self, height: u64) {
        self.next_sequence = height;
        self.last_committed_sequence = height.checked_sub(1);
    }

    fn on_start(&mut self) -> Vec<ConsensusAction> {
        if self.next_sequence > 0 { return vec![]; }
        //
        // View 0 primary creates genesis block.
        //
        if self.node_id != self.primary_for_view(self.view) {
            return vec![];
        }

        let block = Block::new_initial_block();

        let proposal = Proposal { block };

        let digest = proposal.to_digest();

        let key = ProposalKey {
            view: self.view,
            sequence: self.next_sequence,
            digest,
        };

        self.active_proposal = Some(key.clone());

        self.proposals.insert(
            key,
            proposal.clone(),
        );

        vec![
            ConsensusAction::Broadcast(
                ConsensusMessage::Pbft(
                    PbftMessage::PrePrepare {
                        view: self.view,
                        sequence: self.next_sequence,
                        digest,
                        proposal,
                    },
                ),
            ),

            ConsensusAction::ScheduleTimeout {
                view: self.view,
                sequence: self.next_sequence,

                duration: Duration::from_millis(
                    self.timeout_ms,
                ),
            },
        ]
    }

    fn on_event(
        &mut self,
        event: ConsensusEvent,
    ) -> Vec<ConsensusAction> {
        match event {
            ConsensusEvent::Proposal(block) => {
                if !self.can_propose() {
                    return vec![];
                }

                let proposal = Proposal { block };

                let digest = proposal.to_digest();

                let key = ProposalKey {
                    view: self.view,
                    sequence: self.next_sequence,
                    digest,
                };

                self.active_proposal = Some(key.clone());

                self.proposals.insert(
                    key,
                    proposal.clone(),
                );

                vec![
                    ConsensusAction::Broadcast(
                        ConsensusMessage::Pbft(
                            PbftMessage::PrePrepare {
                                view: self.view,
                                sequence: self.next_sequence,
                                digest,
                                proposal,
                            },
                        ),
                    ),

                    ConsensusAction::ScheduleTimeout {
                        view: self.view,
                        sequence: self.next_sequence,

                        duration: Duration::from_millis(
                            self.timeout_ms,
                        ),
                    },
                ]
            }

            ConsensusEvent::Message {
                from,
                message,
            } => match message {
                ConsensusMessage::Pbft(message) => {
                    match message {
                        PbftMessage::PrePrepare {
                            view,
                            sequence,
                            digest,
                            proposal,
                        } => self.handle_preprepare(
                            from,
                            view,
                            sequence,
                            digest,
                            proposal,
                        ),

                        PbftMessage::Prepare {
                            view,
                            sequence,
                            digest,
                        } => self.handle_prepare(
                            from,
                            view,
                            sequence,
                            digest,
                        ),

                        PbftMessage::Commit {
                            view,
                            sequence,
                            digest,
                        } => self.handle_commit(
                            from,
                            view,
                            sequence,
                            digest,
                        ),

                        PbftMessage::ViewChange {
                            new_view,
                            sequence,
                        } => self.handle_view_change(
                            from,
                            new_view,
                            sequence,
                        ),

                        PbftMessage::NewView {
                            view,
                            sequence,
                        } => self.handle_new_view(
                            from,
                            view,
                            sequence,
                        ),
                    }
                }
            },

            ConsensusEvent::Timeout {
                view,
                sequence,
            } => self.handle_timeout(
                view,
                sequence,
            ),
        }
    }

    fn primary_for_view(&self, view: u64) -> NodeId {
        view % self.node_count
    }

    fn prepare_quorum(&self) -> u64 {
        2 * self.f()
    }

    fn commit_quorum(&self) -> u64 {
        2 * self.f() + 1
    }

    fn f(&self) -> u64 {
        (self.node_count - 1) / 3
    }

    fn can_propose(&self) -> bool {
        self.node_id == self.primary_for_view(self.view)
            && self.active_proposal.is_none()
    }

    fn consensus_started(&mut self, turn_on: bool) -> bool {
        if turn_on {
            self.consensus_started = true;
        }

        self.consensus_started
    }

    fn current_primary(&self) -> NodeId {
        self.primary_for_view(self.view)
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
        self.next_sequence
    }

    fn has_active_proposal(&self) -> bool {
        self.active_proposal.is_some()
    }
}
