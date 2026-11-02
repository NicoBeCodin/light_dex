//! Node-local market-data subscriptions and provisional oracle calculation.
//! Observations are not consensus inputs until a block commits their inputs.
pub mod config;
pub mod feed;
pub mod index;

pub use feed::{
    Venue, VenueFeedStatus, VenueQuote, parse_pair_quote, parse_quote, run_feed, run_pair_feed,
};
pub use index::{
    OracleObservation, OraclePair, OracleUpdate, candidate_from_cache, index_from_observations,
    reference_index_e8,
};
