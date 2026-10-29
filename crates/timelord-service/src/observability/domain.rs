//! Typed events shared by producers; exporter label strings stop here.
use timelord_core::{Chain, Config};

pub fn proof_lane(c: Chain) -> timelord_supervisor::progress::ProofLane {
    use timelord_supervisor::progress::ProofLane;
    match c {
        Chain::Challenge => ProofLane::Challenge,
        Chain::Reward => ProofLane::Reward,
        Chain::InfusedChallenge => ProofLane::InfusedChallenge,
    }
}
pub fn chain(c: Chain) -> &'static str {
    match c {
        Chain::Challenge => "challenge",
        Chain::Reward => "reward",
        Chain::InfusedChallenge => "infused_challenge",
    }
}
pub(super) fn index(c: Chain) -> usize {
    match c {
        Chain::Challenge => 0,
        Chain::Reward => 1,
        Chain::InfusedChallenge => 2,
    }
}
pub fn board(config: &Config, id: &str) -> String {
    config
        .hardware
        .boards
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.serial.as_deref().unwrap_or(&b.id).to_owned())
        .unwrap_or_else(|| id.to_owned())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofStage {
    Discriminant,
    ClientWait,
    RequestToVerified,
    Verification,
    BundleWait,
}
impl ProofStage {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Discriminant => "discriminant",
            Self::ClientWait => "client_wait",
            Self::RequestToVerified => "request_to_verified",
            Self::Verification => "verification",
            Self::BundleWait => "bundle_wait",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionKind {
    Signage,
    Infusion,
    EndOfSubSlot,
}
impl SubmissionKind {
    pub(super) const ALL: [Self; 3] = [Self::Signage, Self::Infusion, Self::EndOfSubSlot];
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Signage => "signage",
            Self::Infusion => "infusion",
            Self::EndOfSubSlot => "end_of_sub_slot",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartReason {
    Thermal,
    Readback,
    Sensor,
    Verification,
    Usb,
    Process,
    Topology,
}
impl RestartReason {
    pub(super) const ALL: [Self; 7] = [
        Self::Thermal,
        Self::Readback,
        Self::Sensor,
        Self::Verification,
        Self::Usb,
        Self::Process,
        Self::Topology,
    ];
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Thermal => "thermal",
            Self::Readback => "readback",
            Self::Sensor => "sensor",
            Self::Verification => "verification",
            Self::Usb => "usb",
            Self::Process => "process",
            Self::Topology => "topology",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeakResult {
    Accepted,
    Ignored,
    Rejected,
}
impl PeakResult {
    pub(super) const ALL: [Self; 3] = [Self::Accepted, Self::Ignored, Self::Rejected];
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Ignored => "ignored",
            Self::Rejected => "rejected",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionReason {
    Initial,
    InputChange,
    Reconnect,
}
impl SessionReason {
    pub(super) const ALL: [Self; 3] = [Self::Initial, Self::InputChange, Self::Reconnect];
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::InputChange => "input_change",
            Self::Reconnect => "reconnect",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatingPointReason {
    Trial,
    Backoff,
    Restore,
}
impl OperatingPointReason {
    pub(super) const ALL: [Self; 3] = [Self::Trial, Self::Backoff, Self::Restore];
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Trial => "trial",
            Self::Backoff => "backoff",
            Self::Restore => "restore",
        }
    }
}
pub fn restart_reason(message: &str) -> RestartReason {
    let message = message.to_ascii_lowercase();
    if message.contains("temperature_limit")
        || message.contains("temperature limit")
        || message.contains("temperature_guard")
    {
        RestartReason::Thermal
    } else if message.contains("readback") {
        RestartReason::Readback
    } else if message.contains("temperature") {
        RestartReason::Sensor
    } else if message.contains("verif") {
        RestartReason::Verification
    } else if ["usb", "ftdi", "device", "disconnected"]
        .iter()
        .any(|s| message.contains(s))
    {
        RestartReason::Usb
    } else {
        RestartReason::Process
    }
}
