use super::*;
pub(super) use light_dex_protocol_codec::{encode_value, signing_bytes};

pub(super) fn verify(key: &VerifyingKey, bytes: &[u8], hex: &str) -> io::Result<()> {
    let decoded = hex_decode::<64>(hex)?;
    if hex.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(invalid("noncanonical signature encoding"));
    }
    super::cache::verify_cached(key, bytes, &decoded)
}

pub(super) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
