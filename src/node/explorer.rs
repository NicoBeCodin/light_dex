//! Explicit explorer reads. The owner projects a bounded recent list or one
//! requested block, so polling never clones the chain or exchange state.
use super::*;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy)]
pub enum ExplorerQuery {
    Recent,
    Block(u64),
    Transaction(Digest),
    Hash(Digest),
}

impl<C: Consensus> Node<C> {
    pub(super) fn explorer_query(&self, query: ExplorerQuery) -> Value {
        match query {
            ExplorerQuery::Recent => {
                let blocks = self
                    .state
                    .blocks
                    .iter()
                    .rev()
                    .take(20)
                    .map(|block| self.explorer_block_summary(block))
                    .collect::<Vec<_>>();
                let transactions = self
                    .state
                    .blocks
                    .iter()
                    .rev()
                    .flat_map(|block| {
                        block
                            .transactions
                            .iter()
                            .enumerate()
                            .rev()
                            .map(move |(index, tx)| (block.sequence, index, tx))
                    })
                    .take(40)
                    .map(|(sequence, index, tx)| {
                        self.explorer_transaction_summary(sequence, index, tx)
                    })
                    .collect::<Vec<_>>();
                json!({
                    "node_id": self.node_id,
                    "height": self.state.height(),
                    "view": self.consensus.current_view(),
                    "primary": self.consensus.current_primary(),
                    "chain_id": self.signing.as_ref().map(|(manifest, _)| manifest.chain_id.as_str()),
                    "blocks": blocks,
                    "transactions": transactions,
                })
            }
            ExplorerQuery::Block(sequence) => {
                let Some(block) = self
                    .state
                    .block_at(sequence as usize)
                    .filter(|block| block.sequence == sequence)
                else {
                    return json!({"error": "block not found"});
                };
                let transactions = block
                    .transactions
                    .iter()
                    .enumerate()
                    .map(|(index, tx)| self.explorer_transaction_summary(sequence, index, tx))
                    .collect::<Vec<_>>();
                json!({
                    "node_id": self.node_id,
                    "height": self.state.height(),
                    "block": self.explorer_block_summary(block),
                    "transactions": transactions,
                })
            }
            ExplorerQuery::Transaction(digest) => {
                // Full-chain search only runs for an explicit lookup. Recent
                // polling never scans earlier blocks.
                let found = self.state.blocks.iter().rev().find_map(|block| {
                    block
                        .transactions
                        .iter()
                        .enumerate()
                        .find(|(_, tx)| tx.to_digest() == digest)
                        .map(|(index, tx)| (block, index, tx))
                });
                let Some((block, index, tx)) = found else {
                    return json!({"error": "transaction not found"});
                };
                json!({
                    "node_id": self.node_id,
                    "height": self.state.height(),
                    "block": self.explorer_block_summary(block),
                    "transaction": self.explorer_transaction_summary(block.sequence, index, tx),
                    "payload": tx,
                    "signature": block.authentication.as_ref().and_then(|auth| auth.signatures.get(index)),
                })
            }
            ExplorerQuery::Hash(digest) => {
                if let Some(block) = self
                    .state
                    .blocks
                    .iter()
                    .rev()
                    .find(|block| block.to_digest() == digest)
                {
                    return json!({"kind": "block", "detail": self.explorer_query(ExplorerQuery::Block(block.sequence))});
                }
                let detail = self.explorer_query(ExplorerQuery::Transaction(digest));
                if detail.get("error").is_some() {
                    detail
                } else {
                    json!({"kind": "transaction", "detail": detail})
                }
            }
        }
    }

    fn explorer_block_summary(&self, block: &Block) -> Value {
        let certificate = self
            .state
            .certificate_at(block.sequence as usize)
            .and_then(Option::as_ref);
        let (algorithm, view, votes) = match certificate {
            Some(CommitCertificate::Pbft { view, proofs, .. }) => {
                (Some("PBFT"), Some(*view), proofs.len())
            }
            Some(CommitCertificate::Hotstuff { qc }) => {
                (Some("HotStuff"), Some(qc.view), qc.voters.len())
            }
            Some(CommitCertificate::Chained { qcs }) => (
                Some("Chained HotStuff"),
                Some(qcs[0].view),
                qcs[0].voters.len(),
            ),
            Some(CommitCertificate::Hotstuff2 { finality, .. }) => (
                Some("HotStuff-2"),
                Some(finality.view),
                finality.voters.len(),
            ),
            Some(CommitCertificate::Votor { finality, .. }) => (
                Some("Votor-style"),
                Some(finality.view),
                finality.voters.len(),
            ),
            None => (None, None, 0),
        };
        json!({
            "sequence": block.sequence,
            "digest": crate::message::hex_encode(&block.to_digest()),
            "previous_hash": crate::message::hex_encode(&block.previous_hash),
            "state_root": crate::message::hex_encode(&block.state_root),
            "transaction_count": block.transactions.len(),
            "oracle_update_count": block.oracle_updates.len(),
            "algorithm": algorithm,
            "view": view,
            "leader": view.map(|view| self.consensus.leader_for_view(view)),
            "certificate_votes": votes,
        })
    }

    fn explorer_transaction_summary(&self, sequence: u64, index: usize, tx: &Transaction) -> Value {
        let digest = crate::message::hex_encode(&tx.to_digest());
        let outcome = self
            .recent_transaction_outcomes
            .iter()
            .rev()
            .find(|outcome| outcome.digest == digest);
        let kind = match tx {
            Transaction::UpdateQuotes { .. } => "Update quotes",
            Transaction::ClaimAirdrop { .. } => "Claim airdrop",
            Transaction::ReduceOrder { .. } => "Reduce order",
            Transaction::Transfer { .. } => "Transfer",
            Transaction::PlaceOrder { .. } => "Spot order",
            Transaction::PlacePerpOrder { .. } => "Perp order",
            Transaction::CancelOrder { .. } => "Cancel order",
            Transaction::DepositPerpCollateral { .. } => "Deposit collateral",
            Transaction::WithdrawPerpCollateral { .. } => "Withdraw collateral",
            Transaction::SetMarginMode { .. } => "Set margin mode",
            Transaction::LiquidatePerp { .. } => "Liquidation",
        };
        json!({
            "digest": digest,
            "block_sequence": sequence,
            "transaction_index": index,
            "kind": kind,
            "account": tx.account_id(),
            "nonce": tx.nonce(),
            "success": outcome.map(|outcome| outcome.success),
            "error": outcome.and_then(|outcome| outcome.error.as_deref()),
        })
    }
}
