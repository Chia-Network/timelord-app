use super::*;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub(super) fn route(board: &str, chain: Chain, engine: u8) -> Assignment {
    Assignment {
        board_id: board.into(),
        chain,
        engine,
    }
}

pub(super) fn work(iterations: u64, bits: usize) -> ProofRequest {
    ProofRequest {
        generation: 1,
        challenge: Bytes32::const_new([42; 32]),
        input: ClassgroupElement::get_default_element(),
        iterations,
        discriminant_bits: bits,
    }
}

pub(super) async fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (server, client)
}

pub(super) fn lease(socket: TcpStream, slot: u8, generation: u64) -> Lease {
    Lease {
        socket,
        board: "a".into(),
        slot,
        generation,
        clients: Arc::new(SyncMutex::new(BTreeSet::new())),
    }
}

pub(super) fn claimed(
    socket: TcpStream,
    slot: u8,
    generation: u64,
    clients: Arc<SyncMutex<BTreeSet<(String, u8)>>>,
) -> Lease {
    clients.lock().unwrap().insert(("a".into(), slot));
    Lease {
        socket,
        board: "a".into(),
        slot,
        generation,
        clients,
    }
}

pub(super) async fn read_initial(socket: &mut TcpStream) {
    let mut header = [0; 4];
    socket.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0], b'N');
    let count: usize = std::str::from_utf8(&header[1..]).unwrap().parse().unwrap();
    let mut body = vec![0; count + 101];
    socket.read_exact(&mut body).await.unwrap();
}

pub(super) async fn handshake(socket: &mut TcpStream) {
    read_initial(socket).await;
    socket.write_all(b"OK").await.unwrap();
}

pub(super) async fn requested(socket: &mut TcpStream) -> u64 {
    let mut length = [0; 2];
    socket.read_exact(&mut length).await.unwrap();
    let size: usize = std::str::from_utf8(&length).unwrap().parse().unwrap();
    let mut value = vec![0; size];
    socket.read_exact(&mut value).await.unwrap();
    std::str::from_utf8(&value).unwrap().parse().unwrap()
}

pub(super) async fn send_fixture_proof(socket: &mut TcpStream, request: &ProofRequest) {
    let proof = dg_xch_vdf::proof::prove_result(
        request.challenge.as_ref(),
        request.input.data.as_ref(),
        64,
        request.iterations,
    )
    .unwrap();
    let mut raw = request.iterations.to_be_bytes().to_vec();
    raw.extend(100_u64.to_be_bytes());
    raw.extend_from_slice(&proof[..100]);
    raw.push(0);
    raw.extend_from_slice(&proof[100..]);
    let hex: Vec<_> = raw
        .iter()
        .flat_map(|b| format!("{b:02x}").into_bytes())
        .collect();
    socket
        .write_all(&(hex.len() as u32).to_be_bytes())
        .await
        .unwrap();
    socket.write_all(&hex).await.unwrap();
}
