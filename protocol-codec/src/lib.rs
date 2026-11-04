use serde::Serialize;
use serde_json::Value;
use std::io::{self, Write};
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub const TRANSACTION_DOMAIN: &[u8] = b"light-dex/transaction-signature/v10";

pub fn signing_bytes<T: Serialize>(
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

pub fn encode_value<W: Write>(value: &Value, out: &mut W) -> io::Result<()> {
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

#[cfg(target_arch = "wasm32")]
mod browser {
    use super::*;
    use std::cell::RefCell;
    thread_local! { static OUTPUT: RefCell<Vec<u8>> = const {RefCell::new(Vec::new())}; }
    #[unsafe(no_mangle)]
    pub extern "C" fn allocate(length: usize) -> *mut u8 {
        let mut value = vec![0u8; length].into_boxed_slice();
        let ptr = value.as_mut_ptr();
        std::mem::forget(value);
        ptr
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn release(ptr: *mut u8, length: usize) {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, length)) });
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn encode(ptr: *const u8, length: usize) -> u32 {
        let input = unsafe { std::slice::from_raw_parts(ptr, length) };
        let result = (|| -> io::Result<Vec<u8>> {
            let input: Value = serde_json::from_slice(input).map_err(io::Error::other)?;
            let chain = input["chain"]
                .as_array()
                .ok_or_else(|| invalid("missing chain"))?;
            if chain.len() != 32 {
                return Err(invalid("invalid chain"));
            }
            let mut id = [0u8; 32];
            for (out, byte) in id.iter_mut().zip(chain) {
                *out = u8::try_from(byte.as_u64().ok_or_else(|| invalid("invalid chain byte"))?)
                    .map_err(io::Error::other)?;
            }
            signing_bytes(TRANSACTION_DOMAIN, id, &input["transaction"])
        })();
        match result {
            Ok(bytes) => {
                OUTPUT.with(|v| *v.borrow_mut() = bytes);
                1
            }
            Err(_) => {
                OUTPUT.with(|v| v.borrow_mut().clear());
                0
            }
        }
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn output_ptr() -> *const u8 {
        OUTPUT.with(|v| v.borrow().as_ptr())
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn output_len() -> usize {
        OUTPUT.with(|v| v.borrow().len())
    }
}
