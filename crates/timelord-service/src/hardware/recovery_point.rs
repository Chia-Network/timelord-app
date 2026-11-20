//! The conservative operating point used when a board's state is unknown or it
//! is recovering. It may sit below optimization preferences without editing them.
use super::*;

/// Ceiling for a cautious start when no safety event has lowered it.
pub(crate) const CAUTIOUS_CEILING_MHZ: f64 = 1450.;

pub(super) fn recovery_point(shared: &Shared, board: &BoardConfig) -> Point {
    let (frequency_mhz, voltage_v) = board.operating_point();
    let ceiling = shared
        .runtime
        .lock()
        .expect("runtime")
        .safety
        .clone()
        .zip(board.serial.as_ref())
        .and_then(|(store, serial)| store.board(serial))
        .and_then(|record| record.recovery_ceiling_mhz)
        .unwrap_or(CAUTIOUS_CEILING_MHZ);
    Point {
        frequency_mhz: frequency_mhz.min(ceiling).clamp(200., CAUTIOUS_CEILING_MHZ),
        voltage_v: voltage_v.clamp(0.7, 1.),
    }
}
