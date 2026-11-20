use crate::blockchain::coin::Coin;
use crate::blockchain::condition_opcode::ConditionOpcode;
use crate::blockchain::condition_with_args::ConditionWithArgs;
use crate::blockchain::sized_bytes::Bytes32;
use crate::clvm::program::Program;
use crate::errors::ClvmError;
use crate::traits::SizedBytes;
use crate::utils::hash_256;

pub fn created_outputs_for_conditions(
    conditions: &[ConditionWithArgs],
    input_coin_name: Bytes32,
) -> Vec<Coin> {
    let mut output_coins = Vec::new();
    for condition in conditions {
        let ConditionWithArgs::CreateCoin(puzzle_hash, amount, _) = condition else {
            continue;
        };
        output_coins.push(Coin {
            parent_coin_info: input_coin_name,
            puzzle_hash: *puzzle_hash,
            amount: *amount,
        });
    }
    output_coins
}

pub fn conditions_for_solution(
    puzzle_reveal: &Program,
    solution: &Program,
    max_cost: u64,
) -> Result<(Vec<ConditionWithArgs>, u64), ClvmError> {
    let (cost, r) = puzzle_reveal.run_with_cost(max_cost, solution)?;
    let conditions = r.sexp().try_into()?;
    Ok((conditions, cost))
}

pub fn agg_sig_additional_data_for_opcode(
    agg_sig_data: Bytes32,
    opcode: ConditionOpcode,
) -> Bytes32 {
    match opcode {
        ConditionOpcode::AggSigParent
        | ConditionOpcode::AggSigPuzzle
        | ConditionOpcode::AggSigAmount
        | ConditionOpcode::AggSigPuzzleAmount
        | ConditionOpcode::AggSigParentAmount
        | ConditionOpcode::AggSigParentPuzzle => {
            let mut buffer = agg_sig_data.bytes().to_vec();
            buffer.push(opcode as u8);
            Bytes32::new(hash_256(&buffer))
        }
        _ => agg_sig_data,
    }
}
