use super::*;
pub(super) fn build_sync_response(
    chain: &[Block],
    certificates: &[Option<CommitCertificate>],
    from_sequence: u64,
    limit: usize,
) -> BlockSyncResponse {
    let start = usize::try_from(from_sequence)
        .unwrap_or(usize::MAX)
        .min(chain.len());
    let mut blocks = Vec::new();
    let mut copied_certificates = Vec::new();
    let mut transactions = 0usize;
    let anchor_digest = from_sequence
        .checked_sub(1)
        .and_then(|sequence| chain.get(sequence as usize))
        .map(Block::to_digest);
    for (block, certificate) in chain
        .iter()
        .zip(certificates)
        .skip(start)
        .take(limit.min(SYNC_BATCH_SIZE))
    {
        // Always send at least one missing block. The preceding block is
        // checked through its signed digest, not retransmitted in full.
        if !blocks.is_empty()
            && (transactions.saturating_add(block.transactions.len()) > SYNC_BATCH_TRANSACTIONS
                || blocks.len() >= 8)
        {
            break;
        }
        transactions = transactions.saturating_add(block.transactions.len());
        blocks.push(block.clone());
        copied_certificates.push(certificate.clone());
    }
    BlockSyncResponse {
        checkpoint: None,
        from_sequence,
        chain_height: chain.len() as u64,
        anchor_digest,
        blocks,
        certificates: copied_certificates,
        speculative: Vec::new(),
    }
}

#[cfg(test)]
pub(super) fn fetch_blocks(
    address: SocketAddr,
    from_sequence: u64,
    auth_key: &[u8; 32],
    timeout: Duration,
) -> std::io::Result<BlockSyncResponse> {
    fetch_blocks_with_branch(address, from_sequence, auth_key, timeout, false)
}

fn fetch_blocks_with_branch(
    address: SocketAddr,
    from_sequence: u64,
    auth_key: &[u8; 32],
    timeout: Duration,
    want_branch: bool,
) -> std::io::Result<BlockSyncResponse> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = sign_line(
        &Message::Control(ControlMessage::FetchBlocks {
            from_sequence,
            limit: SYNC_BATCH_SIZE,
            want_branch,
        }),
        auth_key,
    )?;
    stream.write_all(request.as_bytes())?;
    let mut line = String::new();
    std::io::Read::take(BufReader::new(stream), 144 * 1024 * 1024 + 1).read_line(&mut line)?;
    if line.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "empty block sync response",
        ));
    }
    crate::message::verify_line_with_limit(&line, auth_key, 72 * 1024 * 1024)
}

impl<C: Consensus> Node<C> {
    pub fn set_sync_timeout(&mut self, millis: u64) {
        self.sync_timeout = Duration::from_millis(millis.clamp(100, 30_000));
    }

    pub fn set_primary_probe_timeout(&mut self, millis: u64) {
        self.primary_probe_timeout = Duration::from_millis(millis.clamp(100, 10_000));
    }

    pub(super) fn start_sync(
        &mut self,
        peer: NodeId,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) {
        self.start_sync_with_branch(peer, false, node_tx);
    }

    pub(super) fn start_branch_sync(
        &mut self,
        peer: NodeId,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) {
        self.pending_branch_peer = Some(peer);
        if !self.sync_in_flight {
            self.metrics.branch_sync_attempts += 1;
        }
        self.start_sync_with_branch(peer, true, node_tx);
    }

    fn start_sync_with_branch(
        &mut self,
        peer: NodeId,
        want_branch: bool,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) {
        if !self.consensus.supports_catch_up() || self.sync_in_flight {
            return;
        }
        let Some(&address) = self.network.peers_address.get(&peer) else {
            return;
        };
        self.sync_in_flight = true;
        self.last_sync_probe = Instant::now();
        // A signed digest of the preceding block checks for conflicting
        // history without retransmitting a full anchor block each second.
        let from_sequence = self.state.height() as u64;
        let auth_key = self.network.auth_key;
        let timeout = self.sync_timeout;
        let node_tx = node_tx.clone();
        thread::spawn(move || {
            let result =
                fetch_blocks_with_branch(address, from_sequence, &auth_key, timeout, want_branch);
            let _ = node_tx.send(NodeEvent::CatchUp { peer, result });
        });
    }

    pub(super) fn probe_sync(&mut self, node_tx: &crate::priority::Sender<NodeEvent>) {
        if !self.consensus.supports_catch_up()
            || self.sync_in_flight
            || self.last_sync_probe.elapsed() < Duration::from_secs(1)
        {
            return;
        }
        if let Some(peer) = self.pending_branch_peer {
            self.start_branch_sync(peer, node_tx);
            return;
        }
        if let Some((peer, _, _)) = self.pending_fast.front() {
            self.start_sync(*peer, node_tx);
            return;
        }
        let mut peers = self
            .network
            .peers_address
            .keys()
            .copied()
            .collect::<Vec<_>>();
        peers.sort_unstable();
        if !peers.is_empty() {
            let peer = peers[self.sync_peer_cursor % peers.len()];
            self.sync_peer_cursor = self.sync_peer_cursor.wrapping_add(1);
            self.start_sync(peer, node_tx);
        }
    }

    pub(super) fn sync_consensus_height(&mut self, node_tx: &crate::priority::Sender<NodeEvent>) {
        let previous_height = self.consensus.next_sequence();
        let height = self.state.height() as u64;
        let mut actions = self.consensus.catch_up_to_height(height);
        if height > previous_height {
            if let (Some(block), Some(Some(certificate))) =
                (self.state.blocks.last(), self.state.certificates.last())
            {
                actions.extend(self.consensus.adopt_verified_tip(block, certificate));
            }
        }
        if let Err(error) = self.execute_consensus_actions(&actions, node_tx) {
            eprintln!(
                "Node {} could not schedule catch-up actions: {error}",
                self.node_id
            );
        }
    }

    pub(super) fn apply_sync_response(
        &mut self,
        peer: NodeId,
        mut response: BlockSyncResponse,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) {
        if let Some(checkpoint) = response.checkpoint.take() {
            if response.from_sequence != self.state.height() as u64
                || response.chain_height <= self.state.height() as u64
            {
                return;
            }
            if let Err(error) = self.state.accept_checkpoint(checkpoint) {
                eprintln!("Rejected peer checkpoint: {error}");
                return;
            }
            self.sync_consensus_height(node_tx);
            return;
        }
        let local_height = self.state.height() as u64;
        if response.blocks.len() > SYNC_BATCH_SIZE
            || response.speculative.len()
                > crate::consensus::chained::MAX_CERTIFIED_BRANCH_ANCESTORS
            || response.from_sequence
                < self.state.base_height as u64 + u64::from(self.state.base_height > 0)
            || response.from_sequence > local_height
            || response.certificates.len() != response.blocks.len()
            || response
                .from_sequence
                .checked_add(response.blocks.len() as u64)
                .is_none_or(|end| !response.blocks.is_empty() && end > response.chain_height)
            || response.blocks.iter().enumerate().any(|(index, block)| {
                response.from_sequence.checked_add(index as u64) != Some(block.sequence)
            })
        {
            eprintln!(
                "Node {} rejected malformed sync response from {peer}",
                self.node_id
            );
            return;
        }
        if let Some(anchor_sequence) = response.from_sequence.checked_sub(1) {
            let local_digest = self
                .state
                .block_at(anchor_sequence as usize)
                .expect("retained sync anchor")
                .to_digest();
            if let Some(remote_digest) = response.anchor_digest {
                if remote_digest != local_digest {
                    eprintln!(
                        "Node {} detected conflicting sync anchor at sequence {anchor_sequence} from node {peer}",
                        self.node_id
                    );
                    return;
                }
            } else if response.chain_height >= response.from_sequence {
                eprintln!(
                    "Node {} rejected missing sync anchor from node {peer}",
                    self.node_id
                );
                return;
            }
        } else if response.anchor_digest.is_some() {
            eprintln!(
                "Node {} rejected unexpected genesis sync anchor from node {peer}",
                self.node_id
            );
            return;
        }
        let speculative = response.speculative;
        let mut applied = 0usize;
        for (block, certificate) in response.blocks.into_iter().zip(response.certificates) {
            let expected = self.state.height() as u64;
            if block.sequence < expected {
                let local = self
                    .state
                    .block_at(block.sequence as usize)
                    .expect("retained sync block");
                if local.to_digest() != block.to_digest() {
                    eprintln!(
                        "Node {} detected conflicting chain at sequence {} from node {peer}",
                        self.node_id, block.sequence
                    );
                    return;
                }
                continue;
            }
            if block.sequence != expected {
                if applied > 0 {
                    self.sync_consensus_height(node_tx);
                }
                eprintln!(
                    "Node {} rejected noncontiguous sync block {} from {peer}; expected {expected}",
                    self.node_id, block.sequence
                );
                return;
            }
            if let Err(error) = self.apply_committed_block(&block, certificate) {
                if applied > 0 {
                    self.sync_consensus_height(node_tx);
                }
                eprintln!(
                    "Node {} rejected sync block {} from {peer}: {error}",
                    self.node_id, block.sequence
                );
                return;
            }
            applied += 1;
        }
        if applied > 0 {
            let height = self.state.height() as u64;
            self.sync_consensus_height(node_tx);
            self.metrics.catch_up_events += 1;
            self.metrics.blocks_caught_up += applied as u64;
            println!(
                "Node {} caught up {applied} blocks from node {peer}; next sequence {height}",
                self.node_id
            );
        }
        if self.consensus.is_chained()
            && !speculative.is_empty()
            && self.state.height() as u64 >= response.chain_height
        {
            let mut repaired = true;
            for entry in speculative {
                let block = &entry.proposal.block;
                if entry.qc.digest != block.to_digest()
                    || entry.qc.height != block.sequence
                    || entry.qc.parent_digest != block.previous_hash
                {
                    repaired = false;
                    break;
                }
                if let Err(error) = self.validate_speculative_proposal(block) {
                    eprintln!(
                        "Node {} could not repair certified branch from {peer}: {error}",
                        self.node_id
                    );
                    repaired = false;
                    break;
                }
                let actions = self.consensus.import_certified_branch_entry(entry);
                if let Err(error) = self.execute_consensus_actions(&actions, node_tx) {
                    eprintln!(
                        "Node {} could not import certified branch from {peer}: {error}",
                        self.node_id
                    );
                    repaired = false;
                    break;
                }
            }
            if repaired {
                self.pending_branch_peer = None;
            } else {
                self.metrics.branch_sync_failures += 1;
                // Retry against another peer instead of repeatedly asking a
                // behind sender for the same incomplete branch.
                self.pending_branch_peer = None;
            }
        } else if speculative.is_empty()
            && self.pending_branch_peer == Some(peer)
            && self.state.height() as u64 >= response.chain_height
        {
            self.metrics.branch_sync_failures += 1;
            self.pending_branch_peer = None;
        }
        if (self.state.height() as u64) < response.chain_height && applied > 0 {
            if self.pending_branch_peer.is_some() {
                self.start_branch_sync(peer, node_tx);
            } else {
                self.start_sync(peer, node_tx);
            }
        }
    }
}
