use super::*;
use crate::consensus::hotstuff::HotStuff;
use crate::message::verify_line;
use std::net::TcpListener;

#[test]
fn lagging_hotstuff_replica_replays_a_verified_batch() {
    let mut source = BlockchainState::new();
    let genesis = Block::new_initial_block();
    source.commit_block(&genesis).unwrap();
    let tx = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 7,
        nonce: 0,
    };
    assert!(source.add_transaction(&tx));
    let next = source.build_block().unwrap();
    source.commit_block(&next).unwrap();
    let address = "127.0.0.1:0".parse().unwrap();
    let network = Network::new(2, address, [9; 32]).unwrap();
    let mut node = Node::new(
        2,
        0,
        network,
        BlockchainState::new(),
        HotStuff::new(2, 4),
        100,
    )
    .unwrap();
    let (tx, _rx) = crate::priority::channel(16);
    node.apply_sync_response(
        0,
        BlockSyncResponse {
            checkpoint: None,
            from_sequence: 0,
            chain_height: 2,
            blocks: vec![genesis, next],
            anchor_digest: None,
            certificates: vec![None, None],
            speculative: Vec::new(),
        },
        &tx,
    );
    assert_eq!(node.state.blocks.len(), 2);
    assert_eq!(node.consensus.next_sequence(), 2);
    assert_eq!(
        node.state
            .exchange_state
            .get_nonce_for_account(0.into())
            .unwrap(),
        1
    );
}

#[test]
fn bad_sync_block_does_not_advance_the_chain() {
    let address = "127.0.0.1:0".parse().unwrap();
    let network = Network::new(2, address, [9; 32]).unwrap();
    let mut node = Node::new(
        2,
        0,
        network,
        BlockchainState::new(),
        HotStuff::new(2, 4),
        100,
    )
    .unwrap();
    let mut forged = Block::new_initial_block();
    forged.sequence = 1;
    let (tx, _rx) = crate::priority::channel(16);
    node.apply_sync_response(
        0,
        BlockSyncResponse {
            checkpoint: None,
            from_sequence: 0,
            chain_height: 2,
            blocks: vec![forged],
            anchor_digest: None,
            certificates: vec![None],
            speculative: Vec::new(),
        },
        &tx,
    );
    assert_eq!(node.state.blocks.len(), 0);
    assert_eq!(node.consensus.next_sequence(), 0);
}

#[test]
fn authenticated_block_fetch_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let key = [11; 32];
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request: Message = verify_line(&line, &key).unwrap();
        assert!(matches!(
            request,
            Message::Control(ControlMessage::FetchBlocks {
                from_sequence: 0,
                limit: SYNC_BATCH_SIZE,
                want_branch: false,
            })
        ));
        let response = BlockSyncResponse {
            checkpoint: None,
            from_sequence: 0,
            chain_height: 1,
            blocks: vec![Block::new_initial_block()],
            anchor_digest: None,
            certificates: vec![None],
            speculative: Vec::new(),
        };
        stream
            .write_all(sign_line(&response, &key).unwrap().as_bytes())
            .unwrap();
    });
    let response = fetch_blocks(address, 0, &key, Duration::from_millis(500)).unwrap();
    assert_eq!(response.blocks.len(), 1);
    assert_eq!(response.chain_height, 1);
    server.join().unwrap();
}

#[test]
fn bounded_sync_response_sends_only_missing_blocks_after_large_anchor() {
    let mut anchor = Block::new_initial_block();
    anchor.sequence = 1;
    anchor.transactions = (0..200)
        .map(|nonce| Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: 0,
            amount: 1,
            nonce,
        })
        .collect();
    let mut next = Block::new_initial_block();
    next.sequence = 2;
    next.transactions = anchor.transactions.clone();
    let response = super::sync::build_sync_response(
        &[Block::new_initial_block(), anchor.clone(), next],
        &[None, None, None],
        2,
        SYNC_BATCH_SIZE,
    );
    assert_eq!(response.anchor_digest, Some(anchor.to_digest()));
    assert_eq!(
        response
            .blocks
            .iter()
            .map(|block| block.sequence)
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(response.certificates.len(), 1);
}

#[test]
fn sync_response_batches_four_full_blocks_for_remote_recovery() {
    let blocks = (0..5)
        .map(|sequence| {
            let mut block = Block::new_initial_block();
            block.sequence = sequence;
            block.transactions = (0..512)
                .map(|nonce| Transaction::Transfer {
                    from: sequence.into(),
                    to: 99.into(),
                    asset: 0,
                    amount: 1,
                    nonce,
                })
                .collect();
            block
        })
        .collect::<Vec<_>>();
    let response =
        super::sync::build_sync_response(&blocks, &vec![None; blocks.len()], 0, SYNC_BATCH_SIZE);
    assert_eq!(response.blocks.len(), 4);
    assert_eq!(
        response
            .blocks
            .iter()
            .map(|block| block.sequence)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}

#[test]
fn conflicting_anchor_is_not_accepted() {
    let mut state = BlockchainState::new();
    let genesis = Block::new_initial_block();
    state.commit_block(&genesis).unwrap();
    let mut conflicting = genesis.clone();
    conflicting.state_root[0] ^= 1;
    let address = "127.0.0.1:0".parse().unwrap();
    let network = Network::new(2, address, [9; 32]).unwrap();
    let mut node = Node::new(2, 0, network, state, HotStuff::new(2, 4), 100).unwrap();
    let (tx, _rx) = crate::priority::channel(16);
    node.apply_sync_response(
        0,
        BlockSyncResponse {
            checkpoint: None,
            from_sequence: 0,
            chain_height: 1,
            blocks: vec![conflicting],
            anchor_digest: None,
            certificates: vec![None],
            speculative: Vec::new(),
        },
        &tx,
    );
    assert_eq!(node.state.blocks[0].to_digest(), genesis.to_digest());
    assert_eq!(node.consensus.next_sequence(), 1);
}

#[test]
fn mismatched_signed_anchor_rejects_following_blocks() {
    let mut state = BlockchainState::new();
    let genesis = Block::new_initial_block();
    state.commit_block(&genesis).unwrap();
    let network = Network::new(2, "127.0.0.1:0".parse().unwrap(), [9; 32]).unwrap();
    let mut node = Node::new(2, 0, network, state, HotStuff::new(2, 4), 100).unwrap();
    let mut next = genesis.clone();
    next.sequence = 1;
    let (tx, _rx) = crate::priority::channel(16);
    node.apply_sync_response(
        0,
        BlockSyncResponse {
            checkpoint: None,
            from_sequence: 1,
            chain_height: 2,
            anchor_digest: Some([0; 32]),
            blocks: vec![next],
            certificates: vec![None],
            speculative: Vec::new(),
        },
        &tx,
    );
    assert_eq!(node.state.blocks.len(), 1);
}
