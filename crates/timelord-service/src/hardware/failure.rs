//! Native protocol faults carry policy independently of their human-readable text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeFailureKind {
    Thermal,
    SensorUnavailable,
    ControlClosed,
    Readback,
    Protocol,
}
#[derive(Debug)]
pub struct NativeFailure {
    pub kind: NativeFailureKind,
    pub message: String,
}
impl NativeFailure {
    pub fn new(kind: NativeFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn rejection(sequence: u64, reason: &str) -> Self {
        let kind = match reason.split_whitespace().next().unwrap_or("") {
            "temperature_limit" | "temperature_guard" => NativeFailureKind::Thermal,
            "temperature_unavailable" => NativeFailureKind::SensorUnavailable,
            "readback" | "initial_readback" => NativeFailureKind::Readback,
            _ => NativeFailureKind::Protocol,
        };
        Self::new(
            kind,
            format!("Native control rejected request {sequence}: {reason}"),
        )
    }
}
impl std::fmt::Display for NativeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for NativeFailure {}

/// The one mapping from a typed failure to its board restriction. Untyped
/// errors are transport failures and never latch a restriction.
pub(crate) fn classify(error: &anyhow::Error) -> crate::board_lifecycle::Fault {
    use crate::board_lifecycle::Fault;
    if error
        .downcast_ref::<NativeFailure>()
        .is_some_and(|failure| failure.kind == NativeFailureKind::Thermal)
    {
        Fault::Thermal
    } else if error
        .downcast_ref::<crate::asic::AsicFailure>()
        .is_some_and(|failure| failure.verification)
    {
        Fault::InvalidProof
    } else {
        Fault::Transport
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_uses_protocol_codes_and_survives_display_context() {
        for (reason, kind) in [
            (
                "temperature_guard temperature=58",
                NativeFailureKind::Thermal,
            ),
            ("initial_readback attempts=3", NativeFailureKind::Readback),
            (
                "unrelated detail=temperature_guard",
                NativeFailureKind::Protocol,
            ),
        ] {
            let error = anyhow::Error::new(NativeFailure::rejection(3, reason))
                .context("Board A needs attention");
            assert_eq!(error.downcast_ref::<NativeFailure>().unwrap().kind, kind);
        }
    }
    #[test]
    fn one_classifier_maps_typed_failures_to_restrictions() {
        use crate::{board_lifecycle::Fault, safety::Reason};
        let invalid = anyhow::Error::new(crate::asic::AsicFailure {
            board_id: "A".into(),
            verification: true,
            message: "invalid proof".into(),
        })
        .context("Board A");
        let thermal = anyhow::Error::new(NativeFailure::rejection(1, "temperature_limit"));
        for (error, fault, reason) in [
            (invalid, Fault::InvalidProof, Reason::InvalidProof),
            (thermal, Fault::Thermal, Reason::Thermal),
            (
                anyhow::anyhow!("ASIC admission rejected: temperature_limit"),
                Fault::Transport,
                Reason::UncleanOwner,
            ),
        ] {
            assert_eq!(classify(&error), fault);
            assert_eq!(Reason::from(classify(&error)), reason);
        }
    }
    #[test]
    fn board_fault_records_typed_kind_for_status() {
        let mut hardware = crate::runtime::HardwareState::default();
        let error = anyhow::Error::new(NativeFailure::new(
            NativeFailureKind::ControlClosed,
            "Native control closed before acknowledgement",
        ))
        .context("Board A");
        hardware.fault(&error);
        assert_eq!(hardware.fault_kind, Some(NativeFailureKind::ControlClosed));
        assert_eq!(
            serde_json::to_value(hardware.fault_kind).unwrap(),
            "control_closed"
        );
        hardware.fault(&anyhow::anyhow!(
            "Native control closed before acknowledgement"
        ));
        assert_eq!(hardware.fault_kind, None);
    }
}
