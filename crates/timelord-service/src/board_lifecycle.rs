//! Serial-scoped board state. Runtime, inventory and proof lanes report events;
//! only this model decides legal status and usable capacity. Telemetry is separate.
use serde::{Serialize, Serializer, ser::SerializeStruct};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Starting,
    Checking,
    Admitted,
    Connected,
    Stopped,
    Fault,
    ThermalStop,
    Quarantined,
    Reconnecting,
}
impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "not_inspected",
            Self::Starting => "starting",
            Self::Checking => "checking",
            Self::Admitted => "admitted",
            Self::Connected => "connected",
            Self::Stopped => "stopped",
            Self::Fault => "fault",
            Self::ThermalStop => "thermal_stop",
            Self::Quarantined => "quarantined",
            Self::Reconnecting => "reconnecting",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Transport,
    Thermal,
    InvalidProof,
    UncleanOwner,
}
impl From<crate::safety::Reason> for Fault {
    fn from(reason: crate::safety::Reason) -> Self {
        match reason {
            crate::safety::Reason::Thermal => Self::Thermal,
            crate::safety::Reason::InvalidProof => Self::InvalidProof,
            crate::safety::Reason::UncleanOwner => Self::UncleanOwner,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inventory {
    Missing,
    Present,
    Ambiguous,
}
/// Immutable view. Consumers cannot mutate lifecycle by changing display state.
#[derive(Clone, Copy, Debug)]
pub struct Status<'a> {
    pub phase: Phase,
    pub generation: u64,
    pub started_unix: u64,
    pub native_initialized: bool,
    pub quiesced: bool,
    pub latched_fault: bool,
    pub fault: Option<Fault>,
    pub fault_reason: Option<&'a str>,
    pub usb_present: Option<bool>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub usable: bool,
    pub accepts_work: bool,
    pub exploration_ready: bool,
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
    restriction: Option<Fault>,
    fault_reason: Option<String>,
    unavailable_since: Option<u64>,
    inventory: Option<Inventory>,
    admission_inventory: Option<u64>,
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
    pub fn fixture(
        phase: Phase,
        ready: bool,
        fault: Option<Fault>,
        inventory: Option<bool>,
        generation: u64,
    ) -> Self {
        Self {
            phase,
            generation,
            restriction: fault,
            native: if ready { Native::Ready } else { Native::Absent },
            inventory: inventory.map(|p| {
                if p {
                    Inventory::Present
                } else {
                    Inventory::Missing
                }
            }),
            ..Self::default()
        }
    }
    /// Call before transferring a drained run's board into a new configuration.
    /// A different physical serial cannot inherit the old board's admission.
    pub fn bind(&mut self, serial: Option<&str>) {
        if self.serial.as_deref() != serial {
            *self = Self {
                serial: serial.map(str::to_owned),
                ..Self::default()
            };
        }
    }
    pub fn status(&self) -> Status<'_> {
        Status {
            phase: self.phase,
            generation: self.generation,
            started_unix: self.started_unix,
            native_initialized: self.native == Native::Ready,
            quiesced: self.native == Native::Retired { confirmed: true },
            latched_fault: self.restriction.is_some(),
            fault: self.restriction,
            fault_reason: self.fault_reason.as_deref(),
            usb_present: match self.inventory {
                Some(Inventory::Present) => Some(true),
                Some(Inventory::Missing) => Some(false),
                _ => None,
            },
        }
    }
    pub fn capacity(&self, now: u64, managed: bool) -> Capacity {
        let usable = self.restriction.is_none()
            && (self.admission_inventory.is_none() || self.inventory == Some(Inventory::Present))
            && self
                .unavailable_since
                .is_none_or(|since| now.saturating_sub(since) < 60);
        let accepts_work = self.restriction.is_none()
            && (!managed || self.native == Native::Ready)
            && !matches!(
                self.phase,
                Phase::Fault
                    | Phase::ThermalStop
                    | Phase::Reconnecting
                    | Phase::Quarantined
                    | Phase::Checking
                    | Phase::Admitted
            );
        Capacity {
            usable,
            accepts_work,
            exploration_ready: self.native == Native::Ready
                && self.phase == Phase::Connected
                && self.restriction.is_none(),
        }
    }
    pub fn begin_attempt(&mut self, now: u64) {
        self.generation = self.generation.saturating_add(1);
        self.verified_generation = None;
        self.verified_slots.clear();
        self.started_unix = now;
        self.start_native();
    }
    /// Proof evidence belongs to the native session that produced it. A late
    /// verification cannot certify a replacement session or another physical slot.
    pub fn verified(&mut self, generation: u64, slot: u8) -> bool {
        if generation != self.generation || slot >= 3 {
            return false;
        }
        self.verified_generation = Some(generation);
        self.verified_slots.insert(slot);
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
    pub fn start_native(&mut self) {
        self.native = Native::Initializing;
        self.phase = match self.restriction {
            Some(Fault::Thermal) => Phase::ThermalStop,
            Some(_) => Phase::Fault,
            None => {
                self.fault_reason = None;
                Phase::Starting
            }
        };
    }
    pub fn checking(&mut self) {
        self.phase = Phase::Checking;
    }
    pub fn initialized(&mut self) {
        self.native = Native::Ready;
    }
    pub fn cool_telemetry(&mut self) {
        if self.restriction.is_none() && matches!(self.phase, Phase::Starting | Phase::Connected) {
            self.phase = Phase::Connected;
        }
        if self.restriction.is_none() && self.native == Native::Ready {
            self.unavailable_since = None;
        }
    }
    pub fn fault(&mut self, fault: Fault, reason: String, now: u64) {
        if fault != Fault::Transport {
            self.restriction = match (self.restriction, fault) {
                (Some(Fault::InvalidProof), _) | (_, Fault::InvalidProof) => {
                    Some(Fault::InvalidProof)
                }
                (Some(Fault::Thermal), _) | (_, Fault::Thermal) => Some(Fault::Thermal),
                _ => Some(fault),
            };
        }
        self.unavailable_since.get_or_insert(now);
        self.phase = if self.restriction == Some(Fault::Thermal) {
            Phase::ThermalStop
        } else {
            Phase::Fault
        };
        self.fault_reason = Some(reason);
    }
    pub fn retry(&mut self) {
        self.phase = match self.restriction {
            Some(Fault::Thermal) => Phase::ThermalStop,
            Some(_) => Phase::Fault,
            None => Phase::Reconnecting,
        };
    }
    pub fn quarantined(&mut self, fault: Fault, reason: String, now: u64) {
        self.fault(fault, reason, now);
        self.phase = Phase::Quarantined;
    }
    pub fn admitted(&mut self, epoch: u64) {
        self.phase = Phase::Admitted;
        self.native = Native::Retired { confirmed: true };
        self.restriction = None;
        self.fault_reason = None;
        self.unavailable_since = None;
        self.admission_inventory = Some(epoch);
        self.inventory = None;
    }
    /// Keep refreshing an admitted, closed board throughout layout debounce.
    /// Native startup invalidates the ticket so open-handle omissions are ignored.
    pub fn admitted_inventory_epoch(&self) -> Option<u64> {
        // Confirmed production retirement cannot renew an admission ticket.
        if self.phase == Phase::Admitted && self.native == (Native::Retired { confirmed: true }) {
            self.admission_inventory
        } else {
            None
        }
    }
    /// epoch=None is a closed-owner discovery made by the current supervisor.
    /// A periodic probe must carry the admission epoch it observed before I/O.
    pub fn inventory(&mut self, epoch: Option<u64>, observed: Inventory) {
        if epoch.is_some() && epoch != self.admitted_inventory_epoch() {
            return;
        }
        self.inventory = Some(observed);
    }
    pub fn retired(&mut self, confirmed: bool, error: Option<String>, now: u64) {
        self.native = Native::Retired { confirmed };
        if let Some(error) = error {
            self.fault(Fault::Transport, error, now);
        } else if !matches!(self.phase, Phase::Fault | Phase::ThermalStop) {
            self.phase = Phase::Stopped;
        }
    }
    pub fn preserve_in_inventory(&self, telemetry_fresh: bool) -> bool {
        let status = self.status();
        (!status.latched_fault || status.phase == Phase::Checking)
            && (status.usb_present == Some(true)
                || (status.usb_present.is_none()
                    && status.phase == Phase::Connected
                    && telemetry_fresh))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_start_and_cool_telemetry_cannot_clear_latched_restrictions() {
        for (fault, phase) in [
            (Fault::Thermal, Phase::ThermalStop),
            (Fault::InvalidProof, Phase::Fault),
        ] {
            let mut owner = BoardLifecycle::default();
            owner.fault(fault, "Original safety restriction".into(), 1);
            owner.retired(true, None, 2);
            owner.begin_attempt(3);
            owner.initialized();
            owner.cool_telemetry();
            assert_eq!(owner.status().phase, phase);
            assert_eq!(owner.status().fault, Some(fault));
            assert_eq!(
                owner.status().fault_reason,
                Some("Original safety restriction")
            );
            assert!(!owner.capacity(100, true).usable);
            assert!(!owner.capacity(100, true).accepts_work);
            assert!(!owner.capacity(100, true).exploration_ready);
            // Only a successful admission can release the persisted restriction.
            owner.admitted(4);
            owner.inventory(Some(4), Inventory::Present);
            owner.begin_attempt(5);
            owner.initialized();
            owner.cool_telemetry();
            assert_eq!(owner.status().phase, Phase::Connected);
            assert_eq!(owner.status().fault, None);
            assert_eq!(owner.status().fault_reason, None);
            assert!(owner.capacity(100, true).exploration_ready);
        }
    }
    #[test]
    fn production_retirement_cannot_revive_an_old_admission_inventory_ticket() {
        for error in [None, Some("transport failed".to_owned())] {
            let mut owner = BoardLifecycle::default();
            owner.admitted(7);
            owner.inventory(Some(7), Inventory::Present);
            owner.begin_attempt(1);
            owner.initialized();
            owner.cool_telemetry();
            owner.retired(true, error.clone(), 10);
            assert!(owner.status().quiesced);
            assert_eq!(owner.admitted_inventory_epoch(), None);
            // A delayed admission probe, or a newly scheduled periodic probe,
            // cannot reinterpret a production retirement as admission waiting.
            owner.inventory(Some(7), Inventory::Missing);
            assert_eq!(owner.status().usb_present, Some(true));
            assert!(owner.capacity(69, true).usable);
            assert_eq!(owner.capacity(70, true).usable, error.is_none());

            owner.admitted(8);
            assert_eq!(owner.admitted_inventory_epoch(), Some(8));
            owner.inventory(Some(7), Inventory::Present);
            assert!(!owner.capacity(71, true).usable);
            owner.inventory(Some(8), Inventory::Present);
            assert!(owner.capacity(71, true).usable);
        }
    }
    #[test]
    fn native_attempt_does_not_release_unconfirmed_admitted_capacity() {
        for observation in [None, Some(Inventory::Missing), Some(Inventory::Ambiguous)] {
            let mut owner = BoardLifecycle::default();
            owner.admitted(7);
            if let Some(observation) = observation {
                owner.inventory(Some(7), observation);
            }
            assert!(!owner.capacity(1, true).usable);
            owner.start_native();
            assert!(!owner.capacity(2, true).usable);
            assert_eq!(owner.admitted_inventory_epoch(), None);
            owner.inventory(Some(7), Inventory::Present);
            assert!(
                !owner.capacity(3, true).usable,
                "an old background probe released startup capacity"
            );
            owner.begin_attempt(4);
            assert!(!owner.capacity(4, true).usable);
            // The native owner has not opened yet; its fresh, explicit locate
            // probe can confirm presence without reviving an old background ticket.
            owner.inventory(None, Inventory::Present);
            assert!(owner.capacity(5, true).usable);
            assert!(!owner.capacity(5, true).accepts_work);
        }
    }
    #[test]
    fn admission_and_inventory_are_one_transition_contract() {
        let mut owner = BoardLifecycle::default();
        owner.bind(Some("A"));
        owner.begin_attempt(10);
        owner.fault(Fault::InvalidProof, "bad proof".into(), 12);
        owner.admitted(7);
        assert!(!owner.capacity(100, true).usable);
        owner.inventory(Some(6), Inventory::Present);
        assert!(!owner.capacity(100, true).usable);
        owner.inventory(Some(7), Inventory::Ambiguous);
        assert!(!owner.capacity(100, true).usable);
        owner.inventory(Some(7), Inventory::Present);
        assert!(owner.capacity(100, true).usable);
        assert!(!owner.capacity(100, true).accepts_work);
        owner.start_native();
        owner.initialized();
        owner.cool_telemetry();
        assert!(owner.capacity(100, true).exploration_ready);
        assert_eq!(
            serde_json::to_value(&owner).unwrap(),
            serde_json::json!({"state":"connected"})
        );
    }
    #[test]
    fn replacement_preserves_fault_but_new_serial_does_not_inherit_it() {
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
    #[test]
    fn proof_evidence_is_fenced_to_the_current_native_generation() {
        let mut owner = BoardLifecycle::default();
        owner.bind(Some("A"));
        owner.begin_attempt(1);
        let first = owner.status().generation;
        assert!(owner.verified(first, 1));
        assert_eq!(owner.verified_generation(), Some(first));
        assert!(owner.slot_verified(1));
        assert!(!owner.verified(first, 3));
        owner.begin_attempt(2);
        assert_eq!(owner.verified_generation(), None);
        assert!(!owner.slot_verified(1));
        assert!(!owner.verified(first, 1));
        assert_eq!(owner.verified_generation(), None);
        let next = owner.status().generation;
        assert!(owner.verified(next, 2));
        owner.forget_slot(1);
        assert!(owner.slot_verified(2));
        owner.forget_slot(2);
        assert_eq!(owner.verified_generation(), None);
    }
    #[test]
    fn physical_identity_rebinding_cannot_inherit_verified_slots() {
        let mut owner = BoardLifecycle::default();
        owner.bind(Some("A"));
        owner.begin_attempt(1);
        assert!(owner.verified(owner.status().generation, 0));
        owner.bind(Some("A"));
        assert!(owner.slot_verified(0));
        owner.bind(Some("B"));
        assert_eq!(owner.verified_generation(), None);
        assert!(!owner.slot_verified(0));
        assert_eq!(
            serde_json::to_value(&owner).unwrap(),
            serde_json::json!({"state":"not_inspected"})
        );
    }
}
