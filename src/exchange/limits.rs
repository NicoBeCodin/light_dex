//! Deterministic protocol work limits. Never use wall-clock execution time to
//! decide which state transitions a validator accepts.
pub const MAX_FILLS_PER_ORDER: usize = 512;
pub const MAX_QUOTE_ACTIONS: usize = 32;
// Bounds the mandatory cancellation phase of liquidation across all markets.
pub const MAX_PERP_ORDERS_PER_ACCOUNT: usize = 512;
pub const MAX_TRANSACTION_WORK: usize = 4096;
pub const MAX_BLOCK_WORK: usize = 16_384;
pub const MAX_BLOCK_BYTES: usize = 1024 * 1024;
pub const MAX_TRANSACTION_BYTES: usize = 16 * 1024;

// Snapshot-safe cells: Clone copies the meter value instead of sharing a
// counter between speculative branches. Reads never change canonical state.
#[derive(Debug, Default)]
pub(super) struct WorkCounter(std::sync::atomic::AtomicUsize);
impl WorkCounter {
    pub fn new(value: usize) -> Self {
        Self(value.into())
    }
    pub fn get(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set(&self, value: usize) {
        self.0.store(value, std::sync::atomic::Ordering::Relaxed);
    }
}
impl Clone for WorkCounter {
    fn clone(&self) -> Self {
        Self::new(self.get())
    }
}
#[derive(Debug, Default)]
pub(super) struct MeterFlag(std::sync::atomic::AtomicBool);
impl MeterFlag {
    pub fn new(value: bool) -> Self {
        Self(value.into())
    }
    pub fn get(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set(&self, value: bool) {
        self.0.store(value, std::sync::atomic::Ordering::Relaxed);
    }
}
impl Clone for MeterFlag {
    fn clone(&self) -> Self {
        Self::new(self.get())
    }
}
