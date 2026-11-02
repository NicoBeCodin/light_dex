use super::*;

impl<C: Consensus> Node<C> {
    fn on_admitted_signed(
        &mut self,
        signed: SignedTransaction,
        forward_now: bool,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) -> std::io::Result<()> {
        let digest = signed.transaction.to_digest();
        self.trace_tx_stage(digest, "admitted", None);
        self.metrics.transactions_received += 1;
        self.metrics
            .transaction_received_at
            .insert(digest, Instant::now());
        let actions = self
            .consensus
            .on_event(ConsensusEvent::TransactionAvailable);
        self.execute_consensus_actions(&actions, node_tx)?;
        self.request_proposal(node_tx);
        let primary = self.consensus.current_primary();
        if forward_now && self.node_id != primary {
            if let Err(error) =
                self.forward_transaction(primary, signed.transaction.clone(), Some(signed))
            {
                // The admission gossip already offers this transaction to
                // every peer; avoid replaying the whole mempool on one full
                // leader writer. A stalled chain triggers a bounded sweep.
                // A bounded local writer being full is not evidence that the
                // leader died. Retry pending work without a spurious view change.
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    eprintln!(
                        "Node {} failed forwarding signed tx to primary {primary}: {error}",
                        self.node_id
                    );
                    let actions = self.consensus.on_event(ConsensusEvent::LeaderUnavailable);
                    self.execute_consensus_actions(&actions, node_tx)?;
                }
            } else {
                self.trace_tx_stage(digest, "forwarded", None);
            }
        }
        Ok(())
    }
    fn handle_control_event(&mut self, event: ControlEvent) -> Result<(), std::io::Error> {
        match event {
            ControlEvent::PeerConnected {
                node_id,
                lane,
                tcp_stream,
            } => self.network.add_peer(node_id, lane, tcp_stream),

            ControlEvent::Pause { millis } => {
                println!("Node {} PAUSED for {} ms", self.node_id, millis);

                thread::sleep(Duration::from_millis(millis));

                println!("Node {} RESUMED", self.node_id);

                Ok(())
            }

            ControlEvent::Status { stream, accounts } => {
                let status = self.status_metadata();
                let snapshot = self.read_snapshot(false);
                let writer = self
                    .status_writer
                    .get_or_insert_with(|| status_worker::StatusWriter::new(self.network.auth_key));
                writer.send(status, snapshot, accounts, stream);
                Ok(())
            }

            ControlEvent::Health { stream } => {
                let status = self.status_metadata();
                let writer = self
                    .status_writer
                    .get_or_insert_with(|| status_worker::StatusWriter::new(self.network.auth_key));
                writer.send_health(status, stream);
                Ok(())
            }

            ControlEvent::FetchBlocks {
                from_sequence,
                limit,
                want_branch,
                stream,
            } => {
                let mut response = sync::build_sync_response(
                    &self.state.blocks,
                    &self.state.certificates,
                    from_sequence,
                    limit,
                );
                if want_branch && self.consensus.is_chained() {
                    response.speculative = self.consensus.certified_branch();
                }
                let writer = self
                    .sync_writer
                    .get_or_insert_with(|| sync_worker::SyncWriter::new(self.network.auth_key));
                writer.send(response, stream);
                Ok(())
            }
        }
    }

    pub fn run(&mut self) -> Result<(), std::io::Error> {
        let (node_tx, node_rx) = crate::priority::channel::<NodeEvent>(8_192);
        let (updates, _) = tokio::sync::broadcast::channel(128);
        self.read_updates = Some(updates.clone());
        let read_addr = read_api::start(node_tx.clone(), updates, self.network.auth_key)?;
        println!("READ_API {read_addr}");

        for (&(pair, venue), url) in &self.feed_urls {
            let url = url.clone();
            let tx = node_tx.clone();
            let stop = self.market_data_stop.clone();
            thread::spawn(move || {
                let mut last_sent: Option<Instant> = None;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = crate::oracle::run_pair_feed(pair, venue, &url, &stop, |quote| {
                        if last_sent.is_none_or(|at| at.elapsed() >= Duration::from_millis(200)) {
                            last_sent = Some(Instant::now());
                            let _ = tx.try_send(NodeEvent::MarketData { pair, quote });
                        }
                    });
                    if !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        thread::sleep(Duration::from_secs(2));
                    }
                }
            });
        }

        //
        // Block production timer
        //
        let block_tx = node_tx.clone();

        let block_time_ms = self.block_time_ms;

        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(block_time_ms));

                match block_tx.try_send(NodeEvent::ProduceBlock) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
        });

        if self.consensus.is_chained()
            || (self.consensus.supports_catch_up()
                && std::env::var_os("LIGHT_DEX_COMPACT_PROPOSALS").is_some())
        {
            let retry_tx = node_tx.clone();
            thread::spawn(move || {
                loop {
                    thread::sleep(Duration::from_millis(100));
                    match retry_tx.try_send(NodeEvent::DataRetry) {
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            });
        }

        //
        // Network
        //
        self.network.start()?;

        let network_rx = self
            .network
            .take_event_receiver()
            .expect("network receiver already taken");

        let network_node_tx = node_tx.clone();
        let admission_key = self.network.auth_key;
        let signing_manifest = self.signing.as_ref().map(|(manifest, _)| manifest.clone());
        let own_node_id = self.node_id;

        thread::spawn(move || {
            loop {
                let Ok(event) = network_rx.recv() else {
                    break;
                };

                match event {
                    NetworkEvent::AuthenticationRejected => {
                        let _ = network_node_tx.try_send(NodeEvent::AuthenticationRejected);
                    }
                    NetworkEvent::Message { from, message } => {
                        match message {
                            Message::Consensus(message) => {
                                if signing_manifest.is_some() {
                                    let _ =
                                        network_node_tx.try_send(NodeEvent::AuthenticationRejected);
                                    continue;
                                }
                                let _ = network_node_tx.send(NodeEvent::Consensus(
                                    ConsensusEvent::Message { from, message },
                                ));
                            }

                            // Peer forwarded a transaction.
                            Message::Transaction(transaction) => {
                                if signing_manifest.is_some() {
                                    let _ =
                                        network_node_tx.try_send(NodeEvent::AuthenticationRejected);
                                    continue;
                                }
                                let _ = network_node_tx
                                    .try_send(NodeEvent::NewTransaction(transaction));
                            }

                            Message::SignedConsensus(signed) => {
                                let Some(manifest) = &signing_manifest else {
                                    continue;
                                };
                                let consensus_verify_started = Instant::now();
                                let handoff = match &signed.message {
                                    crate::consensus::ConsensusMessage::Chained(
                                        crate::consensus::chained::ChainedMessage::NewView {
                                            view,
                                            ..
                                        },
                                    ) => Some((*view, "new_view")),
                                    crate::consensus::ConsensusMessage::Chained(
                                        crate::consensus::chained::ChainedMessage::StartView {
                                            view,
                                            ..
                                        },
                                    ) => Some((*view, "start_view")),
                                    _ => None,
                                };
                                let verify_started = handoff.map(|_| Instant::now());
                                if signed.signer != from
                                    || verify_consensus(&signed, manifest).is_err()
                                {
                                    let _ =
                                        network_node_tx.try_send(NodeEvent::AuthenticationRejected);
                                    continue;
                                }
                                // Warm exact-payload verification results before
                                // handing a proposal to the state-owning thread.
                                if let Some(block) = signed.message.proposed_block() {
                                    if crate::crypto::verification::verify_block_transactions(
                                        block, manifest,
                                    )
                                    .is_err()
                                    {
                                        let _ = network_node_tx
                                            .try_send(NodeEvent::AuthenticationRejected);
                                        continue;
                                    }
                                }
                                crate::profile::record(
                                    "consensus_message_verify",
                                    consensus_verify_started.elapsed(),
                                    1,
                                );
                                if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some() {
                                    if let (Some((view, kind)), Some(started)) =
                                        (handoff, verify_started)
                                    {
                                        eprintln!(
                                            "HANDOFF node={} view={} event=message_verified kind={} from={} verify_us={} unix_us={}",
                                            own_node_id,
                                            view,
                                            kind,
                                            from,
                                            started.elapsed().as_micros(),
                                            std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_micros()
                                        );
                                    }
                                }
                                let _ = network_node_tx
                                    .send(NodeEvent::SignedConsensus { from, signed });
                            }

                            Message::SignedTransaction(transaction) => {
                                if signing_manifest.as_ref().is_some_and(|manifest| {
                                    verify_transaction(&transaction, manifest).is_ok()
                                }) {
                                    let _ = network_node_tx
                                        .try_send(NodeEvent::NewSignedTransaction(transaction));
                                } else {
                                    let _ =
                                        network_node_tx.try_send(NodeEvent::AuthenticationRejected);
                                }
                            }

                            Message::SignedTransactionBatch {
                                transactions,
                                prewarm,
                            } => {
                                const MAX_PEER_BATCH: usize = 32;
                                if transactions.is_empty()
                                    || transactions.len() > MAX_PEER_BATCH
                                    || !signing_manifest.as_ref().is_some_and(|manifest| {
                                        transactions.iter().all(|transaction| {
                                            verify_transaction(transaction, manifest).is_ok()
                                        })
                                    })
                                {
                                    let _ =
                                        network_node_tx.try_send(NodeEvent::AuthenticationRejected);
                                    continue;
                                }
                                // One bounded bulk event avoids making consensus wait behind
                                // 32 separate owner-thread deliveries.
                                let _ = network_node_tx.try_send(
                                    NodeEvent::NewSignedTransactionBatch {
                                        transactions,
                                        prewarm,
                                    },
                                );
                            }

                            Message::Control(_)
                            | Message::PeerHandshake(_)
                            | Message::SubmitTransaction(_) => {}
                        }
                    }

                    NetworkEvent::IncomingPeer {
                        node_id,
                        lane,
                        stream,
                    } => {
                        let _ =
                            network_node_tx.send(NodeEvent::Control(ControlEvent::PeerConnected {
                                node_id,
                                lane,
                                tcp_stream: stream,
                            }));
                    }

                    NetworkEvent::PeerConnected { .. } => {}

                    NetworkEvent::ClientTransaction(transaction) => {
                        if signing_manifest.is_none() {
                            let _ =
                                network_node_tx.try_send(NodeEvent::NewTransaction(transaction));
                        } else {
                            let _ = network_node_tx.try_send(NodeEvent::AuthenticationRejected);
                        }
                    }

                    NetworkEvent::SignedClientTransaction(transaction) => {
                        if signing_manifest.as_ref().is_some_and(|manifest| {
                            verify_transaction(&transaction, manifest).is_ok()
                        }) {
                            let _ = network_node_tx
                                .try_send(NodeEvent::NewClientSignedTransaction(transaction));
                        } else {
                            let _ = network_node_tx.try_send(NodeEvent::AuthenticationRejected);
                        }
                    }

                    NetworkEvent::SubmitTransaction {
                        transaction,
                        stream,
                        received,
                    } => {
                        let manifest = signing_manifest.clone();
                        let owner = network_node_tx.clone();
                        let job = Box::new(move || {
                            let mut stream = stream;
                            if received.elapsed() > Duration::from_millis(250)
                                || !manifest.as_ref().is_some_and(|manifest| {
                                    verify_transaction(&transaction, manifest).is_ok()
                                })
                            {
                                crate::network::reply_admission(
                                    &mut stream,
                                    &transaction,
                                    false,
                                    Some("invalid signature or stale verification queue"),
                                    &admission_key,
                                );
                                return;
                            }
                            if let Err(error) = owner.try_send(NodeEvent::SubmitTransaction {
                                signed: transaction,
                                stream,
                                received,
                            }) {
                                let event = match error {
                                    mpsc::TrySendError::Full(event)
                                    | mpsc::TrySendError::Disconnected(event) => event,
                                };
                                if let NodeEvent::SubmitTransaction {
                                    signed, mut stream, ..
                                } = event
                                {
                                    crate::network::reply_admission(
                                        &mut stream,
                                        &signed,
                                        false,
                                        Some("node admission queue full"),
                                        &admission_key,
                                    );
                                }
                            }
                        });
                        if let Err(error) = crate::crypto::verification::try_verify_job(job) {
                            // Bounded synchronous fallback, never an unbounded
                            // thread per transaction and never an unchecked tx.
                            match error {
                                mpsc::TrySendError::Full(job)
                                | mpsc::TrySendError::Disconnected(job) => job(),
                            }
                        }
                    }

                    NetworkEvent::ClientControl { message, stream } => match message {
                        ControlMessage::Pause { millis } => {
                            let _ = network_node_tx
                                .try_send(NodeEvent::Control(ControlEvent::Pause { millis }));
                        }

                        ControlMessage::StatusAccounts { accounts } => {
                            if accounts.len() <= 4096 {
                                let _ = network_node_tx.try_send(NodeEvent::Control(
                                    ControlEvent::Status {
                                        stream,
                                        accounts: Some(accounts),
                                    },
                                ));
                            }
                        }
                        ControlMessage::Status => {
                            let _ = network_node_tx.try_send(NodeEvent::Control(
                                ControlEvent::Status {
                                    stream,
                                    accounts: None,
                                },
                            ));
                        }

                        ControlMessage::Health => {
                            let _ = network_node_tx
                                .try_send(NodeEvent::Control(ControlEvent::Health { stream }));
                        }

                        ControlMessage::FetchBlocks {
                            from_sequence,
                            limit,
                            want_branch,
                        } => {
                            let _ = network_node_tx.try_send(NodeEvent::Control(
                                ControlEvent::FetchBlocks {
                                    from_sequence,
                                    limit,
                                    want_branch,
                                    stream,
                                },
                            ));
                        }

                        ControlMessage::Shutdown => {
                            let _ = network_node_tx.send(NodeEvent::Shutdown);
                        }

                        ControlMessage::Hello { .. } | ControlMessage::BulkHello { .. } => {}
                    },
                }
            }
        });

        self.maybe_start_consensus(&node_tx)?;

        //
        // Node event loop
        //
        let mut local_events = VecDeque::new();
        let mut deferred_events = VecDeque::new();
        loop {
            let ready = if !self.pending_commit {
                deferred_events.pop_front()
            } else {
                None
            };
            let event = ready
                .or_else(|| local_events.pop_front())
                .map(Ok)
                .unwrap_or_else(|| node_rx.recv())
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "node event channel closed")
                })?;

            // Keep the old committed state readable while fsync runs. No
            // execution/consensus continuation may observe an undurable state.
            if self.pending_commit
                && !matches!(
                    &event,
                    NodeEvent::DurableCommit { .. }
                        | NodeEvent::ReadQuery { .. }
                        | NodeEvent::ExplorerQuery { .. }
                        | NodeEvent::MarketData { .. }
                        | NodeEvent::AuthenticationRejected
                        | NodeEvent::Control(
                            ControlEvent::Status { .. }
                                | ControlEvent::Health { .. }
                                | ControlEvent::PeerConnected { .. }
                        )
                )
            {
                match event {
                    NodeEvent::ProduceBlock => {} // periodic and coalescible
                    NodeEvent::SubmitTransaction {
                        signed,
                        mut stream,
                        received,
                    } if deferred_events.len() >= 8192 => {
                        let _ = received;
                        crate::network::reply_admission(
                            &mut stream,
                            &signed,
                            false,
                            Some("durability continuation queue full; retry"),
                            &self.network.auth_key,
                        );
                    }
                    other => {
                        if deferred_events.len() >= 8192 {
                            return Err(std::io::Error::other(
                                "durability continuation queue full",
                            ));
                        }
                        deferred_events.push_back(other);
                    }
                }
                continue;
            }

            match event {
                NodeEvent::DurableCommit {
                    result,
                    committed_view,
                } => {
                    let record = result.map_err(std::io::Error::other)?;
                    self.journal_write_us += record.write_us as u128;
                    self.journal_writes += 1;
                    let (block, outcomes) = self
                        .state
                        .commit_durable(record)
                        .map_err(std::io::Error::other)?;
                    self.record_committed_block(&block, outcomes)?;
                    self.pending_commit = false;
                    self.retry_pending_fast(&mut local_events);
                    if !self.consensus.is_chained() {
                        if let Some(interval) = self
                            .signing
                            .as_ref()
                            .and_then(|(manifest, _)| manifest.auto_view_change_blocks)
                        {
                            if block.sequence > 0 && (block.sequence + 1) % interval == 0 {
                                self.prewarm_next_primary();
                            }
                            if block.sequence > 0 && block.sequence % interval == 0 {
                                let old_primary = self.consensus.current_primary();
                                let actions =
                                    self.consensus
                                        .on_event(ConsensusEvent::AutomaticViewChange {
                                            committed_view,
                                            next_sequence: block.sequence + 1,
                                        });
                                self.execute_consensus_actions(&actions, &node_tx)?;
                                if old_primary != self.consensus.current_primary() {
                                    if self.signing.is_none() {
                                        self.forward_cursor = Some(0);
                                        self.forward_mempool_to_primary();
                                    }
                                }
                            }
                        }
                    }
                    while !self.pending_commit {
                        let Some(action) = self.deferred_actions.pop_front() else {
                            break;
                        };
                        self.execute_action(&action, &node_tx)?;
                    }
                }
                NodeEvent::ReadQuery { query, reply } => {
                    let _ = reply
                        .send(self.read_snapshot(matches!(query, read_api::ReadQuery::Summary)));
                }
                NodeEvent::ExplorerQuery { query, reply } => {
                    let _ = reply.send(self.explorer_query(query));
                }
                NodeEvent::MarketData { pair, quote } => {
                    self.market_data_quotes
                        .insert((pair, quote.venue), (quote, Instant::now()));
                }
                NodeEvent::AuthenticationRejected => self.metrics.authentication_rejections += 1,
                NodeEvent::SignedConsensus { from, signed } => {
                    self.consensus.record_verified(signed.clone());
                    local_events.push_back(NodeEvent::Consensus(ConsensusEvent::Message {
                        from,
                        message: signed.message,
                    }));
                }
                NodeEvent::Consensus(event) => {
                    // A next-height proposal can overtake the preceding
                    // durable commit. Hold it before state validation, then
                    // retry after the journal advances instead of dropping it.
                    if let ConsensusEvent::Message { from, message } = &event {
                        if let Some(height) = message.fast_height() {
                            if height > self.consensus.next_sequence()
                                || height > self.state.blocks.len() as u64
                            {
                                self.defer_fast_message(*from, message.clone());
                                if !self.pending_commit {
                                    self.start_sync(*from, &node_tx);
                                }
                                continue;
                            }
                        }
                    }
                    if let ConsensusEvent::Message {
                        from,
                        message:
                            crate::consensus::ConsensusMessage::Hotstuff2(
                                crate::consensus::fast::FastMessage::RotationNotice {
                                    height,
                                    previous,
                                    ..
                                },
                            ),
                    } = &event
                    {
                        let matches_durable_tip = self.state.blocks.last().is_some_and(|block| {
                            block.sequence.checked_add(1) == Some(*height)
                                && block.to_digest() == previous.digest
                        });
                        if !matches_durable_tip {
                            eprintln!(
                                "Node {} rejected rotation certificate for unknown predecessor at height {height}",
                                self.node_id
                            );
                            self.start_sync(*from, &node_tx);
                            continue;
                        }
                    }
                    let event = match event {
                        ConsensusEvent::Message {
                            from,
                            message:
                                crate::consensus::ConsensusMessage::Chained(
                                    crate::consensus::chained::ChainedMessage::CompactData {
                                        view,
                                        height,
                                        digest,
                                        compact,
                                    },
                                ),
                        } => {
                            self.metrics.compact_received += 1;
                            if compact.header.sequence != height
                                || from != self.consensus.leader_for_view(view)
                            {
                                continue;
                            }
                            match self.expand_compact_data(&compact, digest) {
                                Ok(Some(proposal)) => {
                                    self.metrics.compact_reconstructed += 1;
                                    ConsensusEvent::Message {
                                        from,
                                        message: crate::consensus::ConsensusMessage::Chained(
                                            crate::consensus::chained::ChainedMessage::Data {
                                                view,
                                                height,
                                                digest,
                                                proposal,
                                            },
                                        ),
                                    }
                                }
                                Ok(None) => {
                                    if self.pending_compact.len() >= 128
                                        && !self.pending_compact.contains_key(&digest)
                                    {
                                        continue;
                                    }
                                    let now = Instant::now();
                                    self.pending_compact.entry(digest).or_insert(
                                        PendingCompactData {
                                            from,
                                            view,
                                            height,
                                            compact,
                                            mode: CompactMode::Chained,
                                            first_seen: now,
                                            next_request: now + Duration::from_millis(150),
                                        },
                                    );
                                    continue;
                                }
                                Err(error) => {
                                    eprintln!(
                                        "Node {} rejected compact proposal: {error}",
                                        self.node_id
                                    );
                                    continue;
                                }
                            }
                        }
                        ConsensusEvent::Message {
                            from,
                            message:
                                crate::consensus::ConsensusMessage::Hotstuff2(
                                    crate::consensus::fast::FastMessage::CompactProposal {
                                        view,
                                        height,
                                        digest,
                                        compact,
                                        justify,
                                    },
                                ),
                        } => {
                            use crate::consensus::{ConsensusMessage, fast::FastMessage};
                            if compact.header.sequence != height
                                || from != self.consensus.leader_for_view(view)
                            {
                                continue;
                            }
                            if height > self.consensus.next_sequence()
                                || height > self.state.blocks.len() as u64
                            {
                                self.defer_fast_message(
                                    from,
                                    ConsensusMessage::Hotstuff2(FastMessage::CompactProposal {
                                        view,
                                        height,
                                        digest,
                                        compact,
                                        justify,
                                    }),
                                );
                                self.start_sync(from, &node_tx);
                                continue;
                            }
                            self.metrics.compact_received += 1;
                            match self.expand_compact_data(&compact, digest) {
                                Ok(Some(proposal)) => {
                                    self.metrics.compact_reconstructed += 1;
                                    ConsensusEvent::Message {
                                        from,
                                        message: ConsensusMessage::Hotstuff2(
                                            FastMessage::Proposal {
                                                view,
                                                height,
                                                proposal,
                                                justify,
                                            },
                                        ),
                                    }
                                }
                                Ok(None) => {
                                    if self.pending_compact.len() >= 128
                                        && !self.pending_compact.contains_key(&digest)
                                    {
                                        continue;
                                    }
                                    let now = Instant::now();
                                    self.pending_compact.entry(digest).or_insert(
                                        PendingCompactData {
                                            from,
                                            view,
                                            height,
                                            compact,
                                            mode: CompactMode::Hotstuff2 { justify },
                                            first_seen: now,
                                            next_request: now + Duration::from_millis(150),
                                        },
                                    );
                                    continue;
                                }
                                Err(error) => {
                                    eprintln!(
                                        "Node {} rejected compact HotStuff2 proposal: {error}",
                                        self.node_id
                                    );
                                    continue;
                                }
                            }
                        }
                        event => event,
                    };
                    if let ConsensusEvent::Message { from, message } = &event {
                        if let Some(height) = message.fast_height() {
                            if height > self.consensus.next_sequence()
                                || height > self.state.blocks.len() as u64
                            {
                                self.defer_fast_message(*from, message.clone());
                                self.start_sync(*from, &node_tx);
                                continue;
                            }
                        }
                    }
                    if let ConsensusEvent::Message {
                        from,
                        message:
                            crate::consensus::ConsensusMessage::Chained(
                                crate::consensus::chained::ChainedMessage::DataRequest { .. },
                            ),
                    } = &event
                    {
                        self.full_body_peers
                            .insert(*from, Instant::now() + Duration::from_secs(60));
                    }
                    if let ConsensusEvent::Message { from, message } = &event {
                        if self.consensus.is_chained() {
                            let branch = match message {
                                crate::consensus::ConsensusMessage::Chained(
                                    crate::consensus::chained::ChainedMessage::NewView {
                                        branch,
                                        view,
                                        ..
                                    },
                                ) => Some((branch, *view, "new_view")),
                                crate::consensus::ConsensusMessage::Chained(
                                    crate::consensus::chained::ChainedMessage::StartView {
                                        branch,
                                        view,
                                        ..
                                    },
                                ) => Some((branch, *view, "start_view")),
                                crate::consensus::ConsensusMessage::Chained(
                                    crate::consensus::chained::ChainedMessage::PrewarmBranch {
                                        branch,
                                        view,
                                        ..
                                    },
                                ) => Some((branch, *view, "prewarm_branch")),
                                crate::consensus::ConsensusMessage::Chained(
                                    crate::consensus::chained::ChainedMessage::BranchResponse {
                                        branch,
                                        view,
                                        ..
                                    },
                                ) => Some((branch, *view, "branch_response")),
                                _ => None,
                            };
                            if let Some((branch, view, kind)) = branch {
                                if branch.len()
                                    > crate::consensus::chained::MAX_CERTIFIED_BRANCH_ANCESTORS
                                {
                                    eprintln!(
                                        "Node {} rejected oversized chained branch from {from}",
                                        self.node_id
                                    );
                                    continue;
                                }
                                let validation_started = Instant::now();
                                let mut invalid = false;
                                for proposal in branch {
                                    if let Err(error) =
                                        self.validate_speculative_proposal(&proposal.block)
                                    {
                                        eprintln!(
                                            "Node {} rejected chained branch from {from}: {error}",
                                            self.node_id
                                        );
                                        if error == "missing speculative parent" {
                                            self.start_branch_sync(*from, &node_tx);
                                            // Keep the signed, QC-linked handoff visible to consensus.
                                            // Proposal bodies still require state validation before
                                            // this node can vote for a child or commit anything.
                                            break;
                                        }
                                        invalid = true;
                                        break;
                                    }
                                }
                                if invalid {
                                    continue;
                                }
                                self.retry_pending_data(&mut local_events);
                                if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some() {
                                    eprintln!(
                                        "HANDOFF node={} view={} event=branch_validated kind={} from={} blocks={} validate_us={} unix_us={}",
                                        self.node_id,
                                        view,
                                        kind,
                                        from,
                                        branch.len(),
                                        validation_started.elapsed().as_micros(),
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_micros()
                                    );
                                }
                            }
                        }
                        if let Some(block) = message.proposed_block() {
                            if !self.consensus.is_chained()
                                && self.consensus.supports_catch_up()
                                && block.sequence > self.state.blocks.len() as u64
                            {
                                self.start_sync(*from, &node_tx);
                                continue;
                            }
                            let validate_started = Instant::now();
                            let validated = if self.consensus.is_chained() {
                                self.validate_speculative_proposal(block)
                            } else {
                                self.state.validate_block(block)
                            };
                            crate::profile::record(
                                "proposal_validation",
                                validate_started.elapsed(),
                                block.transactions.len() as u64,
                            );
                            if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some()
                                && matches!(
                                    message,
                                    crate::consensus::ConsensusMessage::Hotstuff2(
                                        crate::consensus::fast::FastMessage::Proposal { .. }
                                    )
                                )
                            {
                                eprintln!(
                                    "FAST_VALIDATE node={} from={} height={} txs={} elapsed_us={} ok={}",
                                    self.node_id,
                                    from,
                                    block.sequence,
                                    block.transactions.len(),
                                    validate_started.elapsed().as_micros(),
                                    validated.is_ok()
                                );
                            }
                            if let Err(error) = validated {
                                eprintln!("Node {} rejected proposal: {error}", self.node_id);
                                if error == "missing speculative parent" {
                                    if let crate::consensus::ConsensusMessage::Chained(
                                        crate::consensus::chained::ChainedMessage::Data {
                                            digest,
                                            ..
                                        },
                                    ) = message
                                    {
                                        if self.pending_data.len() < 128
                                            || self.pending_data.contains_key(digest)
                                        {
                                            self.pending_data.entry(*digest).or_insert_with(|| {
                                                (*from, message.clone(), Instant::now())
                                            });
                                        }
                                    } else {
                                        self.start_branch_sync(*from, &node_tx);
                                    }
                                }
                                continue;
                            }
                            if let crate::consensus::ConsensusMessage::Chained(
                                crate::consensus::chained::ChainedMessage::Data { digest, .. },
                            ) = message
                            {
                                self.pending_data.remove(digest);
                                self.pending_compact.remove(digest);
                            }
                            if let crate::consensus::ConsensusMessage::Hotstuff2(
                                crate::consensus::fast::FastMessage::Proposal { proposal, .. },
                            ) = message
                            {
                                self.pending_compact.remove(&proposal.to_digest());
                            }
                            self.retry_pending_data(&mut local_events);
                        }
                        if let crate::consensus::ConsensusMessage::Chained(
                            crate::consensus::chained::ChainedMessage::Certified { qc },
                        ) = message
                        {
                            if let Some(block) = self
                                .speculative
                                .get(&qc.digest)
                                .map(|(block, _)| block.clone())
                            {
                                self.trace_block_stage(&block, "certified");
                            }
                        }
                    }
                    let old_primary = self.consensus.current_primary();

                    let dispatch_started = Instant::now();
                    let actions = self.consensus.on_event(event);
                    crate::profile::record(
                        "consensus_dispatch",
                        dispatch_started.elapsed(),
                        actions.len() as u64,
                    );

                    self.execute_consensus_actions(&actions, &node_tx)?;
                    let new_primary = self.consensus.current_primary();

                    if new_primary != old_primary {
                        self.last_primary_probe = Instant::now();
                        println!(
                            "Node {} primary changed {} -> {}",
                            self.node_id, old_primary, new_primary
                        );

                        if self.signing.is_none() {
                            self.forward_cursor = Some(0);
                            self.forward_mempool_to_primary();
                        }
                    }
                }

                NodeEvent::Control(event) => {
                    self.handle_control_event(event)?;
                }

                NodeEvent::NewTransaction(transaction) => {
                    if self.signing.is_some() {
                        continue;
                    }
                    let primary = self.consensus.current_primary();

                    let is_new = self.state.add_transaction(&transaction);

                    if is_new {
                        self.trace_tx_stage(transaction.to_digest(), "admitted", None);
                        self.metrics.transactions_received += 1;

                        self.metrics
                            .transaction_received_at
                            .insert(transaction.to_digest(), Instant::now());
                        let actions = self
                            .consensus
                            .on_event(ConsensusEvent::TransactionAvailable);
                        self.execute_consensus_actions(&actions, &node_tx)?;
                        self.request_proposal(&node_tx);
                    }

                    // Only forward toward the current leader.
                    if is_new && self.node_id != primary {
                        if let Err(error) = self.forward_transaction(primary, transaction, None) {
                            if self.signing.is_none() {
                                self.forward_cursor.get_or_insert(0);
                            }
                            if error.kind() == std::io::ErrorKind::WouldBlock {
                                continue;
                            }
                            eprintln!(
                                "Node {} failed forwarding tx to primary {}: {}",
                                self.node_id, primary, error
                            );

                            let actions =
                                self.consensus.on_event(ConsensusEvent::LeaderUnavailable);

                            self.execute_consensus_actions(&actions, &node_tx)?;
                        }
                    }
                }

                NodeEvent::SubmitTransaction {
                    signed,
                    mut stream,
                    received,
                } => {
                    // Local overload policy, not a block validity rule. Keep
                    // stale client work from extending the queue indefinitely.
                    if received.elapsed() > Duration::from_millis(250) {
                        crate::network::reply_admission(
                            &mut stream,
                            &signed,
                            false,
                            Some("admission queue age exceeds 250 ms; retry"),
                            &self.network.auth_key,
                        );
                        continue;
                    }
                    let valid = self
                        .signing
                        .as_ref()
                        .is_some_and(|(manifest, _)| verify_transaction(&signed, manifest).is_ok());
                    let known = valid
                        && self
                            .state
                            .knows_transaction(&signed.transaction.to_digest());
                    let admitted = valid && self.state.add_signed_transaction(&signed);
                    crate::network::reply_admission_with_leader(
                        &mut stream,
                        &signed,
                        admitted || known,
                        (!(admitted || known))
                            .then_some("invalid signature, nonce, account, or mempool capacity"),
                        &self.network.auth_key,
                        Some((
                            self.consensus.current_view(),
                            self.consensus.current_primary(),
                        )),
                    );
                    if admitted {
                        self.queue_client_gossip(signed.clone(), &node_tx);
                        self.on_admitted_signed(signed, false, &node_tx)?;
                    } else if known
                        && self.node_id != self.consensus.current_primary()
                        && self
                            .state
                            .has_pending_signed_transaction(&signed.transaction.to_digest())
                    {
                        // Admission is not commit: a prior forward can have been
                        // dropped by a full leader ingress queue. Retry it without
                        // inserting twice or restarting its latency clock.
                        let _ = self.forward_transaction(
                            self.consensus.current_primary(),
                            signed.transaction.clone(),
                            Some(signed),
                        );
                    }
                }

                NodeEvent::NewSignedTransaction(signed) => {
                    if self.state.add_signed_transaction(&signed) {
                        self.on_admitted_signed(signed, true, &node_tx)?;
                    }
                }

                NodeEvent::NewClientSignedTransaction(signed) => {
                    if self.state.add_signed_transaction(&signed) {
                        self.queue_client_gossip(signed.clone(), &node_tx);
                        self.on_admitted_signed(signed, false, &node_tx)?;
                    }
                }

                NodeEvent::GossipReady { epoch } => {
                    if epoch == self.gossip_epoch {
                        self.flush_client_gossip();
                    }
                }

                NodeEvent::NewSignedTransactionBatch {
                    transactions,
                    prewarm,
                } => {
                    let mut admitted = 0;
                    let mut reroute = Vec::new();
                    for signed in transactions {
                        let digest = signed.transaction.to_digest();
                        if self.state.add_signed_transaction(&signed) {
                            self.trace_tx_stage(digest, "admitted", None);
                            self.metrics.transactions_received += 1;
                            self.metrics
                                .transaction_received_at
                                .insert(digest, Instant::now());
                            admitted += 1;
                        }
                        if !prewarm && self.state.has_pending_signed_transaction(&digest) {
                            reroute.push(signed);
                        }
                    }
                    if admitted > 0 {
                        self.retry_compact_data(&mut local_events, &node_tx)?;
                        let actions = self
                            .consensus
                            .on_event(ConsensusEvent::TransactionAvailable);
                        self.execute_consensus_actions(&actions, &node_tx)?;
                        self.request_proposal(&node_tx);
                    }
                    if !reroute.is_empty() && self.node_id != self.consensus.current_primary() {
                        let primary = self.consensus.current_primary();
                        let message = Message::SignedTransactionBatch {
                            transactions: reroute,
                            prewarm: false,
                        };
                        let result =
                            crate::message::sign_compressed_line(&message, &self.network.auth_key)
                                .and_then(|line| self.network.send_bulk(&primary, line.as_bytes()));
                        if result.is_err() {
                            self.forward_cursor.get_or_insert(0);
                        }
                    }
                }

                NodeEvent::ProduceBlock => {
                    if self.consensus.is_chained() && !self.state.mempool.is_empty() {
                        let actions = self
                            .consensus
                            .on_event(ConsensusEvent::TransactionAvailable);
                        self.execute_consensus_actions(&actions, &node_tx)?;
                    }
                    if self.forward_cursor.is_none()
                        && !self.state.mempool.is_empty()
                        && self.last_forward_sweep.elapsed() >= Duration::from_secs(5)
                        && (self.signing.is_none()
                            || self.last_commit_at.elapsed() >= Duration::from_secs(5))
                    {
                        // Signed sessions normally gossip at admission. Sweep
                        // only after a stall to repair a missed bulk delivery.
                        self.forward_cursor = Some(0);
                    }
                    if self.forward_cursor.is_some() {
                        self.forward_mempool_to_primary();
                    }
                    if !self.network.is_ready() {
                        self.network.reconnect_missing();
                    }
                    self.probe_sync(&node_tx);
                    // Detect a dead leader even when no fresh transaction is
                    // admitted (for example, only stale nonces are replayed).
                    // A quiet cluster should still elect a live primary.
                    let primary = self.consensus.current_primary();
                    if self.consensus.consensus_started(false)
                        && primary != self.node_id
                        && self.last_primary_probe.elapsed()
                            >= Duration::from_millis(self.consensus.get_timeout())
                    {
                        self.last_primary_probe = Instant::now();
                        let unavailable =
                            self.network
                                .peers_address
                                .get(&primary)
                                .is_some_and(|address| {
                                    TcpStream::connect_timeout(address, self.primary_probe_timeout)
                                        .is_err()
                                });
                        if unavailable {
                            let actions =
                                self.consensus.on_event(ConsensusEvent::LeaderUnavailable);
                            self.execute_consensus_actions(&actions, &node_tx)?;
                        }
                    }
                    self.periodic_proposal(&node_tx)?;
                }

                NodeEvent::DataRetry => {
                    if !self.pending_compact.is_empty() {
                        self.retry_compact_data(&mut local_events, &node_tx)?;
                    }
                    if self.consensus.is_chained() {
                        if !self.sync_in_flight
                            && self.last_sync_probe.elapsed() >= Duration::from_millis(500)
                        {
                            let peer = self
                                .pending_data
                                .values()
                                .filter(|(_, _, seen)| seen.elapsed() >= Duration::from_millis(250))
                                .min_by_key(|(_, _, seen)| *seen)
                                .map(|(peer, _, _)| *peer);
                            if let Some(peer) = peer {
                                self.start_branch_sync(peer, &node_tx);
                            }
                        }
                        let actions = self.consensus.on_event(ConsensusEvent::DataRetry);
                        self.execute_consensus_actions(&actions, &node_tx)?;
                    }
                }

                NodeEvent::BatchReady { epoch } => {
                    self.batch_ready(epoch, &node_tx)?;
                }

                NodeEvent::Shutdown => {
                    self.market_data_stop
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    println!("Node {} shutting down", self.node_id);
                    println!(
                        "PROFILE journal_writes={} journal_write_us={}",
                        self.journal_writes, self.journal_write_us
                    );
                    println!(
                        "PROFILE gossip_shed_bytes={}",
                        self.network.gossip_shed_bytes
                    );
                    println!(
                        "PROFILE compact_received={} compact_reconstructed={} compact_body_requests={} adaptive_full_body_sends={}",
                        self.metrics.compact_received,
                        self.metrics.compact_reconstructed,
                        self.metrics.compact_body_requests,
                        self.metrics.adaptive_full_body_sends
                    );
                    crate::profile::report(self.node_id);
                    break;
                }

                NodeEvent::CatchUp { peer, result } => {
                    self.sync_in_flight = false;
                    match result {
                        Ok(response) => {
                            self.apply_sync_response(peer, response, &node_tx);
                            self.retry_pending_data(&mut local_events);
                            self.retry_pending_fast(&mut local_events);
                        }
                        Err(error) => {
                            eprintln!(
                                "Node {} could not sync from node {peer}: {error}",
                                self.node_id
                            );
                            if self.pending_branch_peer == Some(peer) {
                                self.metrics.branch_sync_failures += 1;
                                self.pending_branch_peer = None;
                            }
                        }
                    }
                }
            }

            self.maybe_start_consensus(&node_tx)?;
        }

        Ok(())
    }
}
