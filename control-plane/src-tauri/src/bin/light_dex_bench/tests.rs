use super::*;
fn transfer(account: u64, nonce: u64) -> Transaction {
    Transaction::Transfer {
        from: account.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce,
    }
}
fn failure(transaction: &Transaction) -> TransactionOutcome {
    TransactionOutcome {
        digest: hex_encode(&transaction.to_digest()),
        account_id: transaction.account_id(),
        nonce: transaction.nonce(),
        success: false,
        error: Some("test failure".into()),
    }
}
fn test_driver(window: usize) -> ExtraDriver {
    let mut driver = ExtraDriver::new(Workload::Transfer, window, vec![0]);
    driver.wallet.insert(0, 0.into());
    driver
}
#[test]
fn percentile_nearest_rank() {
    assert_eq!(percentile(&[1, 2, 3, 4, 5], 0.95), Some(5));
    assert_eq!(percentile(&[], 0.95), None);
}
#[test]
fn balanced_driver_pairs_cover_all_six_markets() {
    let markets = (0..24).map(balanced_market).collect::<Vec<_>>();
    assert_eq!(
        markets,
        [
            0, 0, 1, 1, 2, 2, 3, 3, 8, 8, 9, 9, 0, 0, 1, 1, 2, 2, 3, 3, 8, 8, 9, 9
        ]
    );
}
#[test]
fn integer_schedule_has_exact_slot_count_at_non_divisor_rate() {
    let rate = 2_200;
    let measure = Duration::from_secs(60);
    assert!(scheduled_offset(131_999, rate) < measure);
    assert_eq!(scheduled_offset(132_000, rate), measure);
    assert!(scheduled_offset(132_001, rate) > measure);
}
#[test]
fn admission_errors_are_grouped_without_wallet_addresses() {
    assert_eq!(
        submissions::categorize_submission_error("no confirmed nonce available for account 0x123"),
        "no_confirmed_nonce"
    );
    assert_eq!(
        submissions::categorize_submission_error(
            "account 0x123 permits nonce 1..=32; transaction uses 33"
        ),
        "nonce_window"
    );
}
#[test]
fn outcome_trace_counts_success_failure_and_fills_by_market() {
    let directory = std::env::temp_dir().join(format!(
        "light-dex-market-trace-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("node-0.latency.jsonl"), concat!(
            "{\"sequence\":4,\"outcomes\":[{\"market_id\":1,\"success\":true,\"fills\":2,\"error\":null},",
            "{\"market_id\":1,\"success\":false,\"fills\":0,\"error\":\"InvalidNonce\"}]}\n",
            "{\"sequence\":5,\"outcomes\":[{\"market_id\":2,\"success\":true,\"fills\":1,\"error\":null}]}\n",
        )).unwrap();
    let (markets, failures, rows) = market_trace_outcomes(directory.to_str(), 4, 5).unwrap();
    assert_eq!(rows, 2);
    assert_eq!(markets[&1].included, 2);
    assert_eq!(markets[&1].successful, 1);
    assert_eq!(markets[&1].failed, 1);
    assert_eq!(markets[&1].fills, 2);
    assert_eq!(failures["InvalidNonce"], 1);
    assert!(!markets.contains_key(&2));
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn remote_replay_waits_for_completed_journal_archive() {
    let dir =
        std::env::temp_dir().join(format!("light-dex-archive-test-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(archived_remote_journals(&dir, &[3]).is_none());
    std::fs::write(
        dir.join("remote-archive.log"),
        "Retrieved signed journal for node 3\n",
    )
    .unwrap();
    assert!(archived_remote_journals(&dir, &[3]).is_none());
    std::fs::write(dir.join("remote-archive.log"), "Remote archival complete\n").unwrap();
    assert!(archived_remote_journals(&dir, &[3]).unwrap().is_err());
    std::fs::write(dir.join("node-3.json.journal"), "journal").unwrap();
    assert!(archived_remote_journals(&dir, &[3]).unwrap().is_ok());
    std::fs::remove_file(dir.join("node-3.json.journal")).unwrap();
    std::fs::write(dir.join("node-3.audit.json"), "audit").unwrap();
    assert!(archived_remote_journals(&dir, &[3]).unwrap().is_ok());
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn planned_pilot_has_all_axes() {
    let specs = trials(false);
    assert_eq!(specs.len(), 26);
    assert!(specs.iter().any(|s| s.nodes == 13));
    assert!(specs.iter().any(|s| s.timeout_ms == 200));
    assert!(specs.iter().any(|s| s.offered_tps == 160));
}
#[test]
fn pipeline_allocates_contiguous_nonces_up_to_window() {
    let mut driver = test_driver(3);
    let now = Instant::now();
    driver.observe_account(0, 7, &[]);
    for nonce in 7..10 {
        assert_eq!(driver.next_nonce(0), Some(nonce));
        driver.remember(transfer(0, nonce), now);
    }
    assert_eq!(driver.next_nonce(0), None);
    driver.observe_account(0, 9, &[]);
    assert_eq!(driver.pending_count(), 1);
    assert_eq!(driver.next_nonce(0), Some(10));
    // A lagging replica cannot make the driver's confirmed nonce regress.
    driver.observe_account(0, 7, &[]);
    assert_eq!(driver.next_nonce(0), Some(10));
}
#[test]
fn out_of_order_commit_keeps_lower_pending_nonce() {
    let mut driver = test_driver(4);
    let now = Instant::now() - Duration::from_secs(3);
    driver.observe_account(0, 0, &[]);
    for nonce in 0..3 {
        driver.remember(transfer(0, nonce), now);
    }
    let failures = std::collections::HashSet::new();
    driver.observe_account_indexed(
        0,
        &AccountNonce {
            account_id: 0.into(),
            nonce: 3,
            used_nonces: [1, 0],
        },
        &failures,
    );
    assert_eq!(driver.pending_count(), 2);
    assert_eq!(driver.next_nonce(0), Some(3));
    assert_eq!(
        driver.maintenance_action(0, Instant::now()),
        Some(MaintenanceAction::Retry(transfer(0, 0)))
    );
}
#[test]
fn failed_head_is_repaired_at_same_nonce_before_new_work() {
    let mut driver = test_driver(3);
    let now = Instant::now();
    driver.observe_account(0, 5, &[]);
    let head = transfer(0, 5);
    driver.remember(head.clone(), now);
    driver.remember(transfer(0, 6), now);
    driver.observe_account(0, 5, &[failure(&head)]);
    assert_eq!(driver.next_nonce(0), None);
    assert_eq!(
        driver.maintenance_action(0, now),
        Some(MaintenanceAction::Repair(repair_transfer(0.into(), 5)))
    );
    // After nonce 5 is consumed, a failed tail becomes the next head.
    driver.observe_account(0, 6, &[failure(&transfer(0, 6))]);
    assert_eq!(
        driver.maintenance_action(0, now),
        Some(MaintenanceAction::Repair(repair_transfer(0.into(), 6)))
    );
}
#[test]
fn timed_out_head_retries_identical_transaction_and_nonce() {
    let mut driver = test_driver(2);
    let now = Instant::now();
    let transaction = transfer(0, 3);
    driver.observe_account(0, 3, &[]);
    driver.remember(transaction.clone(), now);
    assert_eq!(
        driver.maintenance_action(0, now + Duration::from_secs(1)),
        None
    );
    assert_eq!(
        driver.maintenance_action(0, now + Duration::from_secs(2)),
        Some(MaintenanceAction::Retry(transaction))
    );
}
