//! Bounded, copy-on-write chunks. A reader freezes handles, never a day of records.
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};
use std::{collections::VecDeque, ops::Index, sync::Arc};
const CHUNK: usize = 256;
/// Record cap for every history; checkpoints rely on it to detect truncation.
pub const LIMIT: usize = 86400;

#[derive(Clone)]
pub struct History<T> {
    chunks: VecDeque<Arc<Vec<T>>>,
    len: usize,
}
impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            chunks: VecDeque::new(),
            len: 0,
        }
    }
}
impl<T: Clone> History<T> {
    pub fn push(&mut self, value: T) {
        if self.chunks.back().is_none_or(|c| c.len() == CHUNK) {
            self.chunks.push_back(Arc::new(Vec::with_capacity(CHUNK)));
        }
        Arc::make_mut(self.chunks.back_mut().expect("chunk")).push(value);
        self.len += 1;
        while self.len > LIMIT {
            self.pop_front();
        }
    }
    fn pop_front(&mut self) {
        let front = self.chunks.front_mut().expect("nonempty history");
        if front.len() == 1 {
            self.chunks.pop_front();
        } else {
            Arc::make_mut(front).remove(0);
        }
        self.len -= 1;
    }
    pub fn expire_front(&mut self, mut expired: impl FnMut(&T) -> bool) {
        while self.chunks.front().is_some_and(|c| expired(&c[0])) {
            self.pop_front();
        }
    }
    /// A frozen snapshot shares complete chunks, including a partial last chunk.
    pub fn append(&mut self, other: &Self) {
        if other.is_empty() {
            return;
        }
        self.chunks.extend(other.chunks.iter().cloned());
        self.len += other.len;
    }
    /// Snapshot validation runs outside the runtime lock; unchanged chunks stay shared.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.chunks.retain_mut(|chunk| {
            let mut decisions = [false; CHUNK];
            for (decision, value) in decisions.iter_mut().zip(chunk.iter()) {
                *decision = keep(value);
            }
            if !decisions[..chunk.len()].iter().all(|keep| *keep) {
                let mut decisions = decisions[..chunk.len()].iter();
                Arc::make_mut(chunk)
                    .retain(|_| *decisions.next().expect("one decision per record"));
            }
            !chunk.is_empty()
        });
        self.len = self.chunks.iter().map(|c| c.len()).sum();
        while self.len > LIMIT {
            self.pop_front();
        }
    }
    pub fn retain_mut(&mut self, mut keep: impl FnMut(&mut T) -> bool) {
        self.chunks.retain_mut(|chunk| {
            Arc::make_mut(chunk).retain_mut(&mut keep);
            !chunk.is_empty()
        });
        self.len = self.chunks.iter().map(|c| c.len()).sum();
        while self.len > LIMIT {
            self.pop_front();
        }
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.chunks
            .iter_mut()
            .flat_map(|c| Arc::make_mut(c).iter_mut())
    }
    pub fn last_mut(&mut self) -> Option<&mut T> {
        self.chunks
            .back_mut()
            .and_then(|c| Arc::make_mut(c).last_mut())
    }
}
impl<T> History<T> {
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + Clone {
        self.chunks.iter().flat_map(|c| c.iter())
    }
    pub fn first(&self) -> Option<&T> {
        self.chunks.front().and_then(|c| c.first())
    }
    pub fn last(&self) -> Option<&T> {
        self.chunks.back().and_then(|c| c.last())
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}
impl<T: Clone> From<Vec<T>> for History<T> {
    fn from(values: Vec<T>) -> Self {
        let mut result = Self::default();
        for value in values {
            result.push(value);
        }
        result
    }
}
impl<T> Index<usize> for History<T> {
    type Output = T;
    fn index(&self, mut index: usize) -> &T {
        for chunk in &self.chunks {
            if index < chunk.len() {
                return &chunk[index];
            }
            index -= chunk.len();
        }
        panic!("history index out of bounds")
    }
}
impl<T: Serialize> Serialize for History<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.len))?;
        for value in self.iter() {
            sequence.serialize_element(value)?;
        }
        sequence.end()
    }
}
impl<'de, T: Deserialize<'de> + Clone> Deserialize<'de> for History<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Vec::<T>::deserialize(deserializer)?.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retain_calls_a_stateful_predicate_once_per_record_in_order() {
        let original: Vec<_> = (0..CHUNK + 7).collect();
        let mut history: History<_> = original.clone().into();
        let frozen = history.clone();
        let mut visited = Vec::new();
        history.retain(|value| {
            visited.push(*value);
            visited.len() % 3 != 0
        });
        assert_eq!(visited, original);
        let expected: Vec<_> = original
            .iter()
            .enumerate()
            .filter_map(|(index, value)| ((index + 1) % 3 != 0).then_some(*value))
            .collect();
        assert_eq!(history.iter().copied().collect::<Vec<_>>(), expected);
        assert_eq!(frozen.iter().copied().collect::<Vec<_>>(), original);
    }
    #[test]
    fn out_of_order_timestamps_are_filtered_by_snapshot_validation() {
        let mut live: History<_> = vec![100, 10, 101].into();
        live.expire_front(|timestamp| *timestamp < 50);
        // A clock reversal may hide an old record behind a fresh prefix. The
        // live queue stays count-bounded; a frozen snapshot validates all records.
        let mut snapshot = live.clone();
        snapshot.retain(|timestamp| *timestamp >= 50 && *timestamp <= 101);
        assert_eq!(snapshot.iter().copied().collect::<Vec<_>>(), vec![100, 101]);
        assert_eq!(live.len(), 3);
    }
    #[test]
    fn frozen_chunks_survive_append_expiration_and_mutation() {
        let mut history: History<_> = (0..LIMIT).collect::<Vec<_>>().into();
        let frozen = history.clone();
        assert!(Arc::ptr_eq(&history.chunks[1], &frozen.chunks[1]));
        history.push(LIMIT);
        assert_eq!(history.len(), LIMIT);
        assert_eq!(history[0], 1);
        assert_eq!(frozen[0], 0);
        history.expire_front(|v| *v < 1000);
        assert_eq!(history[0], 1000);
        *history.last_mut().unwrap() = 999999;
        assert_eq!(frozen.last(), Some(&(LIMIT - 1)));
        let bytes = serde_json::to_vec(&history).unwrap();
        let restored: History<usize> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.len(), history.len());
        assert_eq!(restored.last(), Some(&999999));
    }
}
