use super::*;

pub fn new_key() -> io::Result<SigningKey> {
    let mut secret = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut secret)
        .map_err(io::Error::other)?;
    Ok(SigningKey::from_bytes(&secret))
}

pub fn write_secret(path: &Path, key: &SigningKey) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(hex_encode(&key.to_bytes()).as_bytes())?;
    file.sync_all()
}

pub fn read_secret(path: &Path) -> io::Result<SigningKey> {
    let text = fs::read_to_string(path)?;
    Ok(SigningKey::from_bytes(&hex_decode(text.trim())?))
}

pub fn create_session(path: &Path, consensus: &str, nodes: u64) -> io::Result<SessionManifest> {
    create_session_with_faucet(path, consensus, nodes, FaucetTerms::default())
}

pub fn create_session_with_faucet(
    path: &Path,
    consensus: &str,
    nodes: u64,
    terms: FaucetTerms,
) -> io::Result<SessionManifest> {
    create_session_with_options(path, consensus, nodes, terms, None)
}

pub fn create_session_with_options(
    path: &Path,
    consensus: &str,
    nodes: u64,
    terms: FaucetTerms,
    auto_view_change_blocks: Option<u64>,
) -> io::Result<SessionManifest> {
    create_session_with_mode(
        path,
        consensus,
        nodes,
        terms,
        auto_view_change_blocks,
        consensus == "hotstuff",
    )
}

pub fn create_session_with_mode(
    path: &Path,
    consensus: &str,
    nodes: u64,
    terms: FaucetTerms,
    auto_view_change_blocks: Option<u64>,
    chained_hotstuff: bool,
) -> io::Result<SessionManifest> {
    if nodes == 0 || nodes > 1_000 {
        return Err(invalid("invalid validator count"));
    }
    if auto_view_change_blocks == Some(0) {
        return Err(invalid("automatic view-change interval must be positive"));
    }
    fs::create_dir_all(path)?;
    let keys = path.join("keys");
    fs::create_dir(&keys)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&keys, fs::Permissions::from_mode(0o700))?;
    }
    let mut nonce = [0u8; 32];
    OsRng.try_fill_bytes(&mut nonce).map_err(io::Error::other)?;
    let mut manifest = SessionManifest {
        version: PROTOCOL_VERSION,
        session_nonce: hex_encode(&nonce),
        consensus: consensus.into(),
        chained_hotstuff,
        initial_state_root: String::new(),
        validators: BTreeMap::new(),
        accounts: BTreeSet::new(),
        faucet: FaucetConfig {
            max_claims: terms.max_claims,
            usd_per_claim: terms.usd_per_claim,
            btc_per_claim: terms.btc_per_claim,
        },
        auto_view_change_blocks,
        chain_id: String::new(),
    };
    manifest.faucet.validate().map_err(io::Error::other)?;
    for id in 0..nodes {
        let key = new_key()?;
        manifest
            .validators
            .insert(id, hex_encode(&key.verifying_key().to_bytes()));
        write_secret(&keys.join(format!("validator-{id}.key")), &key)?;
    }
    let wallet = BTreeMap::<u64, Address>::new();
    manifest.initial_state_root = hex_encode(
        &ExchangeState::with_claim_faucet(manifest.faucet)
            .map_err(io::Error::other)?
            .state_digest(),
    );
    let mut wallet_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join("wallet.json"))?;
    wallet_file.write_all(&serde_json::to_vec_pretty(&wallet).map_err(io::Error::other)?)?;
    wallet_file.sync_all()?;
    manifest.chain_id = hex_encode(&manifest.calculated_id()?);
    manifest.validate()?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join("manifest.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        fs::File::open(&keys)?.sync_all()?;
        fs::File::open(path)?.sync_all()?;
    }
    Ok(manifest)
}
