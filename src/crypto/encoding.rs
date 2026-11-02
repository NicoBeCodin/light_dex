use super::*;

pub(super) fn signing_bytes<T: Serialize>(
    domain: &[u8],
    chain_id: [u8; 32],
    value: &T,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    bytes.extend_from_slice(domain);
    bytes.extend_from_slice(&chain_id);
    encode_value(
        &serde_json::to_value(value).map_err(io::Error::other)?,
        &mut bytes,
    )?;
    Ok(bytes)
}

pub(super) fn encode_value<W: Write>(value: &Value, out: &mut W) -> io::Result<()> {
    match value {
        Value::Null => out.write_all(&[0]),
        Value::Bool(value) => out.write_all(&[1, u8::from(*value)]),
        Value::Number(value) => {
            let number = value
                .as_u64()
                .ok_or_else(|| invalid("only unsigned integers are signable"))?;
            out.write_all(&[2])?;
            out.write_all(&number.to_be_bytes())
        }
        Value::String(value) => {
            out.write_all(&[3])?;
            length_bytes(out, value.as_bytes())
        }
        Value::Array(values) => {
            out.write_all(&[4])?;
            out.write_all(&(values.len() as u64).to_be_bytes())?;
            for value in values {
                encode_value(value, out)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            out.write_all(&[5])?;
            out.write_all(&(values.len() as u64).to_be_bytes())?;
            let mut sorted = values.iter().collect::<Vec<_>>();
            sorted.sort_by_key(|(key, _)| *key);
            for (key, value) in sorted {
                length_bytes(out, key.as_bytes())?;
                encode_value(value, out)?;
            }
            Ok(())
        }
    }
}

fn length_bytes<W: Write>(out: &mut W, bytes: &[u8]) -> io::Result<()> {
    out.write_all(&(bytes.len() as u64).to_be_bytes())?;
    out.write_all(bytes)
}

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
