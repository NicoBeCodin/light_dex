use super::*;

const MAX_GOSSIP_TRANSACTIONS: usize = 32;
const GOSSIP_WINDOW: Duration = Duration::from_millis(20);

impl<C: Consensus> Node<C> {
    /// Batch client transactions for the current leader and prewarm the other
    /// validators before ordering. Receivers do not rebroadcast prewarm batches.
    pub(super) fn queue_client_gossip(
        &mut self,
        signed: SignedTransaction,
        events: &crate::priority::Sender<NodeEvent>,
    ) {
        if self.gossip_pending.is_empty() {
            self.gossip_epoch = self.gossip_epoch.wrapping_add(1);
            let epoch = self.gossip_epoch;
            let events = events.clone();
            thread::spawn(move || {
                thread::sleep(GOSSIP_WINDOW);
                let _ = events.send(NodeEvent::GossipReady { epoch });
            });
        }
        self.gossip_pending.push(signed);
        if self.gossip_pending.len() >= MAX_GOSSIP_TRANSACTIONS {
            self.flush_client_gossip();
        }
    }

    pub(super) fn flush_client_gossip(&mut self) {
        if self.gossip_pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.gossip_pending);
        let primary = self.consensus.current_primary();
        for batch in pending.chunks(MAX_GOSSIP_TRANSACTIONS) {
            if primary != self.node_id {
                let to_leader = Message::SignedTransactionBatch {
                    transactions: batch.to_vec(),
                    prewarm: false,
                };
                match crate::message::sign_compressed_line(&to_leader, &self.network.auth_key)
                    .and_then(|line| self.network.send_bulk(&primary, line.as_bytes()))
                {
                    Ok(()) => {}
                    Err(_) => {
                        self.forward_cursor.get_or_insert(0);
                    }
                }
            }
            // Diagnostic loss injection for compact-proposal recovery tests.
            // The leader still receives the transactions; followers must
            // request a full signed proposal before they can vote.
            if std::env::var_os("LIGHT_DEX_DISABLE_TX_PREWARM").is_some() {
                continue;
            }
            let message = Message::SignedTransactionBatch {
                transactions: batch.to_vec(),
                prewarm: true,
            };
            if let Ok(line) = crate::message::sign_compressed_line(&message, &self.network.auth_key)
            {
                self.network.broadcast_gossip(
                    (primary != self.node_id).then_some(primary),
                    line.as_bytes(),
                );
            }
        }
    }
}
