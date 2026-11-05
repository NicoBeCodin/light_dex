use super::*;
use crate::{
    blockchain::{BlockchainState, CommittedBlock},
    consensus::{CommitCertificate, hotstuff::Phase},
};
use std::sync::Arc;

#[test]
fn selected_markets_are_bound_by_genesis_and_survive_manifest_reload() {
    let path =
        std::env::temp_dir().join(format!("ldex-market-selection-{}", rand::random::<u64>()));
    let manifest = create_session_with_market_ids(
        &path,
        "hotstuff2",
        4,
        FaucetTerms::default(),
        Some(4),
        vec![0, 1, 2, 3, 8, 9],
    )
    .unwrap();
    let loaded = SessionManifest::load(&path.join("manifest.json")).unwrap();
    assert_eq!(loaded.chain_id, manifest.chain_id);
    assert_eq!(loaded.initial_exchange().unwrap().markets().len(), 6);
    let mut changed = loaded.clone();
    changed.market_ids = Some(vec![0, 1]);
    assert!(changed.validate().is_err());
    changed.market_ids = None;
    assert!(changed.validate().is_err());
    assert!(ExchangeState::with_claim_faucet_market_ids(manifest.faucet, Some(&[0, 0])).is_err());
    assert!(ExchangeState::with_claim_faucet_market_ids(manifest.faucet, Some(&[0, 999])).is_err());
    fs::remove_dir_all(path).unwrap();
}

fn session() -> (std::path::PathBuf, SessionManifest) {
    let path = std::env::temp_dir().join(format!(
        "light-dex-signature-test-{}",
        rand::random::<u64>()
    ));
    let manifest = create_session(&path, "pbft", 4).unwrap();
    (path, manifest)
}

#[test]
fn votor_certificates_require_distinct_signed_validators_and_the_right_session() {
    use crate::consensus::fast::{FastMessage, FastMode, FastQc, VoteKind};
    let path =
        std::env::temp_dir().join(format!("light-dex-votor-proof-{}", rand::random::<u64>()));
    let manifest =
        create_session_with_mode(&path, "votor", 7, FaucetTerms::default(), Some(4), false)
            .unwrap();
    let digest = [3; 32];
    let mut qc = FastQc {
        view: 2,
        height: 9,
        digest,
        kind: VoteKind::Notarize,
        voters: Vec::new(),
        signatures: Vec::new(),
    };
    for signer in 0..6 {
        let key = read_secret(&path.join("keys").join(format!("validator-{signer}.key"))).unwrap();
        let proof = sign_consensus(
            ConsensusMessage::Votor(FastMessage::Vote {
                view: 2,
                height: 9,
                digest,
                kind: VoteKind::Notarize,
            }),
            signer,
            &manifest,
            &key,
        )
        .unwrap();
        qc.voters.push(signer);
        qc.signatures.push(proof.signature);
    }
    verify_fast_qc(&qc, &manifest, FastMode::Votor).unwrap();
    qc.voters[5] = qc.voters[4];
    assert!(verify_fast_qc(&qc, &manifest, FastMode::Votor).is_err());
    qc.voters[5] = 5;
    qc.signatures[5] = "00".repeat(64);
    assert!(verify_fast_qc(&qc, &manifest, FastMode::Votor).is_err());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn peer_handoff_batch_preserves_account_signatures() {
    let (path, manifest) = session();
    let key = new_key().unwrap();
    let address = Address::from_public_key(&key.verifying_key());
    let transactions = (0..32)
        .map(|nonce| {
            sign_transaction(
                Transaction::Transfer {
                    from: address,
                    to: address,
                    asset: 0,
                    amount: 1,
                    nonce,
                },
                &manifest,
                &key,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let wire = crate::message::sign_line(
        &crate::message::Message::SignedTransactionBatch {
            transactions,
            prewarm: true,
        },
        &[7; 32],
    )
    .unwrap();
    let decoded: crate::message::Message = crate::message::verify_line(&wire, &[7; 32]).unwrap();
    let crate::message::Message::SignedTransactionBatch {
        mut transactions,
        prewarm,
    } = decoded
    else {
        panic!("batch message lost its variant");
    };
    assert!(prewarm);
    assert_eq!(transactions.len(), 32);
    assert!(
        transactions
            .iter()
            .all(|tx| verify_transaction(tx, &manifest).is_ok())
    );
    transactions[31].signature = "00".repeat(64);
    assert!(
        transactions
            .iter()
            .any(|tx| verify_transaction(tx, &manifest).is_err())
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn chained_handoff_proofs_bind_the_branch_without_repeating_it() {
    use crate::consensus::chained::{
        ChainedMessage, ChainedNewViewProof, ChainedQC, branch_digest,
    };
    let path =
        std::env::temp_dir().join(format!("light-dex-handoff-proof-{}", rand::random::<u64>()));
    let manifest =
        create_session_with_mode(&path, "hotstuff", 7, FaucetTerms::default(), Some(4), true)
            .unwrap();
    let branch = (0..8)
        .map(|sequence| {
            let mut block = Block::new_initial_block();
            block.sequence = sequence;
            crate::blockchain::Proposal { block }
        })
        .collect::<Vec<_>>();
    let mut proofs = Vec::new();
    let mut old_proofs = Vec::new();
    for signer in 0..5 {
        let key = read_secret(&path.join("keys").join(format!("validator-{signer}.key"))).unwrap();
        let signed = sign_consensus(
            ConsensusMessage::Chained(ChainedMessage::NewView {
                view: 1,
                high_qc: ChainedQC::genesis(),
                branch_digest: branch_digest(&branch),
                branch: branch.clone(),
            }),
            signer,
            &manifest,
            &key,
        )
        .unwrap();
        verify_consensus(&signed, &manifest).unwrap();
        let mut changed = signed.clone();
        if let ConsensusMessage::Chained(ChainedMessage::NewView { branch_digest, .. }) =
            &mut changed.message
        {
            branch_digest[0] ^= 1;
        }
        assert!(verify_consensus(&changed, &manifest).is_err());
        proofs.push(ChainedNewViewProof {
            signer,
            view: 1,
            high_qc: ChainedQC::genesis(),
            branch_digest: branch_digest(&branch),
            signature: signed.signature.clone(),
        });
        old_proofs.push(signed);
    }
    verify_chained_start_view(
        1,
        &ChainedQC::genesis(),
        &branch_digest(&branch),
        &proofs,
        &manifest,
    )
    .unwrap();
    let compact_bytes = serde_json::to_vec(&proofs).unwrap().len();
    let old_bytes = serde_json::to_vec(&old_proofs).unwrap().len();
    assert!(
        compact_bytes * 2 < old_bytes,
        "compact={compact_bytes}, old={old_bytes}"
    );
    proofs[0].branch_digest[0] ^= 1;
    assert!(
        verify_chained_start_view(
            1,
            &ChainedQC::genesis(),
            &branch_digest(&branch),
            &proofs,
            &manifest
        )
        .is_err()
    );
    proofs[0].branch_digest[0] ^= 1;
    let mut substituted = branch.clone();
    substituted[0].block.sequence += 1;
    assert!(
        verify_chained_start_view(
            1,
            &ChainedQC::genesis(),
            &branch_digest(&substituted),
            &proofs,
            &manifest
        )
        .is_err()
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn quote_batch_signature_binds_actions_order_flags_and_nonce() {
    use crate::blockchain::QuoteOrder;
    use crate::exchange::order::{OrderType, Side};
    let (path, manifest) = session();
    let key = new_key().unwrap();
    let tx = Transaction::UpdateQuotes {
        reduce: vec![],
        account_id: Address::from_public_key(&key.verifying_key()),
        market_id: 1,
        cancel: vec![12, 14],
        nonce: 7,
        place: vec![QuoteOrder {
            order_type: OrderType::IOC,
            side: Side::Buy,
            price: 100,
            quantity: 10,
            reduce_only: true,
        }],
    };
    let signed = sign_transaction(tx, &manifest, &key).unwrap();
    verify_transaction(&signed, &manifest).unwrap();
    for field in 0..9 {
        let mut tampered = signed.clone();
        if let Transaction::UpdateQuotes {
            market_id,
            cancel,
            reduce,
            place,
            nonce,
            ..
        } = &mut tampered.transaction
        {
            match field {
                0 => *market_id += 1,
                1 => cancel.reverse(),
                2 => place[0].price += 1,
                3 => place[0].quantity += 1,
                4 => place[0].reduce_only = false,
                5 => place[0].side = Side::Sell,
                6 => place[0].order_type = OrderType::PostOnly,
                7 => reduce.push(crate::blockchain::QuoteReduction {
                    order_id: 99,
                    remaining_quantity: 1,
                }),
                _ => *nonce += 1,
            }
        }
        assert!(verify_transaction(&tampered, &manifest).is_err());
    }
    let mut invalid = signed.transaction.clone();
    if let Transaction::UpdateQuotes { cancel, .. } = &mut invalid {
        *cancel = vec![12, 12];
    }
    assert!(sign_transaction(invalid, &manifest, &key).is_err());
    fs::remove_dir_all(path).unwrap();
}

fn pbft_certificate(path: &Path, manifest: &SessionManifest, block: &Block) -> CommitCertificate {
    let proofs = (0..3)
        .map(|id| {
            sign_consensus(
                ConsensusMessage::Pbft(PbftMessage::Commit {
                    view: 0,
                    sequence: block.sequence,
                    digest: block.to_digest(),
                }),
                id,
                manifest,
                &read_secret(&path.join("keys").join(format!("validator-{id}.key"))).unwrap(),
            )
            .unwrap()
        })
        .collect();
    CommitCertificate::Pbft {
        view: 0,
        sequence: block.sequence,
        digest: block.to_digest(),
        proofs,
    }
}

#[test]
fn canonical_encoding_has_stable_sorted_field_bytes() {
    let value = serde_json::json!({"b": true, "a": 1});
    let mut actual = Vec::new();
    encode_value(&value, &mut actual).unwrap();
    let mut expected = vec![5];
    expected.extend_from_slice(&2u64.to_be_bytes());
    expected.extend_from_slice(&1u64.to_be_bytes());
    expected.push(b'a');
    expected.push(2);
    expected.extend_from_slice(&1u64.to_be_bytes());
    expected.extend_from_slice(&1u64.to_be_bytes());
    expected.push(b'b');
    expected.extend_from_slice(&[1, 1]);
    assert_eq!(actual, expected);
}

#[test]
fn parallel_block_verification_rejects_bad_signature_in_later_chunk() {
    let (path, manifest) = session();
    let key = new_key().unwrap();
    let address = Address::from_public_key(&key.verifying_key());
    let signed: Vec<_> = (0..80)
        .map(|nonce| {
            sign_transaction(
                Transaction::Transfer {
                    from: address,
                    to: address,
                    asset: 0,
                    amount: 1,
                    nonce,
                },
                &manifest,
                &key,
            )
            .unwrap()
        })
        .collect();
    let mut block = Block::new_initial_block_signed(
        &manifest.chain_id,
        hex_decode(&manifest.initial_state_root).unwrap(),
    );
    block.transactions = signed
        .iter()
        .map(|signed| signed.transaction.clone())
        .collect();
    block.authentication.as_mut().unwrap().signatures = signed
        .iter()
        .map(|signed| signed.signature.clone())
        .collect();
    super::verification::verify_block_transactions(&block, &manifest).unwrap();
    let good = block.clone();
    block.authentication.as_mut().unwrap().signatures[79] = "00".repeat(64);
    assert!(super::verification::verify_block_transactions(&block, &manifest).is_err());
    super::verification::verify_block_transactions(&good, &manifest).unwrap();
    let mut other_chain = manifest.clone();
    other_chain.chain_id = hex_encode(&[42; 32]);
    assert!(super::verification::verify_block_transactions(&good, &other_chain).is_err());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn automatic_rotation_interval_is_bound_to_the_session() {
    let (path, mut manifest) = session();
    assert_eq!(manifest.auto_view_change_blocks, None);
    let original_chain_id = manifest.chain_id.clone();
    manifest.auto_view_change_blocks = Some(2);
    assert!(manifest.validate().is_err());
    manifest.chain_id = hex_encode(&manifest.calculated_id().unwrap());
    assert_ne!(manifest.chain_id, original_chain_id);
    manifest.validate().unwrap();
    manifest.auto_view_change_blocks = Some(0);
    assert!(manifest.validate().is_err());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn signed_transaction_and_certified_archive_reject_tampering() {
    let (path, manifest) = session();
    let manifest = Arc::new(manifest);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path.join("keys/validator-0.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.join("keys"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert!(manifest.accounts.is_empty());
    assert_eq!(
        fs::read_to_string(path.join("wallet.json")).unwrap().trim(),
        "{}"
    );
    let account = new_key().unwrap();
    let recipient = new_key().unwrap();
    let sender_address = Address::from_public_key(&account.verifying_key());
    let tx = Transaction::ClaimAirdrop {
        account_id: sender_address,
        nonce: 0,
    };
    let signed = sign_transaction(tx, &manifest, &account).unwrap();
    assert!(verify_transaction(&signed, &manifest).is_ok());
    let mut changed = signed.clone();
    if let Transaction::ClaimAirdrop { account_id, .. } = &mut changed.transaction {
        *account_id = Address::from_public_key(&recipient.verifying_key());
    }
    assert!(verify_transaction(&changed, &manifest).is_err());
    let mut other_chain = (*manifest).clone();
    other_chain.chain_id = hex_encode(&[42; 32]);
    assert!(verify_transaction(&signed, &other_chain).is_err());
    let file = path.join("node-0.json");
    let mut state = BlockchainState::load_signed(file.clone(), manifest.clone()).unwrap();
    assert!(!state.add_signed_transaction(&changed));
    let genesis = Block::new_initial_block_signed(
        &manifest.chain_id,
        hex_decode(&manifest.initial_state_root).unwrap(),
    );
    let proof = pbft_certificate(&path, &manifest, &genesis);
    state
        .commit_block_with_certificate(&genesis, proof)
        .unwrap();
    assert!(state.add_signed_transaction(&signed));
    let block = state.build_block().unwrap();
    // An extra signature is ignored by the hash's zip but must never
    // bypass authentication through the proposer's prepared-state cache.
    let mut malformed = block.clone();
    malformed
        .authentication
        .as_mut()
        .unwrap()
        .signatures
        .push("00".repeat(64));
    assert_eq!(malformed.to_digest(), block.to_digest());
    assert!(state.validate_block(&malformed).is_err());
    state.validate_block(&block).unwrap();
    let mut fake = pbft_certificate(&path, &manifest, &block);
    if let CommitCertificate::Pbft { proofs, .. } = &mut fake {
        let first = &proofs[0].signature[..2];
        let changed = if first == "00" { "01" } else { "00" };
        proofs[0].signature.replace_range(..2, changed);
    }
    assert!(state.commit_block_with_certificate(&block, fake).is_err());
    assert_eq!(state.blocks.len(), 1);
    let proof = pbft_certificate(&path, &manifest, &block);
    state.commit_block_with_certificate(&block, proof).unwrap();
    let replayed = BlockchainState::load_signed(file.clone(), manifest.clone()).unwrap();
    assert_eq!(replayed.blocks.len(), 2);
    assert_eq!(
        replayed
            .exchange_state
            .get_nonce_for_account(sender_address)
            .unwrap(),
        1
    );
    let journal = BlockchainState::journal_path(&file);
    let mut records = fs::read_to_string(&journal)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<CommittedBlock>(line).unwrap())
        .collect::<Vec<_>>();
    if let CommitCertificate::Pbft { proofs, .. } = &mut records[1].certificate {
        let changed = if &proofs[0].signature[..2] == "ff" {
            "fe"
        } else {
            "ff"
        };
        proofs[0].signature.replace_range(..2, changed);
    }
    let corrupt = records
        .iter()
        .map(|record| format!("{}\n", serde_json::to_string(record).unwrap()))
        .collect::<String>();
    fs::write(journal, corrupt).unwrap();
    assert!(BlockchainState::load_signed(file, manifest).is_err());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn hotstuff_qc_requires_distinct_valid_phase_votes() {
    let (path, mut manifest) = session();
    manifest.consensus = "hotstuff".into();
    manifest.chain_id = hex_encode(&manifest.calculated_id().unwrap());
    let digest = [9u8; 32];
    let mut qc = QuorumCertificate {
        view: 2,
        height: 3,
        phase: Phase::Commit,
        digest,
        voters: vec![0, 1, 2],
        signatures: Vec::new(),
    };
    for id in &qc.voters {
        let key = read_secret(&path.join("keys").join(format!("validator-{id}.key"))).unwrap();
        qc.signatures.push(
            sign_consensus(
                ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
                    view: qc.view,
                    height: qc.height,
                    phase: qc.phase,
                    digest,
                }),
                *id,
                &manifest,
                &key,
            )
            .unwrap()
            .signature,
        );
    }
    assert!(verify_hotstuff_qc(&qc, &manifest).is_ok());
    qc.phase = Phase::Prepare;
    assert!(verify_hotstuff_qc(&qc, &manifest).is_err());
    qc.phase = Phase::Commit;
    qc.voters[1] = 0;
    assert!(verify_hotstuff_qc(&qc, &manifest).is_err());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn view_change_proofs_bind_view_and_distinct_validators() {
    let (path, manifest) = session();
    let proofs = (0..3)
        .map(|id| {
            let key = read_secret(&path.join("keys").join(format!("validator-{id}.key"))).unwrap();
            sign_consensus(
                ConsensusMessage::Pbft(PbftMessage::ViewChange {
                    new_view: 2,
                    sequence: 5,
                }),
                id,
                &manifest,
                &key,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(verify_pbft_new_view(2, 5, &proofs, &manifest).is_ok());
    assert!(verify_pbft_new_view(3, 5, &proofs, &manifest).is_err());
    let mut duplicate = proofs.clone();
    duplicate[1] = duplicate[0].clone();
    assert!(verify_pbft_new_view(2, 5, &duplicate, &manifest).is_err());
    let mut forged = proofs.clone();
    forged[2].signer = 3;
    assert!(verify_pbft_new_view(2, 5, &forged, &manifest).is_err());

    let mut hotstuff = manifest.clone();
    hotstuff.consensus = "hotstuff".into();
    hotstuff.chain_id = hex_encode(&hotstuff.calculated_id().unwrap());
    let genesis = QuorumCertificate::genesis();
    let new_views = (0..3)
        .map(|id| {
            let key = read_secret(&path.join("keys").join(format!("validator-{id}.key"))).unwrap();
            sign_consensus(
                ConsensusMessage::Hotstuff(HotStuffMessage::NewView {
                    view: 2,
                    high_qc: genesis.clone(),
                }),
                id,
                &hotstuff,
                &key,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(verify_hotstuff_start_view(2, &genesis, &new_views, &hotstuff).is_ok());
    assert!(verify_hotstuff_start_view(2, &genesis, &new_views[..2], &hotstuff).is_err());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn signed_peer_hello_cannot_claim_another_validator_id() {
    let (path, manifest) = session();
    let key = read_secret(&path.join("keys/validator-0.key")).unwrap();
    let nonce = random_nonce().unwrap();
    let hello = sign_handshake("hello", 0, &nonce, "", &manifest, &key).unwrap();
    assert!(verify_handshake(&hello, "hello", &manifest).is_ok());
    let mut forged = hello.clone();
    forged.node_id = 1;
    assert!(verify_handshake(&forged, "hello", &manifest).is_err());
    forged = hello;
    forged.nonce = random_nonce().unwrap();
    assert!(verify_handshake(&forged, "hello", &manifest).is_err());
    fs::remove_dir_all(path).unwrap();
}
