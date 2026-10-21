//! The native chiavdf client's TCP protocol. Parsing is not proof verification.
use std::io::{Error, ErrorKind, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const FORM_BYTES: usize = 100;
pub const CHECKPOINT_BYTES: usize = 8 + 33 + FORM_BYTES;
pub const BASE_PROOF_BYTES: usize = 8 + 8 + FORM_BYTES + 1 + FORM_BYTES;
pub const MAX_HEX_FRAME_BYTES: usize = (BASE_PROOF_BYTES + 255 * CHECKPOINT_BYTES) * 2;
// The current hw_vdf_client starts each challenge with a fixed 2B-iteration target.
pub const MAX_REQUEST_ITERATIONS: u64 = 2_000_000_000;

fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::InvalidData, message)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub iterations: u64,
    pub challenge_prime: [u8; 33],
    pub witness: [u8; FORM_BYTES],
}

/// Structurally decoded, UNVERIFIED data. Checkpoints are newest first on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiedProof {
    pub iterations: u64,
    pub output: [u8; FORM_BYTES],
    pub witness: [u8; FORM_BYTES],
    pub checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClientMessage {
    Stop,
    Proof(Box<UnverifiedProof>),
}

pub fn encode_initial(discriminant: &str, initial_form: &[u8; FORM_BYTES]) -> Result<Vec<u8>> {
    let digits = discriminant
        .strip_prefix('-')
        .ok_or_else(|| invalid("Expected a negative decimal discriminant"))?;
    if digits.is_empty()
        || !digits.bytes().all(|c| c.is_ascii_digit())
        || digits.bytes().all(|c| c == b'0')
        || discriminant.len() > 349
    {
        return Err(invalid("Invalid decimal discriminant"));
    }
    // Mode N, three ASCII length digits, signed decimal discriminant, one byte
    // form length, then the compressed form. Send as one initial handshake write.
    let mut bytes = format!("N{:03}{discriminant}", discriminant.len()).into_bytes();
    bytes.push(FORM_BYTES as u8);
    bytes.extend_from_slice(initial_form);
    Ok(bytes)
}

pub fn encode_request(iterations: u64) -> Result<Vec<u8>> {
    if iterations == 0 || iterations > MAX_REQUEST_ITERATIONS {
        return Err(invalid(
            "Proof request exceeds the native client's current challenge target",
        ));
    }
    let value = iterations.to_string();
    Ok(format!("{:02}{value}", value.len()).into_bytes())
}

pub async fn request<W: AsyncWrite + Unpin>(writer: &mut W, iterations: u64) -> Result<()> {
    writer.write_all(&encode_request(iterations)?).await
}
pub async fn request_stop<W: AsyncWrite + Unpin>(writer: &mut W) -> Result<()> {
    writer.write_all(b"010").await
}
pub async fn acknowledge_stop<W: AsyncWrite + Unpin>(writer: &mut W) -> Result<()> {
    writer.write_all(b"ACK").await
}

pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<ClientMessage> {
    let mut header = [0; 4];
    reader.read_exact(&mut header).await?;
    if &header == b"STOP" {
        return Ok(ClientMessage::Stop);
    }
    let size = u32::from_be_bytes(header) as usize;
    if !(BASE_PROOF_BYTES * 2..=MAX_HEX_FRAME_BYTES).contains(&size) || !size.is_multiple_of(2) {
        return Err(invalid("Invalid or oversized proof frame"));
    }
    let mut hex = vec![0; size];
    reader.read_exact(&mut hex).await?;
    Ok(ClientMessage::Proof(Box::new(decode_proof(&hex)?)))
}

pub fn decode_proof(hex: &[u8]) -> Result<UnverifiedProof> {
    if !(BASE_PROOF_BYTES * 2..=MAX_HEX_FRAME_BYTES).contains(&hex.len())
        || !hex.len().is_multiple_of(2)
    {
        return Err(invalid("Invalid proof length"));
    }
    fn digit(b: u8) -> Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(invalid("Non-hexadecimal proof data")),
        }
    }
    let mut raw = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_chunks::<2>().0 {
        raw.push(digit(pair[0])? << 4 | digit(pair[1])?);
    }
    let iterations = u64::from_be_bytes(raw[0..8].try_into().expect("fixed header"));
    let form_length = u64::from_be_bytes(raw[8..16].try_into().expect("fixed header"));
    if iterations == 0 || iterations > i64::MAX as u64 || form_length != FORM_BYTES as u64 {
        return Err(invalid("Invalid proof iteration count or form size"));
    }
    let count = raw[16 + FORM_BYTES] as usize;
    if raw.len() != BASE_PROOF_BYTES + count * CHECKPOINT_BYTES {
        return Err(invalid(
            "Checkpoint count does not match proof frame length",
        ));
    }
    let mut checkpoints = Vec::with_capacity(count);
    for segment in raw[BASE_PROOF_BYTES..].as_chunks::<CHECKPOINT_BYTES>().0 {
        let iterations = u64::from_be_bytes(segment[..8].try_into().expect("fixed segment"));
        if iterations == 0 || iterations > i64::MAX as u64 {
            return Err(invalid("Invalid checkpoint iteration count"));
        }
        checkpoints.push(Checkpoint {
            iterations,
            challenge_prime: segment[8..41].try_into().expect("fixed prime"),
            witness: segment[41..].try_into().expect("fixed witness"),
        });
    }
    Ok(UnverifiedProof {
        iterations,
        output: raw[16..116].try_into().expect("fixed output"),
        witness: raw[117..217].try_into().expect("fixed witness"),
        checkpoints,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hex(raw: &[u8]) -> Vec<u8> {
        raw.iter()
            .flat_map(|b| format!("{b:02x}").into_bytes())
            .collect()
    }
    fn golden_raw() -> Vec<u8> {
        // Independent fixture matching hw_vdf_client::handle_proofs: final,
        // newest checkpoint, oldest checkpoint. Intentionally not a valid VDF.
        let mut raw = 1_000_000_u64.to_be_bytes().to_vec();
        raw.extend(100_u64.to_be_bytes());
        raw.extend([0x08; 100]);
        raw.push(2);
        raw.extend([0x11; 100]);
        for (iters, prime, witness) in [(250_000_u64, 0x22, 0x33), (500_000, 0x44, 0x55)] {
            raw.extend(iters.to_be_bytes());
            raw.extend([prime; 33]);
            raw.extend([witness; 100]);
        }
        raw
    }
    #[test]
    fn handshake_matches_native_layout() {
        let mut form = [0; 100];
        form[0] = 8;
        let b = encode_initial("-23", &form).unwrap();
        assert_eq!(&b[..8], b"N003-23d");
        assert_eq!(&b[8..], &form);
        assert!(encode_initial("-0", &form).is_err());
        assert!(encode_initial("-0x17", &form).is_err());
        assert_eq!(encode_request(1_000_000).unwrap(), b"071000000");
        assert!(encode_request(0).is_err());
        assert!(encode_request(MAX_REQUEST_ITERATIONS + 1).is_err());
    }
    #[test]
    fn preserves_chained_checkpoint_order() {
        let p = decode_proof(&hex(&golden_raw())).unwrap();
        assert_eq!(p.iterations, 1_000_000);
        assert_eq!(p.output, [8; 100]);
        assert_eq!(p.checkpoints[0].iterations, 250_000);
        assert_eq!(p.checkpoints[1].iterations, 500_000);
        assert_eq!(p.checkpoints[1].challenge_prime, [0x44; 33]);
    }
    #[test]
    fn rejects_malformed_frames_before_interpretation() {
        let mut raw = golden_raw();
        raw[116] = 3;
        assert!(decode_proof(&hex(&raw)).is_err());
        raw = golden_raw();
        raw[15] = 99;
        assert!(decode_proof(&hex(&raw)).is_err());
        let mut h = hex(&golden_raw());
        h[10] = b'z';
        assert!(decode_proof(&h).is_err());
    }
    #[tokio::test]
    async fn handles_fragmented_tcp_style_reads_and_stop_after_proof() {
        let (mut tx, mut rx) = tokio::io::duplex(16);
        let hex = hex(&golden_raw());
        let writer = tokio::spawn(async move {
            tx.write_all(&(hex.len() as u32).to_be_bytes())
                .await
                .unwrap();
            for chunk in hex.chunks(7) {
                tx.write_all(chunk).await.unwrap();
            }
            tx.write_all(b"STOP").await.unwrap();
        });
        assert!(matches!(
            read_message(&mut rx).await.unwrap(),
            ClientMessage::Proof(_)
        ));
        assert_eq!(read_message(&mut rx).await.unwrap(), ClientMessage::Stop);
        writer.await.unwrap();
    }
    #[tokio::test]
    async fn rejects_oversized_length_without_waiting_for_payload() {
        let (mut tx, mut rx) = tokio::io::duplex(16);
        tx.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(read_message(&mut rx).await.is_err());
    }
}
