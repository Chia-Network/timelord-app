use super::*;

/// Result of offering an unfinished block to the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The candidate was validated and scheduled for infusion.
    Admitted(Bytes32),
    /// The candidate needs transaction context that the next peak will provide.
    Deferred,
    /// The candidate needed deferral but the bounded queue is full.
    DeferredQueueFull,
    /// The scheduler already tracks this candidate or has infused it.
    Duplicate,
    /// The candidate belongs to an older chain or slot.
    Stale,
    /// No peak is known yet, or chain parameters do not match this peak.
    Ignored,
}

impl Scheduler {
    pub fn add_unfinished(&mut self, message: NewUnfinishedBlockTimelord) -> Result<bool, Error> {
        self.admit_unfinished(message)
            .map(|admission| matches!(admission, Admission::Admitted(_)))
    }

    /// Admission owns hashing and candidate-local errors; observers reuse its identity.
    pub fn admit_unfinished(
        &mut self,
        message: NewUnfinishedBlockTimelord,
    ) -> Result<Admission, Error> {
        let hash = message.reward_chain_block.hash()?;
        self.add_unfinished_hashed(message, hash)
    }

    /// Retry outcomes from the most recent peak only; they are cleared on every new peak.
    pub fn take_retry_outcomes(&mut self) -> Vec<(Bytes32, Result<Admission, String>)> {
        std::mem::take(&mut self.retry_outcomes)
    }

    fn transaction_lower(&self) -> u32 {
        self.last_transaction.map_or(0, |(height, _)| height)
    }

    pub(super) fn update_transaction_context(
        &mut self,
        is_transaction_block: bool,
        height: u32,
        hash: Bytes32,
        rewards: &VecDeque<(Bytes32, u128)>,
        contiguous: bool,
    ) {
        if is_transaction_block {
            self.transaction_height = Some(height);
            self.last_transaction = Some((height, hash));
            return;
        }
        if !contiguous {
            self.transaction_height = None;
        }
        let ancestor = self
            .last_transaction
            .is_some_and(|(_, last)| rewards.iter().any(|(reward, _)| *reward == last));
        if !contiguous && !ancestor {
            self.last_transaction = None;
        }
    }

    fn add_unfinished_hashed(
        &mut self,
        message: NewUnfinishedBlockTimelord,
        hash: Bytes32,
    ) -> Result<Admission, Error> {
        let Some(state) = &self.state else {
            return Ok(Admission::Ignored);
        };
        let block = &message.reward_chain_block;
        if self.candidates.contains_key(&hash.const_bytes()) || self.infused.contains(&hash) {
            return Ok(Admission::Duplicate);
        }
        if block.total_iters <= state.total_iters {
            return Ok(Admission::Stale);
        }
        if self.candidates.len() >= self.reward_limit()
            || message.difficulty != state.difficulty
            || message.sub_slot_iters != state.sub_slot_iters
        {
            return Ok(Admission::Ignored);
        }
        let height = match state.height {
            Some(height) => height
                .checked_add(1)
                .ok_or_else(|| invalid("chain height overflow"))?,
            None => 0,
        };
        let signage_hash = match block.challenge_chain_sp_vdf {
            Some(info) if block.signage_point_index != 0 => info.output.hash()?,
            None if block.signage_point_index == 0 => block.pos_ss_cc_challenge_hash,
            _ => return Err(invalid("invalid unfinished signage-point fields")),
        };
        let quality = if block.proof_of_space.version == 1 {
            block.proof_of_space.chia_timelord_quality(
                &self.constants,
                block.pos_ss_cc_challenge_hash,
                signage_hash,
            )
        } else {
            dg_xch_pos::verify_and_get_quality_string_without_activation(
                &block.proof_of_space,
                &self.constants,
                block.pos_ss_cc_challenge_hash,
                signage_hash,
                height,
            )
        }
        .ok_or_else(|| invalid("unfinished block proof of space is invalid"))?;
        if !verify_plot_signature(
            &block.proof_of_space.plot_public_key,
            signage_hash,
            &block.challenge_chain_sp_signature,
        ) {
            return Err(invalid("unfinished block signage signature is invalid"));
        }
        let Some(required) =
            dg_xch_core::consensus::pot_iterations::timelord_iterations_with_context(
                &self.constants,
                &block.proof_of_space,
                quality,
                message.difficulty,
                signage_hash,
                dg_xch_core::consensus::pot_iterations::TxContext {
                    known: self.transaction_height,
                    lower: self.transaction_lower(),
                    upper: height.saturating_sub(1),
                },
            )?
        else {
            if self.deferred.len() < self.reward_limit() {
                self.deferred.insert(hash.const_bytes(), message);
                return Ok(Admission::Deferred);
            }
            return Ok(Admission::DeferredQueueFull);
        };
        self.deferred.remove(&hash.const_bytes());
        let offset = calculate_ip_iters(
            &self.constants,
            message.sub_slot_iters,
            block.signage_point_index,
            required,
        )?;
        let signage_offset = calculate_sp_iters(
            &self.constants,
            message.sub_slot_iters,
            block.signage_point_index,
        )?;
        if block.challenge_chain_sp_vdf.is_some_and(|info| {
            info.challenge != block.pos_ss_cc_challenge_hash
                || info.number_of_iterations != signage_offset
        }) {
            return Err(invalid(
                "unfinished challenge signage VDF metadata mismatch",
            ));
        }
        let reward_signage_hash = match block.reward_chain_sp_vdf {
            Some(info) if block.signage_point_index != 0 && info.challenge == message.rc_prev => {
                info.output.hash()?
            }
            None if block.signage_point_index == 0 => message.rc_prev,
            _ => return Err(invalid("invalid unfinished reward signage-point fields")),
        };
        if !verify_plot_signature(
            &block.proof_of_space.plot_public_key,
            reward_signage_hash,
            &block.reward_chain_sp_signature,
        ) {
            return Err(invalid("unfinished reward signage signature is invalid"));
        }
        let overflow = is_overflow_block(&self.constants, block.signage_point_index)?;
        let slot_start = block
            .total_iters
            .checked_sub(u128::from(offset))
            .ok_or_else(|| invalid("unfinished block iteration underflow"))?;
        let next_slot = state.end()?;
        if block.total_iters <= state.total_iters
            || (slot_start != state.slot_start && !(overflow && slot_start == next_slot))
            || (overflow && state.new_epoch && slot_start == state.slot_start)
        {
            return Ok(Admission::Stale);
        }
        if (!overflow || slot_start == next_slot)
            && block.pos_ss_cc_challenge_hash != state.challenge.challenge
        {
            return Ok(Admission::Stale);
        }
        let signage_total_iters = slot_start
            .checked_add(u128::from(signage_offset))
            .and_then(|total| {
                if overflow {
                    total.checked_sub(u128::from(message.sub_slot_iters))
                } else {
                    Some(total)
                }
            })
            .ok_or_else(|| invalid("unfinished signage iteration overflow"))?;
        if !self.reward_matches(message.rc_prev, signage_total_iters) {
            return Ok(Admission::Stale);
        }
        self.candidates.insert(
            hash.const_bytes(),
            Candidate {
                hash,
                total_iters: block.total_iters,
                overflow,
                slot_start,
                rc_prev: message.rc_prev,
                signage_total_iters,
            },
        );
        Ok(Admission::Admitted(hash))
    }
}
