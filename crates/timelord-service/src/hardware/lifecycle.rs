//! Serial-scoped native-owner state. One small state machine decides whether a
//! board may receive work; status and routing read it, never mutate it directly.
//!
//! ```text
//! Idle ─begin_attempt→ Starting ─checking→ Checking ─recovered→ Connected
//!   ↑                      │                   │                    │
//!   └──────── retired ←────┴──── fault ────────┴──── fault ─────────┘
//!                         (Reconnecting | ThermalStop | InvalidProof)
//! ```
use serde::{Serialize, Serializer, ser::SerializeStruct};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Starting,
    Checking,
    Connected,
    Stopped,
    /// A transient failure; the owner restarts after its backoff.
    Reconnecting,
    ThermalStop,
    /// An invalid proof was received; a private proof check is required.
    InvalidProof,
}
impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "not_inspected",
            Self::Starting => "starting",
            Self::Checking => "checking",
            Self::Connected => "connected",
            Self::Stopped => "stopped",
            Self::Reconnecting => "reconnecting",
            Self::ThermalStop => "thermal_stop",
            Self::InvalidProof => "invalid_proof",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Transport,
    Thermal,
    InvalidProof,
}
impl Fault {
    fn restriction(self) -> Option<crate::safety::Reason> {
        match self {
            Self::Transport => None,
            Self::Thermal => Some(crate::safety::Reason::Thermal),
            Self::InvalidProof => Some(crate::safety::Reason::InvalidProof),
        }
    }
}
impl From<crate::safety::Reason> for Fault {
    fn from(reason: crate::safety::Reason) -> Self {
        match reason {
            crate::safety::Reason::Thermal => Self::Thermal,
            crate::safety::Reason::InvalidProof => Self::InvalidProof,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Native {
    #[default]
    Absent,
    Initializing,
    Ready,
    Retired {
        confirmed: bool,
    },
}
/// Immutable view. Consumers cannot mutate lifecycle by changing display state.
#[derive(Clone, Copy, Debug)]
pub struct Status<'a> {
    pub phase: Phase,
    pub generation: u64,
    pub started_unix: u64,
    pub quiesced: bool,
    pub latched_fault: bool,
    pub fault: Option<Fault>,
    pub fault_reason: Option<&'a str>,
    pub serving_since_unix: u64,
    pub evidence_unix: u64,
}
#[derive(Clone, Debug, Default)]
pub struct BoardLifecycle {
    serial: Option<String>,
    phase: Phase,
    native: Native,
    generation: u64,
    verified_generation: Option<u64>,
    verified_slots: std::collections::BTreeSet<u8>,
    started_unix: u64,
    restriction: Option<crate::safety::Reason>,
    fault_reason: Option<String>,
    usb_present: Option<bool>,
    serving_since_unix: u64,
    evidence_unix: u64,
}
impl Serialize for BoardLifecycle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("BoardLifecycle", 1)?;
        state.serialize_field("state", self.phase.as_str())?;
        state.end()
    }
}
impl BoardLifecycle {
    #[cfg(test)]
    pub fn fixture(phase: Phase, ready: bool, fault: Option<Fault>, generation: u64) -> Self {
        Self {
            phase,
            generation,
            restriction: fault.and_then(Fault::restriction),
            native: if ready { Native::Ready } else { Native::Absent },
            ..Self::default()
        }
    }
    #[cfg(test)]
    pub fn fixture_usb(
        phase: Phase,
        ready: bool,
        fault: Option<Fault>,
        usb_present: Option<bool>,
        generation: u64,
    ) -> Self {
        Self {
            usb_present,
            ..Self::fixture(phase, ready, fault, generation)
        }
    }
    pub(super) fn matches_serial(&self, serial: Option<&str>) -> bool {
        self.serial.as_deref() == serial
    }
    /// A different physical serial cannot inherit another board's state.
    pub(super) fn bind(&mut self, serial: Option<&str>) {
        if !self.matches_serial(serial) {
            *self = Self {
                serial: serial.map(str::to_owned),
                ..Self::default()
            };
        }
    }
    pub(super) fn restore_policy(&mut self, policy: crate::safety::Policy) {
        if let Some(reason) = policy.restriction {
            let message = policy.hold_reason(crate::unix_now()).unwrap_or_else(|| {
                match reason {
                    crate::safety::Reason::Thermal => {
                        "Checking for cool telemetry after reaching the temperature limit"
                    }
                    crate::safety::Reason::InvalidProof => {
                        "Rechecking proofs after an invalid proof"
                    }
                }
                .to_owned()
            });
            self.restriction = Some(reason);
            self.fault_reason = Some(message);
            if self.native != Native::Ready {
                self.phase = Self::restricted_phase(reason);
            }
        }
    }
    fn restricted_phase(reason: crate::safety::Reason) -> Phase {
        match reason {
            crate::safety::Reason::Thermal => Phase::ThermalStop,
            crate::safety::Reason::InvalidProof => Phase::InvalidProof,
        }
    }
    /// Proof workers may report only evidence from their captured generation.
    pub(crate) fn report_fault(
        &mut self,
        generation: u64,
        fault: Fault,
        reason: String,
        now: u64,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.fault(fault, reason, now);
        true
    }
    pub fn status(&self) -> Status<'_> {
        Status {
            phase: self.phase,
            generation: self.generation,
            started_unix: self.started_unix,
            quiesced: self.native == Native::Retired { confirmed: true },
            latched_fault: self.restriction.is_some(),
            fault: self.restriction.map(Fault::from),
            fault_reason: self.fault_reason.as_deref(),
            serving_since_unix: self.serving_since_unix,
            evidence_unix: self.evidence_unix,
        }
    }
    /// Only a ready, checked native owner without a safety restriction serves work.
    pub fn accepts_work(&self) -> bool {
        self.restriction.is_none() && self.native == Native::Ready && self.phase == Phase::Connected
    }
    /// The native owner must be retired: a fault was reported while it runs.
    pub fn needs_retirement(&self) -> bool {
        matches!(self.native, Native::Ready | Native::Initializing)
            && matches!(
                self.phase,
                Phase::Reconnecting | Phase::ThermalStop | Phase::InvalidProof
            )
    }
    pub(super) fn begin_attempt(&mut self, now: u64) {
        self.generation = self.generation.saturating_add(1);
        self.verified_generation = None;
        self.verified_slots.clear();
        self.started_unix = now;
        self.serving_since_unix = 0;
        self.native = Native::Initializing;
        self.phase = match self.restriction {
            Some(reason) => Self::restricted_phase(reason),
            None => {
                self.fault_reason = None;
                Phase::Starting
            }
        };
    }
    /// Proof evidence belongs to the native session that produced it. A late
    /// verification cannot certify a replacement session or another physical slot.
    pub fn verified(&mut self, generation: u64, slot: u8, now: u64) -> bool {
        if generation != self.generation || slot >= 3 {
            return false;
        }
        self.verified_generation = Some(generation);
        self.verified_slots.insert(slot);
        self.evidence_unix = self.evidence_unix.max(now);
        true
    }
    /// A private canary proof through the current owner.
    pub fn canary(&mut self, generation: u64, now: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.evidence_unix = self.evidence_unix.max(now);
        true
    }
    pub fn forget_slot(&mut self, slot: u8) {
        self.verified_slots.remove(&slot);
        if self.verified_slots.is_empty() {
            self.verified_generation = None;
        }
    }
    pub fn verified_generation(&self) -> Option<u64> {
        self.verified_generation
    }
    pub fn slot_verified(&self, slot: u8) -> bool {
        self.verified_generation == Some(self.generation) && self.verified_slots.contains(&slot)
    }
    /// Recovery checks run on every new native owner: cool telemetry, and a
    /// private proof after an invalid one.
    pub(super) fn checking(&mut self) {
        if self.restriction.is_none() {
            self.phase = Phase::Checking;
        }
    }
    pub(super) fn initialized(&mut self) {
        self.native = Native::Ready;
    }
    /// Fresh owner checks passed; the board may serve.
    pub(super) fn recovered(&mut self, now: u64) {
        self.native = Native::Ready;
        self.phase = Phase::Connected;
        self.restriction = None;
        self.fault_reason = None;
        self.usb_present = Some(true);
        self.serving_since_unix = now;
    }
    pub(super) fn fault(&mut self, fault: Fault, reason: String, _now: u64) {
        if let Some(incoming) = fault.restriction() {
            self.restriction = Some(
                self.restriction
                    .map_or(incoming, |previous| previous.merge(incoming)),
            );
        }
        self.serving_since_unix = 0;
        self.phase = match self.restriction {
            Some(reason) => Self::restricted_phase(reason),
            None => Phase::Reconnecting,
        };
        self.fault_reason = Some(reason);
    }
    pub(super) fn retired(&mut self, confirmed: bool, error: Option<String>, now: u64) {
        self.native = Native::Retired { confirmed };
        self.serving_since_unix = 0;
        if let Some(error) = error {
            self.fault(Fault::Transport, error, now);
        } else if !matches!(
            self.phase,
            Phase::Reconnecting | Phase::ThermalStop | Phase::InvalidProof
        ) {
            self.phase = Phase::Stopped;
        }
    }
    /// A closed-owner USB inventory observed this serial (or its absence).
    pub(super) fn inventory(&mut self, present: bool) {
        self.usb_present = Some(present);
    }
    /// Open FTDI handles can hide their metadata. Keep a running or known-present
    /// board in inventory until a closed-owner probe confirms its absence.
    pub fn preserve_in_inventory(&self, telemetry_fresh: bool) -> bool {
        self.restriction.is_none()
            && (self.usb_present == Some(true)
                || (self.usb_present.is_none()
                    && self.phase == Phase::Connected
                    && telemetry_fresh))
    }
}

// Explicit test construction bypasses owner access only in fixture builds.
#[cfg(test)]
pub(crate) struct FixtureOwner<'a>(&'a mut BoardLifecycle);
#[cfg(test)]
impl BoardLifecycle {
    pub(crate) fn testing(&mut self) -> FixtureOwner<'_> {
        FixtureOwner(self)
    }
}
#[cfg(test)]
impl FixtureOwner<'_> {
    pub fn begin_attempt(&mut self, now: u64) {
        self.0.begin_attempt(now);
    }
    pub fn serve(&mut self, now: u64) {
        self.0.begin_attempt(now);
        self.0.initialized();
        self.0.recovered(now);
    }
    pub fn fault(&mut self, fault: Fault, reason: String, now: u64) {
        self.0.fault(fault, reason, now);
    }
    pub fn inventory(&mut self, present: bool) {
        self.0.inventory(present);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_fault_reports_are_fenced_to_the_native_generation() {
        let mut owner = BoardLifecycle::default();
        owner.begin_attempt(1);
        let previous = owner.status().generation;
        owner.begin_attempt(2);
        assert!(!owner.report_fault(previous, Fault::InvalidProof, "late old proof".into(), 3));
        assert_eq!(owner.status().fault, None);
        assert!(owner.report_fault(
            owner.status().generation,
            Fault::InvalidProof,
            "current proof".into(),
            4
        ));
        assert_eq!(owner.status().fault, Some(Fault::InvalidProof));
        assert_eq!(owner.status().phase, Phase::InvalidProof);
    }
    #[test]
    fn thermal_restriction_wins_in_both_event_orders() {
        for order in [
            [Fault::Thermal, Fault::InvalidProof],
            [Fault::InvalidProof, Fault::Thermal],
        ] {
            let mut owner = BoardLifecycle::default();
            for fault in order {
                owner.fault(fault, "safety evidence".into(), 1);
            }
            owner.fault(Fault::Transport, "transport unavailable".into(), 2);
            owner.retired(true, None, 3);
            owner.begin_attempt(4);
            owner.initialized();
            owner.checking();
            assert_eq!(owner.status().fault, Some(Fault::Thermal));
            assert_eq!(owner.status().phase, Phase::ThermalStop);
            assert!(!owner.accepts_work());
        }
    }
    #[test]
    fn only_completed_recovery_checks_serve_and_clear_restrictions() {
        let mut owner = BoardLifecycle::default();
        owner.fault(Fault::InvalidProof, "bad proof".into(), 1);
        owner.retired(true, None, 2);
        owner.begin_attempt(3);
        owner.initialized();
        assert!(!owner.accepts_work());
        owner.recovered(5);
        assert!(owner.accepts_work());
        assert_eq!(owner.status().serving_since_unix, 5);
        assert_eq!(owner.status().fault, None);
        assert_eq!(
            serde_json::to_value(&owner).unwrap(),
            serde_json::json!({"state":"connected"})
        );
    }
    #[test]
    fn transport_faults_retire_the_running_owner_without_a_restriction() {
        let mut owner = BoardLifecycle::default();
        owner.begin_attempt(1);
        owner.initialized();
        owner.recovered(2);
        owner.fault(Fault::Transport, "lane failed repeatedly".into(), 3);
        assert!(owner.needs_retirement());
        assert!(!owner.status().latched_fault);
        assert_eq!(owner.status().phase, Phase::Reconnecting);
        owner.retired(false, None, 4);
        assert!(!owner.needs_retirement());
        assert!(!owner.status().quiesced);
        owner.begin_attempt(5);
        assert_eq!(owner.status().phase, Phase::Starting);
        assert_eq!(owner.status().fault_reason, None);
    }
    #[test]
    fn evidence_is_fenced_to_the_current_generation_and_slot() {
        let mut owner = BoardLifecycle::default();
        owner.bind(Some("A"));
        owner.begin_attempt(1);
        let first = owner.status().generation;
        assert!(owner.verified(first, 1, 10));
        assert!(owner.slot_verified(1));
        assert_eq!(owner.status().evidence_unix, 10);
        assert!(!owner.verified(first, 3, 11));
        owner.begin_attempt(2);
        assert!(!owner.slot_verified(1));
        assert!(!owner.verified(first, 1, 12));
        assert!(!owner.canary(first, 12));
        let next = owner.status().generation;
        assert!(owner.canary(next, 13));
        assert_eq!(owner.status().evidence_unix, 13);
        owner.bind(Some("B"));
        assert_eq!(owner.status().evidence_unix, 0);
        assert_eq!(
            serde_json::to_value(&owner).unwrap(),
            serde_json::json!({"state":"not_inspected"})
        );
    }
    #[test]
    fn replacement_keeps_its_restriction_but_a_new_serial_does_not() {
        let mut owner = BoardLifecycle::default();
        owner.bind(Some("A"));
        owner.begin_attempt(1);
        owner.fault(Fault::Thermal, "hot".into(), 2);
        owner.retired(true, Some("hot".into()), 3);
        let mut replacement = owner.clone();
        replacement.bind(Some("A"));
        assert!(replacement.status().latched_fault);
        assert!(replacement.status().quiesced);
        replacement.bind(Some("B"));
        assert!(!replacement.status().latched_fault);
        assert!(!replacement.status().quiesced);
    }
}
