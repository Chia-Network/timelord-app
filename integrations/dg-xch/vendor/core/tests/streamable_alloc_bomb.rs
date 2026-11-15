//! Allocation-bomb hardening — the streamable decoder must never pre-allocate an attacker-claimed
//! length before the reader is proven to hold that many bytes.
//!
//! A length-prefixed field (`String`, byte `Vec<u8>`, generic `Vec<T>`, `HashMap<K,V>`) carries a
//! u32-BE count, so a hostile peer can claim up to `0xFFFF_FFFF` (~4 GiB) in a few bytes.
//! Every inbound p2p message is streamable-decoded, so pre-allocating the claimed length
//! would let a single peer OOM the node. The decoder (`serialize/src/lib.rs`) bounds the
//! declared length against the bytes actually remaining before allocating.
//!
//! Asserting only that an over-claim returns `Err` is an insufficient regression lock: an
//! unguarded `vec![0u8; claimed]` would still succeed on a large-RAM host, get zero-filled,
//! and *then* fail the trailing `read_exact` — the decode still returns `Err` while the
//! node is OOM-vulnerable. The only lock that actually catches the bomb is a positive
//! bound on bytes allocated *during* the decode. This file installs a counting global
//! allocator and asserts exactly that: decoding an over-claim allocates a bounded, tiny
//! amount — never anything close to the claimed length.

use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// --- counting global allocator ---------------------------------------------
//
// Tracks live bytes and the peak live-bytes watermark. `vec![0u8; n]` reaches the allocator via
// `alloc_zeroed`, whose default `GlobalAlloc` impl calls `self.alloc`; `with_capacity`/`reserve`
// reach `self.alloc`/`self.realloc` (whose default impl also routes through `self.alloc`). All
// paths are therefore counted. The hooks touch only atomics — no locks — so they cannot deadlock
// the allocator.

struct Tracking;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarding to the System allocator with the same layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `self.alloc` with this `layout`.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static GLOBAL: Tracking = Tracking;

// Serialize the measured sections against each other so one measured decode's peak is not inflated
// by another's. Unmeasured tests only ever allocate a few KiB, far below the assertion bound, so
// they need not take this lock.
static MEASURE_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` and return its result plus the peak bytes allocated *during* the call (delta over the
/// live watermark at entry). The peak watermark is reset to the current live total at entry, so the
/// returned figure is attributable to `f` alone.
fn peak_alloc_during<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let _guard = MEASURE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    let peak = PEAK.load(Ordering::Relaxed);
    (out, peak.saturating_sub(base))
}

const V: ChiaProtocolVersion = ChiaProtocolVersion::Chia0_0_37;

// A hostile u32 count (~4.29 GiB / ~4.29e9 elements). If any guarded path ever pre-allocated this,
// the peak-bytes assertions below would balloon into the GiB range (or OOM the runner).
const HOSTILE_LEN: u32 = 0xFFFF_FFFF;

// Any decode that fails fast should not allocate more than a small working budget. The bomb is
// >4 GiB; legitimate concurrent test noise is a few KiB. 16 MiB sits comfortably between the two,
// so this bound is robust in both directions.
const ALLOC_BOUND: usize = 16 * 1024 * 1024;

fn decode<T: ChiaSerialize>(bytes: &[u8]) -> Result<T, std::io::Error> {
    let mut cur = Cursor::new(bytes);
    T::from_bytes(&mut cur, V)
}

// --- positive bounded-allocation evidence ----------------------------------

#[test]
fn overclaim_string_allocates_bounded() {
    // u32 length claims ~4 GiB; only two payload bytes follow.
    let mut input = HOSTILE_LEN.to_be_bytes().to_vec();
    input.extend_from_slice(b"ab");
    let (res, peak) = peak_alloc_during(|| decode::<String>(&input));
    assert!(res.is_err(), "over-claimed String must reject");
    assert!(
        peak < ALLOC_BOUND,
        "String over-claim allocated {peak} bytes (> {ALLOC_BOUND}); the length guard is not \
         bounding the pre-allocation (allocation-bomb regression)"
    );
}

#[test]
fn overclaim_byte_vec_allocates_bounded() {
    let mut input = HOSTILE_LEN.to_be_bytes().to_vec();
    input.extend_from_slice(b"ab");
    let (res, peak) = peak_alloc_during(|| decode::<Vec<u8>>(&input));
    assert!(res.is_err(), "over-claimed Vec<u8> must reject");
    assert!(
        peak < ALLOC_BOUND,
        "Vec<u8> over-claim allocated {peak} bytes (> {ALLOC_BOUND}) (allocation-bomb regression)"
    );
}

#[test]
fn overclaim_typed_vec_allocates_bounded() {
    // A `Vec<u32>` claiming ~4.29e9 elements with a 4-byte body: the element loop must fail on the
    // first short read, never `with_capacity(claimed)` a multi-GiB backing store.
    let mut input = HOSTILE_LEN.to_be_bytes().to_vec();
    input.extend_from_slice(&[0u8; 4]);
    let (res, peak) = peak_alloc_during(|| decode::<Vec<u32>>(&input));
    assert!(res.is_err(), "over-claimed Vec<u32> must reject");
    assert!(
        peak < ALLOC_BOUND,
        "Vec<u32> over-claim allocated {peak} bytes (> {ALLOC_BOUND}) (allocation-bomb regression)"
    );
}

#[test]
fn overclaim_hashmap_allocates_bounded() {
    // The map decoder must not `with_capacity(claimed)` — a garbage u32 would size a multi-GiB hash
    // table before a single entry is read. It must grow from empty and fail on the first short key.
    let mut input = HOSTILE_LEN.to_be_bytes().to_vec();
    input.extend_from_slice(&[0u8; 2]);
    let (res, peak) = peak_alloc_during(|| decode::<HashMap<u32, u32>>(&input));
    assert!(res.is_err(), "over-claimed HashMap must reject");
    assert!(
        peak < ALLOC_BOUND,
        "HashMap over-claim allocated {peak} bytes (> {ALLOC_BOUND}) (allocation-bomb regression)"
    );
}

// --- exact boundary: claimed == remaining OK, claimed == remaining + 1 Err --

#[test]
fn string_length_exact_boundary() {
    // claimed == remaining: decodes.
    let mut ok = 3u32.to_be_bytes().to_vec();
    ok.extend_from_slice(b"abc");
    assert_eq!(decode::<String>(&ok).unwrap(), "abc");

    // claimed == remaining + 1: rejected without over-reading.
    let mut over = 4u32.to_be_bytes().to_vec();
    over.extend_from_slice(b"abc");
    assert!(
        decode::<String>(&over).is_err(),
        "declared 4 with 3 present must reject"
    );
}

#[test]
fn byte_vec_length_exact_boundary() {
    let mut ok = 3u32.to_be_bytes().to_vec();
    ok.extend_from_slice(b"xyz");
    assert_eq!(decode::<Vec<u8>>(&ok).unwrap(), b"xyz".to_vec());

    let mut over = 4u32.to_be_bytes().to_vec();
    over.extend_from_slice(b"xyz");
    assert!(
        decode::<Vec<u8>>(&over).is_err(),
        "declared 4 with 3 present must reject"
    );
}

// --- regression: valid inputs round-trip byte-identically -------------------
//
// The guard is on the p2p/consensus decode path, so ANY behavior change to valid inputs is
// unacceptable. Only invalid over-claims may be rejected; legitimate values must still decode and
// re-encode byte-for-byte.

#[test]
fn legit_values_round_trip_byte_identically() {
    // Ordered wire types must re-encode byte-for-byte (the p2p/consensus decode path).
    fn check<T>(value: T)
    where
        T: ChiaSerialize + PartialEq + std::fmt::Debug,
    {
        let bytes = value.to_bytes(V).unwrap();
        let back = T::from_bytes_exact(&bytes, V).unwrap();
        assert_eq!(back, value, "structural round-trip");
        assert_eq!(back.to_bytes(V).unwrap(), bytes, "byte-identical re-encode");
    }
    check(String::new());
    check("hello world".to_string());
    check("b".repeat(4096)); // a legitimately large-but-honest String must still decode
    check(Vec::<u8>::new());
    check(vec![0u8, 1, 2, 254, 255]);
    check(vec![0u32, 1, 4_294_967_295]);
    check((7u32, "mixed".to_string(), vec![9u8, 8, 7]));

    // `HashMap::to_bytes` iterates in arbitrary order, so its wire encoding is not canonical and a
    // byte-identical re-encode is not guaranteed — only that a decode reconstructs the same map.
    // (The over-claim HashMap allocation guard is exercised by `overclaim_hashmap_allocates_bounded`.)
    let mut map: HashMap<u32, u32> = HashMap::new();
    map.insert(1, 10);
    map.insert(2, 20);
    map.insert(3, 30);
    let bytes = map.to_bytes(V).unwrap();
    assert_eq!(
        HashMap::<u32, u32>::from_bytes_exact(&bytes, V).unwrap(),
        map,
        "HashMap structural round-trip"
    );
}

// --- property sweep: an over-claim never allocates beyond the input ---------
//
// The (claimed_len, actual_len) property the guard enforces: whenever `claimed > actual`, the decode
// rejects and allocates a bounded amount (never proportional to `claimed`). A deterministic sweep
// over the claim/actual grid.

#[test]
fn overclaim_sweep_bounds_allocation_for_all_claims() {
    // A spread of hostile claims: small over-claims through the full 4 GiB u32.
    let claims: [u32; 8] = [
        1,
        2,
        16,
        1_000,
        1_000_000,
        100_000_000,
        2_000_000_000,
        HOSTILE_LEN,
    ];
    // A spread of actual payload sizes, all strictly smaller than every claim above except the
    // trivial `claimed == actual` case, which is excluded (actual < claim is the property).
    let actuals: [usize; 4] = [0, 1, 3, 7];

    for &claimed in &claims {
        for &actual in &actuals {
            if (claimed as usize) <= actual {
                continue; // only exercise genuine over-claims (claimed > actual)
            }
            let mut input = claimed.to_be_bytes().to_vec();
            input.extend(std::iter::repeat_n(0x61u8, actual));

            // String
            let (res, peak) = peak_alloc_during(|| decode::<String>(&input));
            assert!(
                res.is_err(),
                "String claimed={claimed} actual={actual}: over-claim must reject"
            );
            assert!(
                peak < ALLOC_BOUND,
                "String claimed={claimed} actual={actual}: allocated {peak} bytes (> {ALLOC_BOUND})"
            );

            // Vec<u8>
            let (res, peak) = peak_alloc_during(|| decode::<Vec<u8>>(&input));
            assert!(
                res.is_err(),
                "Vec<u8> claimed={claimed} actual={actual}: over-claim must reject"
            );
            assert!(
                peak < ALLOC_BOUND,
                "Vec<u8> claimed={claimed} actual={actual}: allocated {peak} bytes (> {ALLOC_BOUND})"
            );
        }
    }
}
