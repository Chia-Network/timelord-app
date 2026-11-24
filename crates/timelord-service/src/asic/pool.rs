//! Cancellation-safe physical engine queues. Unselected engines remain parked.
use super::Lease;
use tokio::sync::mpsc;
pub(super) struct Pool {
    receiver: mpsc::Receiver<Lease>,
    parked: Vec<Lease>,
}
impl Pool {
    pub fn new(receiver: mpsc::Receiver<Lease>) -> Self {
        Self {
            receiver,
            parked: Vec::new(),
        }
    }
    pub async fn receive(&mut self, slot: Option<u8>, generation: u64) -> Option<Lease> {
        self.parked.retain(|lease| lease.generation == generation);
        if let Some(index) = self
            .parked
            .iter()
            .position(|lease| slot.is_none_or(|s| s == lease.slot))
        {
            return Some(self.parked.swap_remove(index));
        }
        loop {
            let lease = self.receiver.recv().await?;
            if lease.generation != generation {
                continue;
            }
            if slot.is_none_or(|s| s == lease.slot) {
                return Some(lease);
            }
            self.parked.push(lease);
        }
    }
    pub fn clear(&mut self) {
        self.parked.clear();
        while self.receiver.try_recv().is_ok() {}
    }
}
