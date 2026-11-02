use super::*;
use crate::exchange::limits::*;

struct ByteLimit(usize);
impl Write for ByteLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("block byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn validate(block: &Block) -> Result<(), String> {
    if block.transactions.len() > BlockchainState::MAX_PROPOSAL_TRANSACTIONS {
        return Err("transaction count limit".into());
    }
    for tx in &block.transactions {
        tx.validate_shape().map_err(str::to_string)?;
        serde_json::to_writer(ByteLimit(MAX_TRANSACTION_BYTES), tx)
            .map_err(|_| "transaction byte limit")?;
    }
    serde_json::to_writer(ByteLimit(MAX_BLOCK_BYTES), block)
        .map_err(|_| "block byte limit".to_string())
}
