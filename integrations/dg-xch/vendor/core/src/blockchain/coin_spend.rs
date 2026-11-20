use crate::blockchain::coin::Coin;
use crate::blockchain::condition_opcode::{ConditionCost, ConditionOpcode};
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::utils::{additions_for_solution, fee_for_solution};
use crate::clvm::program::SerializedProgram;
use crate::clvm::utils::INFINITE_COST;
use crate::errors::ClvmError;
use crate::traits::SizedBytes;
use dg_xch_macros::ChiaSerial;
use num_bigint::BigInt;
use serde::{Deserialize, Serialize};

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct CoinSpend {
    pub coin: Coin,
    pub puzzle_reveal: SerializedProgram,
    pub solution: SerializedProgram,
}

impl CoinSpend {
    pub fn additions(&self) -> Result<Vec<Coin>, ClvmError> {
        let reveal = self.puzzle_reveal.to_program()?;
        let solution = self.solution.to_program()?;
        additions_for_solution(self.coin.name(), &reveal, &solution, INFINITE_COST)
    }

    pub fn reserved_fee(self) -> Result<BigInt, ClvmError> {
        let reveal = self.puzzle_reveal.to_program()?;
        let solution = self.solution.to_program()?;
        fee_for_solution(&reveal, &solution, INFINITE_COST)
    }
    pub fn compute_additions_with_cost(
        &self,
        max_cost: u64,
    ) -> Result<(Vec<Coin>, u64), ClvmError> {
        let parent_coin_info = self.coin.name();
        let mut ret: Vec<Coin> = vec![];
        let reveal = self.puzzle_reveal.to_program()?;
        let solution = self.solution.to_program()?;
        let (mut cost, r) = reveal.run_with_cost(max_cost, &solution)?;
        for cond in r.sexp().ref_list() {
            if cost > max_cost {
                Err(ClvmError::CostExceeded(max_cost, cost))?;
            }
            let atoms = cond.ref_list();
            if atoms.is_empty() {
                Err(ClvmError::UnexpectedEndOfValues(
                    "Atoms List is Empty".to_string(),
                ))?;
            }
            let op = atoms[0];
            if [ConditionOpcode::AggSigMe, ConditionOpcode::AggSigUnsafe].contains(&op.into()) {
                cost += ConditionCost::AggSig as u64;
                continue;
            }
            if ConditionOpcode::from(op) != ConditionOpcode::CreateCoin {
                continue;
            }
            cost += ConditionCost::CreateCoin as u64;
            if atoms.len() < 3 {
                return Err(ClvmError::InvalidArgCount(
                    "Invalid Number ot Atoms in Program".to_string(),
                ));
            }
            let puzzle_hash = Bytes32::parse(&atoms[1].as_vec().unwrap_or_default())?;
            let amount = atoms[2].as_int()?;
            ret.push(Coin {
                parent_coin_info,
                puzzle_hash,
                amount: amount
                    .to_u64()
                    .ok_or(ClvmError::AtomNotValidU64(amount.to_string()))?,
            });
        }
        Ok((ret, cost))
    }
}
