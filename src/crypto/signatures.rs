use super::*;
pub fn sign_transaction(
    tx: Transaction,
    manifest: &SessionManifest,
    key: &SigningKey,
) -> io::Result<SignedTransaction> {
    tx.validate_shape().map_err(invalid)?;
    if key.verifying_key()
        != tx
            .account_id()
            .public_key()
            .map_err(|error| invalid(&error))?
    {
        return Err(invalid("wrong account key"));
    }
    let bytes = super::cache::transaction_bytes(&tx, manifest.chain_id_bytes()?)?;
    Ok(SignedTransaction {
        transaction: tx,
        signature: hex_encode(&key.sign(&bytes).to_bytes()),
    })
}

pub fn verify_transaction(tx: &SignedTransaction, manifest: &SessionManifest) -> io::Result<()> {
    tx.transaction.validate_shape().map_err(invalid)?;
    let key = tx
        .transaction
        .account_id()
        .public_key()
        .map_err(|error| invalid(&error))?;
    let bytes = super::cache::transaction_bytes(&tx.transaction, manifest.chain_id_bytes()?)?;
    verify(&key, &bytes, &tx.signature)
}

pub fn sign_consensus(
    message: ConsensusMessage,
    signer: u64,
    manifest: &SessionManifest,
    key: &SigningKey,
) -> io::Result<SignedConsensusMessage> {
    if key.verifying_key() != manifest.verifier(true, signer)? {
        return Err(invalid("wrong validator key"));
    }
    let bytes = consensus_message_bytes(&message, manifest)?;
    Ok(SignedConsensusMessage {
        signer,
        message,
        signature: hex_encode(&key.sign(&bytes).to_bytes()),
    })
}

pub fn verify_consensus(
    signed: &SignedConsensusMessage,
    manifest: &SessionManifest,
) -> io::Result<()> {
    let matches = match (&signed.message, manifest.consensus.as_str()) {
        (ConsensusMessage::Pbft(_), "pbft") => true,
        (ConsensusMessage::Hotstuff(_), "hotstuff") => !manifest.chained_hotstuff,
        (ConsensusMessage::Chained(_), "hotstuff") => manifest.chained_hotstuff,
        (ConsensusMessage::Hotstuff2(_), "hotstuff2") => true,
        (ConsensusMessage::Votor(_), "votor") => true,
        _ => false,
    };
    if !matches {
        return Err(invalid("wrong consensus algorithm"));
    }
    let key = manifest.verifier(true, signed.signer)?;
    let bytes = consensus_message_bytes(&signed.message, manifest)?;
    verify(&key, &bytes, &signed.signature)?;
    if let ConsensusMessage::Hotstuff(message) = &signed.message {
        let qc = match message {
            HotStuffMessage::Prepare { justify, .. } => Some(justify),
            HotStuffMessage::PreCommit { qc, .. }
            | HotStuffMessage::Commit { qc, .. }
            | HotStuffMessage::Decide { qc, .. } => Some(qc),
            HotStuffMessage::NewView { high_qc, .. }
            | HotStuffMessage::StartView { high_qc, .. } => Some(high_qc),
            HotStuffMessage::Vote { .. } => None,
        };
        if let Some(qc) = qc {
            if *qc != QuorumCertificate::genesis() {
                verify_hotstuff_qc(qc, manifest)?;
            }
        }
        if let HotStuffMessage::StartView {
            view,
            high_qc,
            proofs,
        } = message
        {
            verify_hotstuff_start_view(*view, high_qc, proofs, manifest)?;
        }
    }
    if let ConsensusMessage::Chained(message) = &signed.message {
        use crate::consensus::chained::ChainedMessage;
        if let ChainedMessage::NewView {
            branch_digest,
            branch,
            ..
        } = message
        {
            if !branch.is_empty()
                && crate::consensus::chained::branch_digest(branch) != *branch_digest
            {
                return Err(invalid("chained new-view branch digest mismatch"));
            }
        }
        let qc = match message {
            ChainedMessage::Prepare { justify, .. } => Some(justify),
            ChainedMessage::Certified { qc } => Some(qc),
            ChainedMessage::ViewNotice { high_qc, .. }
            | ChainedMessage::PrewarmBranch { high_qc, .. }
            | ChainedMessage::NewView { high_qc, .. }
            | ChainedMessage::StartView { high_qc, .. } => Some(high_qc),
            ChainedMessage::Vote { .. }
            | ChainedMessage::Data { .. }
            | ChainedMessage::CompactData { .. }
            | ChainedMessage::DataRequest { .. }
            | ChainedMessage::BranchRequest { .. }
            | ChainedMessage::BranchResponse { .. } => None,
        };
        if let Some(qc) = qc {
            verify_chained_qc(qc, manifest)?;
        }
        if let ChainedMessage::StartView {
            view,
            high_qc,
            branch_digest,
            branch,
            proofs,
        } = message
        {
            if !branch.is_empty()
                && crate::consensus::chained::branch_digest(branch) != *branch_digest
            {
                return Err(invalid("chained start-view branch digest mismatch"));
            }
            verify_chained_start_view(*view, high_qc, branch_digest, proofs, manifest)?;
        }
    }
    if let ConsensusMessage::Hotstuff2(message) | ConsensusMessage::Votor(message) = &signed.message
    {
        use crate::consensus::fast::{FastMessage, FastMode};
        let mode = if manifest.consensus == "votor" {
            FastMode::Votor
        } else {
            FastMode::Hotstuff2
        };
        let qc = match message {
            FastMessage::Proposal { justify, .. }
            | FastMessage::CompactProposal { justify, .. } => justify.as_ref(),
            FastMessage::Certified { qc } => Some(qc),
            FastMessage::RotationNotice { previous, .. } => Some(previous),
            FastMessage::NewView { high_qc, .. } | FastMessage::StartView { high_qc, .. } => {
                high_qc.as_ref()
            }
            FastMessage::Vote { .. } | FastMessage::ProposalRequest { .. } => None,
        };
        if let Some(qc) = qc {
            verify_fast_qc(qc, manifest, mode)?;
        }
        if let FastMessage::StartView {
            view,
            height,
            high_qc,
            proposal,
            proofs,
        } = message
        {
            verify_fast_start_view(
                *view,
                *height,
                high_qc,
                proposal.as_ref(),
                proofs,
                manifest,
                mode,
            )?;
        }
    }
    if let ConsensusMessage::Pbft(PbftMessage::NewView {
        view,
        sequence,
        proofs,
    }) = &signed.message
    {
        verify_pbft_new_view(*view, *sequence, proofs, manifest)?;
    }
    Ok(())
}

pub fn verify_pbft_new_view(
    view: u64,
    sequence: u64,
    proofs: &[SignedConsensusMessage],
    manifest: &SessionManifest,
) -> io::Result<()> {
    let n = manifest.validators.len() as u64;
    let quorum = crate::consensus::quorum_size(n);
    if proofs.len() < quorum as usize
        || proofs.len() > n as usize
        || proofs
            .windows(2)
            .any(|pair| pair[0].signer >= pair[1].signer)
    {
        return Err(invalid("invalid PBFT view-change quorum"));
    }
    for proof in proofs {
        if !matches!(&proof.message, ConsensusMessage::Pbft(PbftMessage::ViewChange {
            new_view, sequence: requested_sequence,
        }) if *new_view == view && *requested_sequence == sequence)
        {
            return Err(invalid("unrelated PBFT view-change proof"));
        }
        verify_consensus(proof, manifest)?;
    }
    Ok(())
}

pub fn verify_hotstuff_qc(qc: &QuorumCertificate, manifest: &SessionManifest) -> io::Result<()> {
    if *qc == QuorumCertificate::genesis() {
        return Ok(());
    }
    let n = manifest.validators.len() as u64;
    if qc.voters.len() != qc.signatures.len()
        || (qc.voters.len() as u64) < crate::consensus::quorum_size(n)
        || qc.voters.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(invalid("invalid HotStuff certificate quorum"));
    }
    for (&signer, signature) in qc.voters.iter().zip(&qc.signatures) {
        let message = ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
            view: qc.view,
            height: qc.height,
            phase: qc.phase,
            digest: qc.digest,
        });
        let key = manifest.verifier(true, signer)?;
        let bytes = signing_bytes(
            CONSENSUS_DOMAIN,
            manifest.chain_id_bytes()?,
            &(manifest.consensus.as_str(), &message),
        )?;
        verify(&key, &bytes, signature)?;
    }
    Ok(())
}

pub fn verify_fast_qc(
    qc: &crate::consensus::fast::FastQc,
    manifest: &SessionManifest,
    mode: crate::consensus::fast::FastMode,
) -> io::Result<()> {
    use crate::consensus::fast::{FastMessage, FastMode, VoteKind};
    let n = manifest.validators.len() as u64;
    let threshold = match mode {
        FastMode::Hotstuff2 => crate::consensus::quorum_size(n),
        FastMode::Votor => (3 * n + 4) / 5,
    };
    if qc.voters.len() != qc.signatures.len()
        || (qc.voters.len() as u64) < threshold
        || qc.voters.len() as u64 > n
        || qc.voters.windows(2).any(|pair| pair[0] >= pair[1])
        || mode == FastMode::Hotstuff2 && qc.kind != VoteKind::Prepare && qc.kind != VoteKind::Final
        || mode == FastMode::Votor && qc.kind == VoteKind::Prepare
        || qc.kind == VoteKind::Skip && qc.digest != [0; 32]
    {
        return Err(invalid("invalid fast consensus certificate"));
    }
    for (&signer, signature) in qc.voters.iter().zip(&qc.signatures) {
        let vote = FastMessage::Vote {
            view: qc.view,
            height: qc.height,
            digest: qc.digest,
            kind: qc.kind,
        };
        let message = match mode {
            FastMode::Hotstuff2 => ConsensusMessage::Hotstuff2(vote),
            FastMode::Votor => ConsensusMessage::Votor(vote),
        };
        let key = manifest.verifier(true, signer)?;
        let bytes = consensus_message_bytes(&message, manifest)?;
        verify(&key, &bytes, signature)?;
    }
    Ok(())
}

pub fn verify_fast_start_view(
    view: u64,
    height: u64,
    high_qc: &Option<crate::consensus::fast::FastQc>,
    proposal: Option<&crate::blockchain::Proposal>,
    proofs: &[SignedConsensusMessage],
    manifest: &SessionManifest,
    mode: crate::consensus::fast::FastMode,
) -> io::Result<()> {
    use crate::consensus::{
        ConsensusMessage,
        fast::{FastMessage, FastMode},
    };
    let n = manifest.validators.len() as u64;
    let quorum = crate::consensus::quorum_size(n);
    if proofs.len() < quorum as usize
        || proofs.len() > n as usize
        || proofs
            .windows(2)
            .any(|pair| pair[0].signer >= pair[1].signer)
    {
        return Err(invalid("invalid fast view-change quorum"));
    }
    let mut best = None;
    let mut has_body = high_qc.as_ref().is_none_or(|qc| qc.height < height);
    let mut proposed_without_qc = false;
    for proof in proofs {
        let message = match (mode, &proof.message) {
            (
                FastMode::Hotstuff2,
                ConsensusMessage::Hotstuff2(FastMessage::NewView {
                    view: v,
                    height: h,
                    high_qc,
                    proposal,
                }),
            )
            | (
                FastMode::Votor,
                ConsensusMessage::Votor(FastMessage::NewView {
                    view: v,
                    height: h,
                    high_qc,
                    proposal,
                }),
            ) if *v == view && *h == height => (high_qc, proposal),
            _ => return Err(invalid("unrelated fast view-change proof")),
        };
        verify_consensus(proof, manifest)?;
        if let Some(qc) = message.0 {
            if qc.height > height {
                return Err(invalid("future fast view-change certificate"));
            }
            if best
                .as_ref()
                .is_none_or(|current: &crate::consensus::fast::FastQc| {
                    (qc.height, qc.view, qc.digest) > (current.height, current.view, current.digest)
                })
            {
                best = Some(qc.clone());
            }
        }
        if let (Some(qc), Some(body)) = (message.0, message.1) {
            if qc.digest != body.to_digest() || body.block.sequence != qc.height {
                return Err(invalid("unrelated fast view-change body"));
            }
            if high_qc
                .as_ref()
                .is_some_and(|best| best.digest == qc.digest)
            {
                has_body = true;
            }
        } else if let Some(body) = message.1 {
            if mode != FastMode::Votor || body.block.sequence != height {
                return Err(invalid("unjustified fast view-change body"));
            }
            if proposal.is_some_and(|selected| selected.to_digest() == body.to_digest()) {
                proposed_without_qc = true;
            }
        }
    }
    if best != *high_qc
        || !has_body
        || proposal.is_some_and(|body| {
            if let Some(qc) = high_qc {
                body.to_digest() != qc.digest || body.block.sequence != height
            } else {
                mode != FastMode::Votor || !proposed_without_qc
            }
        })
    {
        return Err(invalid("unjustified fast start view"));
    }
    Ok(())
}

pub fn verify_chained_qc(
    qc: &crate::consensus::chained::ChainedQC,
    manifest: &SessionManifest,
) -> io::Result<()> {
    use crate::consensus::chained::{ChainedMessage, ChainedQC};
    if *qc == ChainedQC::genesis() {
        return Ok(());
    }
    let n = manifest.validators.len() as u64;
    if qc.voters.len() != qc.signatures.len()
        || (qc.voters.len() as u64) < crate::consensus::quorum_size(n)
        || qc.voters.windows(2).any(|pair| pair[0] >= pair[1])
        || qc.voters.iter().any(|id| *id >= n)
    {
        return Err(invalid("invalid chained certificate quorum"));
    }
    for (&signer, signature) in qc.voters.iter().zip(&qc.signatures) {
        let message = ConsensusMessage::Chained(ChainedMessage::Vote {
            view: qc.view,
            height: qc.height,
            digest: qc.digest,
            parent_digest: qc.parent_digest,
        });
        let key = manifest.verifier(true, signer)?;
        let bytes = signing_bytes(
            CONSENSUS_DOMAIN,
            manifest.chain_id_bytes()?,
            &(manifest.consensus.as_str(), &message),
        )?;
        verify(&key, &bytes, signature)?;
    }
    Ok(())
}

pub fn verify_chained_start_view(
    view: u64,
    high_qc: &crate::consensus::chained::ChainedQC,
    branch_digest: &crate::blockchain::Digest,
    proofs: &[crate::consensus::chained::ChainedNewViewProof],
    manifest: &SessionManifest,
) -> io::Result<()> {
    let n = manifest.validators.len() as u64;
    if proofs.len() < crate::consensus::quorum_size(n) as usize
        || proofs.len() > n as usize
        || proofs
            .windows(2)
            .any(|pair| pair[0].signer >= pair[1].signer)
    {
        return Err(invalid("invalid chained new-view quorum"));
    }
    let mut best = None;
    for proof in proofs {
        if proof.view != view {
            return Err(invalid("wrong chained view"));
        }
        verify_chained_qc(&proof.high_qc, manifest)?;
        let key = manifest.verifier(true, proof.signer)?;
        let bytes = chained_new_view_bytes(view, &proof.high_qc, &proof.branch_digest, manifest)?;
        verify(&key, &bytes, &proof.signature)?;
        let candidate = &proof.high_qc;
        if best
            .as_ref()
            .is_none_or(|current: &crate::consensus::chained::ChainedQC| {
                (candidate.height, candidate.view, candidate.digest)
                    > (current.height, current.view, current.digest)
            })
        {
            best = Some(candidate.clone());
        }
    }
    if best.as_ref() != Some(high_qc) {
        return Err(invalid("chained high QC is not justified"));
    }
    if !proofs
        .iter()
        .any(|proof| &proof.high_qc == high_qc && &proof.branch_digest == branch_digest)
    {
        return Err(invalid(
            "chained branch is not justified by a new-view proof",
        ));
    }
    Ok(())
}

fn chained_new_view_bytes(
    view: u64,
    high_qc: &crate::consensus::chained::ChainedQC,
    branch_digest: &crate::blockchain::Digest,
    manifest: &SessionManifest,
) -> io::Result<Vec<u8>> {
    signing_bytes(
        CONSENSUS_DOMAIN,
        manifest.chain_id_bytes()?,
        &(
            "chained-new-view/v1",
            manifest.consensus.as_str(),
            view,
            high_qc,
            branch_digest,
        ),
    )
}

fn consensus_message_bytes(
    message: &ConsensusMessage,
    manifest: &SessionManifest,
) -> io::Result<Vec<u8>> {
    if let ConsensusMessage::Chained(crate::consensus::chained::ChainedMessage::NewView {
        view,
        high_qc,
        branch_digest,
        ..
    }) = message
    {
        return chained_new_view_bytes(*view, high_qc, branch_digest, manifest);
    }
    signing_bytes(
        CONSENSUS_DOMAIN,
        manifest.chain_id_bytes()?,
        &(manifest.consensus.as_str(), message),
    )
}

pub fn verify_hotstuff_start_view(
    view: u64,
    high_qc: &QuorumCertificate,
    proofs: &[SignedConsensusMessage],
    manifest: &SessionManifest,
) -> io::Result<()> {
    let n = manifest.validators.len() as u64;
    if proofs.len() < crate::consensus::quorum_size(n) as usize
        || proofs.len() > n as usize
        || proofs
            .windows(2)
            .any(|pair| pair[0].signer >= pair[1].signer)
    {
        return Err(invalid("invalid HotStuff new-view quorum"));
    }
    let mut best: Option<&QuorumCertificate> = None;
    for proof in proofs {
        let ConsensusMessage::Hotstuff(HotStuffMessage::NewView {
            view: requested,
            high_qc: candidate,
        }) = &proof.message
        else {
            return Err(invalid("unrelated HotStuff new-view proof"));
        };
        if *requested != view {
            return Err(invalid("wrong HotStuff view"));
        }
        verify_consensus(proof, manifest)?;
        let rank = |qc: &QuorumCertificate| {
            (
                qc.height,
                qc.view,
                match qc.phase {
                    crate::consensus::hotstuff::Phase::Prepare => 0,
                    crate::consensus::hotstuff::Phase::PreCommit => 1,
                    crate::consensus::hotstuff::Phase::Commit => 2,
                },
                qc.digest,
            )
        };
        if best.is_none_or(|current| rank(candidate) > rank(current)) {
            best = Some(candidate);
        }
    }
    if best != Some(high_qc) {
        return Err(invalid("new-view high QC is not justified"));
    }
    Ok(())
}

pub fn verify_commit_certificate(
    block: &Block,
    certificate: &CommitCertificate,
    manifest: &SessionManifest,
) -> io::Result<()> {
    let n = manifest.validators.len() as u64;
    let quorum = crate::consensus::quorum_size(n);
    match (manifest.consensus.as_str(), certificate) {
        (
            "pbft",
            CommitCertificate::Pbft {
                view,
                sequence,
                digest,
                proofs,
            },
        ) => {
            if *sequence != block.sequence
                || *digest != block.to_digest()
                || proofs.len() < quorum as usize
                || proofs.len() > n as usize
                || proofs
                    .windows(2)
                    .any(|pair| pair[0].signer >= pair[1].signer)
            {
                return Err(invalid("invalid PBFT commit certificate"));
            }
            for proof in proofs {
                if !matches!(&proof.message, ConsensusMessage::Pbft(PbftMessage::Commit {
                    view: vote_view, sequence: vote_sequence, digest: vote_digest,
                }) if vote_view == view && vote_sequence == sequence && vote_digest == digest)
                {
                    return Err(invalid("unrelated PBFT commit vote"));
                }
                verify_consensus(proof, manifest)?;
            }
        }
        ("hotstuff", CommitCertificate::Hotstuff { qc }) => {
            if manifest.chained_hotstuff {
                return Err(invalid("classic HotStuff certificate in chained session"));
            }
            if qc.phase != Phase::Commit
                || qc.height != block.sequence
                || qc.digest != block.to_digest()
                || *qc == QuorumCertificate::genesis()
            {
                return Err(invalid("invalid HotStuff commit certificate"));
            }
            verify_hotstuff_qc(qc, manifest)?;
        }
        ("hotstuff", CommitCertificate::Chained { qcs }) => {
            if !manifest.chained_hotstuff
                || qcs[0].digest != block.to_digest()
                || qcs[0].height != block.sequence
                || qcs[1].parent_digest != qcs[0].digest
                || qcs[2].parent_digest != qcs[1].digest
                || qcs[0].height.checked_add(1) != Some(qcs[1].height)
                || qcs[1].height.checked_add(1) != Some(qcs[2].height)
                || qcs[0].view > qcs[1].view
                || qcs[1].view > qcs[2].view
            {
                return Err(invalid("invalid chained commit ancestry"));
            }
            for qc in qcs {
                verify_chained_qc(qc, manifest)?;
            }
        }
        ("hotstuff2", CommitCertificate::Hotstuff2 { prepare, finality }) => {
            use crate::consensus::fast::{FastMode, VoteKind};
            if prepare.kind != VoteKind::Prepare
                || finality.kind != VoteKind::Final
                || prepare.view != finality.view
                || prepare.height != block.sequence
                || finality.height != block.sequence
                || prepare.digest != block.to_digest()
                || finality.digest != prepare.digest
            {
                return Err(invalid("invalid HotStuff-2 ancestry"));
            }
            verify_fast_qc(prepare, manifest, FastMode::Hotstuff2)?;
            verify_fast_qc(finality, manifest, FastMode::Hotstuff2)?;
        }
        ("votor", CommitCertificate::Votor { notar, finality }) => {
            use crate::consensus::fast::{FastMode, VoteKind};
            if finality.height != block.sequence || finality.digest != block.to_digest() {
                return Err(invalid("invalid Votor finality"));
            }
            match notar {
                None if finality.kind == VoteKind::Notarize
                    && finality.voters.len() as u64 >= (4 * n + 4) / 5 => {}
                Some(notar)
                    if notar.kind == VoteKind::Notarize
                        && finality.kind == VoteKind::Final
                        && notar.height == finality.height
                        && notar.view == finality.view
                        && notar.digest == finality.digest =>
                {
                    verify_fast_qc(notar, manifest, FastMode::Votor)?
                }
                _ => return Err(invalid("invalid Votor finality path")),
            }
            verify_fast_qc(finality, manifest, FastMode::Votor)?;
        }
        _ => return Err(invalid("commit certificate algorithm mismatch")),
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedTransaction {
    pub transaction: Transaction,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedConsensusMessage {
    pub signer: u64,
    pub message: ConsensusMessage,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerHandshake {
    pub stage: String,
    pub node_id: u64,
    pub nonce: String,
    pub echo: String,
    pub signature: String,
}

pub fn random_nonce() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    OsRng.try_fill_bytes(&mut bytes).map_err(io::Error::other)?;
    Ok(hex_encode(&bytes))
}

pub fn sign_handshake(
    stage: &str,
    node_id: u64,
    nonce: &str,
    echo: &str,
    manifest: &SessionManifest,
    key: &SigningKey,
) -> io::Result<PeerHandshake> {
    if key.verifying_key() != manifest.verifier(true, node_id)? {
        return Err(invalid("wrong peer key"));
    }
    let _: [u8; 32] = hex_decode(nonce)?;
    if !echo.is_empty() {
        let _: [u8; 32] = hex_decode(echo)?;
    }
    let bytes = signing_bytes(
        PEER_DOMAIN,
        manifest.chain_id_bytes()?,
        &(stage, node_id, nonce, echo),
    )?;
    Ok(PeerHandshake {
        stage: stage.into(),
        node_id,
        nonce: nonce.into(),
        echo: echo.into(),
        signature: hex_encode(&key.sign(&bytes).to_bytes()),
    })
}

pub fn verify_handshake(
    handshake: &PeerHandshake,
    stage: &str,
    manifest: &SessionManifest,
) -> io::Result<()> {
    if handshake.stage != stage {
        return Err(invalid("wrong peer handshake stage"));
    }
    let _: [u8; 32] = hex_decode(&handshake.nonce)?;
    if !handshake.echo.is_empty() {
        let _: [u8; 32] = hex_decode(&handshake.echo)?;
    }
    let key = manifest.verifier(true, handshake.node_id)?;
    let bytes = signing_bytes(
        PEER_DOMAIN,
        manifest.chain_id_bytes()?,
        &(
            handshake.stage.as_str(),
            handshake.node_id,
            handshake.nonce.as_str(),
            handshake.echo.as_str(),
        ),
    )?;
    verify(&key, &bytes, &handshake.signature)
}
