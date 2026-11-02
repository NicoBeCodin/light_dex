//! Opt-in timing of local work. These samples never enter consensus state.
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};

#[derive(Default)]
struct Samples {
    elapsed_us: Vec<u64>,
    units: u64,
}

static ENABLED: OnceLock<bool> = OnceLock::new();
static SAMPLES: OnceLock<Mutex<BTreeMap<&'static str, Samples>>> = OnceLock::new();

pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var("LIGHT_DEX_PROFILE_STAGES").as_deref() == Ok("1"))
}

pub(crate) fn record(stage: &'static str, elapsed: Duration, units: u64) {
    if !enabled() {
        return;
    }
    let value = elapsed.as_micros().min(u64::MAX as u128) as u64;
    if let Ok(mut samples) = SAMPLES.get_or_init(|| Mutex::new(BTreeMap::new())).lock() {
        let entry = samples.entry(stage).or_default();
        entry.elapsed_us.push(value);
        entry.units = entry.units.saturating_add(units);
    }
}

pub(crate) fn report(node_id: u64) {
    if !enabled() {
        return;
    }
    let Some(samples) = SAMPLES.get() else {
        return;
    };
    let Ok(samples) = samples.lock() else {
        return;
    };
    for (stage, entry) in samples.iter() {
        let mut elapsed = entry.elapsed_us.clone();
        elapsed.sort_unstable();
        let at = |percent: usize| elapsed[(elapsed.len() - 1) * percent / 100];
        let total: u128 = elapsed.iter().map(|value| *value as u128).sum();
        println!(
            "PROFILE_STAGE node={node_id} stage={stage} count={} units={} total_us={total} p50_us={} p95_us={} p99_us={}",
            elapsed.len(),
            entry.units,
            at(50),
            at(95),
            at(99)
        );
    }
}
