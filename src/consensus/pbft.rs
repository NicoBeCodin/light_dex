use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::{
    blockchain::{Block, Digest, Proposal, Transaction},
    node::NodeId,
};

pub trait Consensus {
    fn set_node_count(&mut self, count: u64);
    fn on_start(&mut self) -> Vec<ConsensusAction>;
    fn on_event(&mut self, node_event: ConsensusEvent) -> Vec<ConsensusAction>;
    fn primary_for_view(&self, view: u64) -> u64;
    fn prepare_quorum(&self) -> u64;
    fn commit_quorum(&self) -> u64;
    fn f(&self)->u64;
    fn can_propose(&self)->bool;
    fn consensus_started(&mut self, turn_on: bool)->bool;

    
}

//for future implemenations
#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum ConsensusMessage {
    Pbft(PbftMessage),
}


impl ConsensusMessage {
    pub fn to_vec(&self) -> Result<Vec<u8>, std::io::Error> {
        Ok(serde_json::to_vec(&self).expect("Failed serializing"))
    }

    pub fn from_vec(bytes: &[u8]) -> Result<Self, std::io::Error> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

pub enum ConsensusAction {
    Broadcast(ConsensusMessage),
    Send {
        peer: NodeId,
        message: ConsensusMessage,
    },
    Commit(Proposal),
}

pub enum ConsensusEvent {
    Message {
        from: NodeId,
        message: ConsensusMessage,
    },
    Timeout,
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
        // from: NodeId,
    },
    Commit {
        view: u64,
        sequence: u64,
        digest: Digest,
        // from: NodeId,
    },
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ProposalKey {
    view: u64,
    sequence: u64,
    digest: [u8; 32],
}

impl Pbft {
    pub fn new(node_id: NodeId, node_count: u64) -> Self {
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
        // 1. validate sender is primary for this view and that we are on the same view
        if view != self.view || !(from == self.primary_for_view(view)) {
            return vec![];
        }

        // 2. verify proposal.digest() == digest
        if proposal.to_digest() != digest {
            return vec![];
        }
        let proposal_key = ProposalKey {
            view,
            sequence,
            digest,
        };
        // 3. reject conflicting proposal for same view/sequence if needed
        if self.proposals.contains_key(&proposal_key) {
            eprintln!("Already saw that proposal");
            return vec![];
        }

        self.proposals.insert(proposal_key.clone(), proposal);
        // self.active_proposal = Some(proposal_key.clone());
        let message = PbftMessage::Prepare {
            view,
            sequence,
            digest,
            // from,
        };
        
        let mut actions: Vec<ConsensusAction> =Vec::new();
        let already_sent_prepare = self.prepares.get(&proposal_key).is_some_and(|votes| votes.contains(&self.node_id));
        if !already_sent_prepare {
            self.prepares.entry(proposal_key.clone()).or_default().insert(self.node_id);
            actions.push(ConsensusAction::Broadcast(ConsensusMessage::Pbft(message)));
        }
        
        actions.extend(self.reevaluate(&proposal_key));
        actions
    }

    pub fn handle_prepare(
        &mut self,
        from: NodeId,
        view: u64,
        sequence: u64,
        digest: Digest,
    ) -> Vec<ConsensusAction> {
        let proposal_key = ProposalKey {
            view,
            sequence,
            digest,
        };
        self.prepares
            .entry(proposal_key.clone())
            .or_default()
            .insert(from);
        self.reevaluate(&proposal_key)
    }
    pub fn handle_commit(
        &mut self,
        from: NodeId,
        view: u64,
        sequence: u64,
        digest: Digest,
    ) -> Vec<ConsensusAction> {
        let proposal_key = ProposalKey {
            view,
            sequence,
            digest,
        };
        self.commits
            .entry(proposal_key.clone())
            .or_default()
            .insert(from);
        self.reevaluate(&proposal_key)
    }

    pub fn reevaluate(&mut self, key: &ProposalKey) -> Vec<ConsensusAction> {
        let mut actions = Vec::new();
        let have_proposal = self.proposals.contains_key(key);

        let prepare_count = self.prepares.get(key).map(|votes| votes.len()).unwrap_or(0) as u64;

        let prepared = have_proposal && prepare_count >= self.prepare_quorum();

        let already_sent_commit = self.commits.get(key).is_some_and(|votes| votes.contains(&self.node_id));

        //Quorum on prepare and haven't seen commit yet
        if prepared && !already_sent_commit{
            // let already_sent_commit = self.commits.get(key).is_some_and(|votes| votes.contains(&self.node_id));
            
            // if !already_sent_commit {
            self.commits.entry(key.clone()).or_default().insert(self.node_id);
                
            let broadcast_message = ConsensusMessage::Pbft(PbftMessage::Commit {
                view: key.view,
                sequence: key.sequence,
                digest: key.digest,
                // from: self.node_id,
            });
            
            let broadcast_commit = ConsensusAction::Broadcast(broadcast_message);
            actions.push(broadcast_commit);
            // }            
        }

        let commit_count = self.commits.get(key).map(|votes| votes.len()).unwrap_or(0) as u64;
        let committed = prepared && commit_count >= self.commit_quorum();

        if committed && !self.committed.contains(key){

            self.committed.insert(key.clone());
            
            self.last_committed_sequence = Some(key.sequence);
            self.next_sequence = self.next_sequence.max(key.sequence + 1);
            
            
            let commit_action = ConsensusAction::Commit(self.proposals.get(key).unwrap().clone());
            self.active_proposal = None;

            
            actions.push(commit_action);
        }
        
        actions
    }
}

impl Consensus for Pbft {
    fn set_node_count(&mut self, count: u64) {
        self.node_count = count;
    }
    fn on_start(&mut self) -> Vec<ConsensusAction> {
        if self.node_id == 0 {
            let block = Block::new_initial_block_with(vec![Transaction::new_with("INITIAL BLOCK".to_string())]);
            let proposal = Proposal { block };
            
            
            let proposal_key = ProposalKey { view: self.view, sequence: self.next_sequence, digest: proposal.to_digest() };
            self.active_proposal = Some(proposal_key.clone());
            self.proposals.insert(proposal_key, proposal.clone());
            return vec![ConsensusAction::Broadcast(ConsensusMessage::Pbft(
                PbftMessage::PrePrepare {
                    view: 0,
                    sequence: 0,
                    digest: proposal.to_digest(),
                    proposal,
                },
            ))];
        }
        return vec![];
    }

    fn on_event(&mut self, consensus_event: ConsensusEvent) -> Vec<ConsensusAction> {
        match consensus_event {
            ConsensusEvent::Proposal(block) => {
                //in the case of us being leader we can turn this to PrePrepare
                if self.node_id == self.primary_for_view(self.view) {
                    let proposal = Proposal { block };
                    let proposal_digest = proposal.to_digest();
                    let proposal_key = ProposalKey {
                        view: self.view,
                        sequence: self.next_sequence,
                        digest: proposal_digest,
                    };
                    
                    self.active_proposal = Some(proposal_key.clone());
                    self.proposals.insert(proposal_key, proposal.clone());

                    let pbft_message = PbftMessage::PrePrepare {
                        view: self.view,
                        sequence: self.next_sequence,
                        digest: proposal_digest,
                        proposal,
                    };
                    let consensus_message = ConsensusMessage::Pbft(pbft_message);
                    vec![ConsensusAction::Broadcast(consensus_message)]
                } else {
                    return vec![];
                }
            }
            ConsensusEvent::Message { from: from_node, message } => match message {
                ConsensusMessage::Pbft(pbft_message) => match pbft_message {
                    PbftMessage::PrePrepare {
                        view,
                        sequence,
                        digest,
                        proposal,
                    } => self.handle_preprepare(from_node, view, sequence, digest, proposal),
                    PbftMessage::Prepare {
                        view,
                        sequence,
                        digest,
                    } => self.handle_prepare(from_node, view, sequence, digest),
                    PbftMessage::Commit {
                        view,
                        sequence,
                        digest,
                    } => self.handle_commit(from_node, view, sequence, digest),
                },
            },
            ConsensusEvent::Timeout => todo!(),
        }
    }

    fn primary_for_view(&self, view: u64) -> u64 {
         view % self.node_count
    }

    fn prepare_quorum(&self) -> u64 {
        2 * self.f()
    }
    fn commit_quorum(&self) -> u64 {
        2 * self.f() + 1
    }
    fn f(&self)->u64{
        (self.node_count - 1) / 3
    }

    fn can_propose(&self)->bool {
        self.node_id == self.primary_for_view(self.view) && self.active_proposal.is_none()
    }

    fn consensus_started(&mut self, turn_on: bool)->bool{

        if turn_on {
            self.consensus_started =true;
        }
        return self.consensus_started
        
        
        
    }
}
