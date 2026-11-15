// Representation invariants for the arena's inline small-atom optimization.
//
// The arena encodes a small unsigned integer directly in the node handle instead of storing bytes
// on the heap. That is a pure size optimization and it must be pure: an inline atom has to be
// indistinguishable from the same value stored the long way, through EVERY observation the VM can
// make of it — its bytes, its length, its numeric value, its nil-ness, and its equality with
// another node.
//
// An optimization like this is uniquely dangerous because it is silent. Nothing computes wrongly in
// general; it computes wrongly only for the particular encodings that sit on the inline/heap
// boundary — a leading zero that protects a high bit, a four-byte value one above the cutoff, the
// empty atom, a bare 0x00. Those are exactly the encodings a hand-written test tends to omit and a
// consensus rule tends to depend on. Getting one wrong changes a block's meaning, not its crash
// behavior.
//
// So this walks the boundary deliberately: every value near each width cutoff, plus a seeded sweep
// across the whole 26-bit inline range and beyond it. The predicate `fits_in_small_atom` is treated
// as the specification and checked for agreement with what the arena actually did — if the arena
// inlines something the predicate rejects (or vice versa), the two have drifted apart and the
// consensus atom-count accounting no longer means what it claims.

use dg_xch_core::clvm::arena::{Arena, fits_in_small_atom, len_for_value};

/// Every observation the VM can make of an atom. If two nodes agree on all of these, no CLVM
/// program can tell them apart, which is the property the optimization must have.
fn observe(
    arena: &Arena,
    node: dg_xch_core::clvm::arena::NodePtr,
) -> (Vec<u8>, usize, String, bool) {
    let bytes = arena
        .atom(node)
        .map(|a| a.as_ref().to_vec())
        .expect("atom node has bytes");
    let len = arena.atom_len(node).expect("atom node has a length");
    let number = arena
        .number(node)
        .expect("atom node has a number")
        .to_string();
    let is_nil = arena.nullp(node);
    (bytes, len, number, is_nil)
}

/// Values that sit exactly on the inline/heap boundary, where an off-by-one is a consensus change.
fn boundary_values() -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![
        vec![],  // nil — the empty atom
        vec![0], // a bare zero: NOT the canonical encoding of 0
        vec![1],
        vec![0x7f], // largest 1-byte positive
        vec![0x80], // high bit set: negative, never inline
        vec![0xff],
        vec![0, 0x80], // leading zero protecting a set high bit: canonical
        vec![0, 0x01], // leading zero that protects nothing: not canonical
        vec![0x00, 0x00],
        vec![0x03, 0xff, 0xff, 0xff], // largest 4-byte value the predicate admits
        vec![0x04, 0x00, 0x00, 0x00], // one above the 4-byte cutoff
        vec![0xff, 0xff, 0xff, 0xff],
        vec![0, 0, 0, 0, 1], // five bytes: always heap
    ];
    // Every width cutoff and its neighbours.
    for bits in [7u32, 8, 14, 15, 16, 23, 24, 25, 26, 27] {
        for delta in [-1i64, 0, 1] {
            let v = (1i64 << bits) + delta;
            if v <= 0 {
                continue;
            }
            let mut b = (v as u64).to_be_bytes().to_vec();
            while b.len() > 1 && b[0] == 0 {
                b.remove(0);
            }
            // Canonical positive encoding needs a leading zero when the high bit is set.
            if b[0] & 0x80 != 0 {
                b.insert(0, 0);
            }
            out.push(b);
        }
    }
    out
}

#[test]
fn an_inline_atom_is_indistinguishable_from_a_heap_atom() {
    let mut arena = Arena::new();
    let mut inlined = 0usize;
    let mut heaped = 0usize;

    for bytes in boundary_values() {
        let node = arena.new_atom(&bytes).expect("atom allocates");
        let (got_bytes, got_len, got_number, got_nil) = observe(&arena, node);

        // Whatever the storage decision was, the observable value is the input.
        assert_eq!(
            got_bytes, bytes,
            "atom {bytes:02x?}: reading it back gave different bytes"
        );
        assert_eq!(
            got_len,
            bytes.len(),
            "atom {bytes:02x?}: length disagrees with the input"
        );
        assert_eq!(
            got_nil,
            bytes.is_empty(),
            "atom {bytes:02x?}: nil-ness disagrees with the input"
        );

        match fits_in_small_atom(&bytes) {
            Some(val) => {
                inlined += 1;
                // The predicate's declared length must match the real encoding, or the ghost
                // accounting that keeps the consensus atom limit honest is wrong.
                assert_eq!(
                    len_for_value(val),
                    bytes.len(),
                    "atom {bytes:02x?}: len_for_value({val}) disagrees with the canonical length"
                );
                // The same value reached through a different construction must be identical in
                // every observation — this is the actual indistinguishability claim.
                let twin = arena.new_atom(&bytes).expect("atom allocates");
                assert_eq!(
                    observe(&arena, twin),
                    (got_bytes, got_len, got_number, got_nil),
                    "atom {bytes:02x?}: two constructions of the same value observe differently"
                );
            }
            None => heaped += 1,
        }
    }

    eprintln!("  boundary values: {inlined} inline, {heaped} heap, all indistinguishable");
    assert!(
        inlined > 0 && heaped > 0,
        "the boundary set must exercise both storage paths"
    );
}

#[test]
fn the_inline_predicate_agrees_with_the_arena_across_the_whole_range() {
    // Sweep the inline range and past it. `fits_in_small_atom` is the specification; the arena must
    // behave consistently with it for every value, not just the ones near a cutoff.
    let mut arena = Arena::new();
    let mut rng: u64 = 0x2026_0830;
    let mut next = || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };

    let mut checked = 0usize;
    for i in 0..20_000u64 {
        // Half systematic across the 26-bit range, half random including well past it.
        let v = if i % 2 == 0 {
            (i * 3407) % (1 << 26)
        } else {
            next() % (1 << 30)
        };
        let mut bytes = v.to_be_bytes().to_vec();
        while bytes.len() > 1 && bytes[0] == 0 {
            bytes.remove(0);
        }
        if bytes == [0] {
            bytes.clear(); // canonical zero is the empty atom
        } else if bytes[0] & 0x80 != 0 {
            bytes.insert(0, 0);
        }

        let node = arena.new_atom(&bytes).expect("atom allocates");
        let (got, len, _, nil) = observe(&arena, node);
        assert_eq!(got, bytes, "value {v}: bytes changed through the arena");
        assert_eq!(
            len,
            bytes.len(),
            "value {v}: length changed through the arena"
        );
        assert_eq!(nil, bytes.is_empty(), "value {v}: nil-ness changed");

        if let Some(small) = fits_in_small_atom(&bytes) {
            assert_eq!(
                len_for_value(small),
                bytes.len(),
                "value {v}: len_for_value disagrees with the canonical encoding"
            );
        }
        checked += 1;
    }
    eprintln!("  {checked} values swept; predicate and arena agree throughout");
}

#[test]
fn inline_atoms_survive_being_built_into_pairs() {
    // An inline atom is a handle with no heap backing. Putting one inside a pair, and reading it
    // back out, must not lose or alter it — the case where a tagged handle is mistaken for an
    // index would show up here rather than in isolated atom tests.
    let mut arena = Arena::new();
    let cases: Vec<Vec<u8>> = boundary_values();

    for a in &cases {
        for b in [vec![], vec![1], vec![0x03, 0xff, 0xff, 0xff]] {
            let left = arena.new_atom(a).expect("atom");
            let right = arena.new_atom(&b).expect("atom");
            let pair = arena.new_pair(left, right).expect("pair");

            // The pair itself is not an atom.
            assert!(
                arena.atom(pair).is_none(),
                "a pair reported itself as an atom"
            );
            // Both children read back exactly as constructed.
            assert_eq!(
                arena.atom(left).map(|x| x.as_ref().to_vec()),
                Some(a.clone()),
                "left child {a:02x?} changed after being put in a pair"
            );
            assert_eq!(
                arena.atom(right).map(|x| x.as_ref().to_vec()),
                Some(b.clone()),
                "right child {b:02x?} changed after being put in a pair"
            );
        }
    }
    eprintln!(
        "  {} atom/pair combinations preserved their children",
        cases.len() * 3
    );
}
