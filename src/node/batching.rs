use super::*;

impl<C: Consensus> Node<C> {
    pub fn set_batch_window_ms(&mut self, millis: u64) {
        assert!(
            millis <= 1_000,
            "transaction batch window must be 0..1000 ms"
        );
        self.batch_window = Duration::from_millis(millis);
    }
    /// Coalesce transactions for a short, bounded window. A certified block
    /// with no queued transactions still gets its empty child immediately so
    /// three-chain finality does not wait for the periodic block timer.
    pub(super) fn request_proposal(&mut self, node_tx: &crate::priority::Sender<NodeEvent>) {
        if !self.consensus.can_propose() {
            return;
        }
        if !self.consensus.supports_batched_proposals()
            || self.batch_window.is_zero()
            || self.state.mempool.is_empty()
        {
            let _ = node_tx.try_send(NodeEvent::ProduceBlock);
            return;
        }
        // Once enough work is ready, avoid paying the coalescing timer on top
        // of the consensus round. One immediate event is enough even if more
        // transactions arrive before the owner handles it.
        let ready = if self.state.mempool.len() >= self.proposal_ready_txs {
            let exchange = self
                .consensus
                .speculative_parent()
                .and_then(|digest| self.speculative.get(&digest).map(|(_, exchange)| exchange))
                .unwrap_or(&self.state.exchange_state);
            self.state
                .mempool
                .ready_count(exchange, self.proposal_ready_txs)
        } else {
            self.state.mempool.len()
        };
        if ready >= self.proposal_ready_txs {
            if self
                .batch_deadline
                .is_none_or(|deadline| deadline > Instant::now())
            {
                self.batch_epoch = self.batch_epoch.wrapping_add(1);
                self.batch_deadline = Some(Instant::now());
                let _ = node_tx.try_send(NodeEvent::ProduceBlock);
            }
            return;
        }
        if self.batch_deadline.is_some() {
            return;
        }
        self.batch_epoch = self.batch_epoch.wrapping_add(1);
        let epoch = self.batch_epoch;
        self.batch_deadline = Some(Instant::now() + self.batch_window);
        let duration = self.batch_window;
        let node_tx = node_tx.clone();
        thread::spawn(move || {
            thread::sleep(duration);
            let _ = node_tx.try_send(NodeEvent::BatchReady { epoch });
        });
    }

    pub(super) fn batch_ready(
        &mut self,
        epoch: u64,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) -> std::io::Result<()> {
        if epoch != self.batch_epoch || self.batch_deadline.is_none() {
            return Ok(());
        }
        self.batch_deadline = None;
        self.maybe_propose(node_tx)
    }

    pub(super) fn periodic_proposal(
        &mut self,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) -> std::io::Result<()> {
        if self.consensus.supports_batched_proposals()
            && !self.batch_window.is_zero()
            && !self.state.mempool.is_empty()
        {
            if self
                .batch_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.batch_deadline = None;
                return self.maybe_propose(node_tx);
            }
            if self.batch_deadline.is_none() {
                self.request_proposal(node_tx);
            }
            return Ok(());
        }
        self.maybe_propose(node_tx)
    }
}
