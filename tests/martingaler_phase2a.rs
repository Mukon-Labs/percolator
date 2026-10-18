use percolator::martingaler::{
    oracle_cash_pnl, ClaimSequence, CloseKind, CloseOutcome, FundingKind, HaircutConfig,
    MarketConfig, MarketMode, Martingaler, MartingalerConfig, ModelError, OracleCashPnl,
    PositionSide, SignedMagnitude, V1_MAX_NET_PROFIT_PER_POSITION_QUOTE,
    V1_MAX_POSITION_NOTIONAL_QUOTE, V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE,
    V1_MIN_COLLATERAL_QUOTE, V1_QUOTE_ATOMS_PER_UNIT, V1_QUOTE_DECIMALS,
    V1_TOTAL_POTENTIAL_LIABILITY_QUOTE,
};
use proptest::prelude::*;

const BTC: u16 = 1;
const ETH: u16 = 2;

fn haircut() -> HaircutConfig {
    HaircutConfig {
        base_rate_ppm: 100_000,
        deadband_move_ppm: 20,
        linear_coefficient_ppm: 2_000_000,
        quadratic_coefficient_ppm: 4_000_000,
        max_rate_ppm: 100_000,
        win_fee_ppm: 20_000,
    }
}

fn config(max_records: usize) -> MartingalerConfig {
    MartingalerConfig {
        risk_pool_id: 77,
        max_records,
        max_harvest_entries: 2,
        claim_dust_threshold: 10,
        residual_bad_debt_cap: 1_000_000,
        queue_debt_open_gate: 1_000_000,
        max_reserved_profit_per_controller: V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE,
        total_liability_open_gate: V1_TOTAL_POTENTIAL_LIABILITY_QUOTE,
        reward_basis_cap: 1_000_000,
        fee_accrual_cap: 1_000_000,
        markets: vec![
            MarketConfig {
                market_id: BTC,
                max_leverage_x: 100,
                max_position_notional: V1_MAX_POSITION_NOTIONAL_QUOTE,
                max_net_profit_per_position: V1_MAX_NET_PROFIT_PER_POSITION_QUOTE,
                max_long_open_interest: V1_MAX_POSITION_NOTIONAL_QUOTE,
                max_short_open_interest: V1_MAX_POSITION_NOTIONAL_QUOTE,
                haircut: haircut(),
                initial_mode: MarketMode::Active,
            },
            MarketConfig {
                market_id: ETH,
                max_leverage_x: 100,
                max_position_notional: V1_MAX_POSITION_NOTIONAL_QUOTE,
                max_net_profit_per_position: V1_MAX_NET_PROFIT_PER_POSITION_QUOTE,
                max_long_open_interest: V1_MAX_POSITION_NOTIONAL_QUOTE,
                max_short_open_interest: V1_MAX_POSITION_NOTIONAL_QUOTE,
                haircut: haircut(),
                initial_mode: MarketMode::Active,
            },
        ],
    }
}

fn model() -> Martingaler {
    Martingaler::new(config(64)).unwrap()
}

fn open_cash(
    model: &mut Martingaler,
    id: u64,
    owner: u64,
    market: u16,
    side: PositionSide,
    principal: u128,
) {
    let notional = principal.checked_mul(100).unwrap();
    model
        .open_cash_position(id, owner, market, side, principal, notional, 100)
        .unwrap();
}

fn create_claim(
    model: &mut Martingaler,
    id: u64,
    owner: u64,
    market: u16,
    raw_profit: u128,
) -> ClaimSequence {
    open_cash(model, id, owner, market, PositionSide::Long, 100);
    model
        .close_position(
            id,
            CloseKind::Profit {
                raw_profit,
                favorable_move_ppm: 0,
            },
        )
        .unwrap()
        .new_claim_sequence
        .unwrap()
}

fn fund_bankroll_with_loss(
    model: &mut Martingaler,
    id: u64,
    owner: u64,
    market: u16,
    amount: u128,
) {
    model
        .open_cash_position(id, owner, market, PositionSide::Short, amount, amount, 1)
        .unwrap();
    let close = model.close_position(id, CloseKind::Loss(amount)).unwrap();
    assert_eq!(close.cash_loss_collected, amount);
    assert_eq!(close.cash_principal_returned, 0);
}

fn account_close_outcome(
    outcome: &CloseOutcome,
    custody: &mut u128,
    outflows: &mut u128,
    cash_rewards: &mut u128,
    claim_rewards: &mut u128,
    residual_debt: &mut u128,
    fees: &mut u128,
) {
    let paid = outcome.cash_principal_returned + outcome.payout_paid;
    *custody = custody.checked_sub(paid).unwrap();
    *outflows = outflows.checked_add(paid).unwrap();
    *cash_rewards = cash_rewards
        .checked_add(outcome.cash_loss_collected)
        .unwrap();
    *claim_rewards = claim_rewards
        .checked_add(outcome.claim_loss_extinguished)
        .unwrap();
    *residual_debt = residual_debt
        .checked_add(outcome.residual_bad_debt_added)
        .unwrap();
    *fees = fees.checked_add(outcome.win_fee).unwrap();
}

#[test]
fn oracle_cash_pnl_rounds_losses_up_profits_down_and_is_wide() {
    assert_eq!(
        oracle_cash_pnl(PositionSide::Long, 101, 100, 100),
        Ok(OracleCashPnl::Flat)
    );
    assert_eq!(
        oracle_cash_pnl(PositionSide::Long, 101, 100, 99),
        Ok(OracleCashPnl::Loss(2))
    );
    assert_eq!(
        oracle_cash_pnl(PositionSide::Long, 101, 100, 101),
        Ok(OracleCashPnl::Profit(1))
    );
    assert_eq!(
        oracle_cash_pnl(PositionSide::Short, 101, 100, 101),
        Ok(OracleCashPnl::Loss(2))
    );
    assert_eq!(
        oracle_cash_pnl(PositionSide::Short, 101, 100, 99),
        Ok(OracleCashPnl::Profit(1))
    );
    assert_eq!(
        oracle_cash_pnl(PositionSide::Long, u128::MAX, 2, 1),
        Ok(OracleCashPnl::Loss((u128::MAX / 2) + 1))
    );
    assert_eq!(
        oracle_cash_pnl(PositionSide::Long, u128::MAX, 1, u64::MAX),
        Err(ModelError::ArithmeticOverflow)
    );
    assert_eq!(
        oracle_cash_pnl(PositionSide::Long, 1, 0, 1),
        Err(ModelError::InvalidConfiguration)
    );
}

#[test]
fn oracle_classification_drives_exact_deposited_cash_nonpositive_close() {
    let mut model = model();
    model.deposit_cash_collateral(10, 100).unwrap();
    model
        .open_deposited_cash_position(1, 10, BTC, PositionSide::Long, 100, 1_000, 10)
        .unwrap();
    let loss = match oracle_cash_pnl(PositionSide::Long, 1_000, 100, 95).unwrap() {
        OracleCashPnl::Loss(loss) => loss,
        other => panic!("expected adverse PnL, got {other:?}"),
    };
    let close = model.close_position(1, CloseKind::Loss(loss)).unwrap();
    assert_eq!(loss, 50);
    assert_eq!(close.cash_loss_collected, 50);
    assert_eq!(close.cash_principal_returned, 50);
    assert_eq!(model.protected_principal(), 0);
    assert_eq!(model.free_cash(), 50);
    assert_eq!(model.quote_custody().unwrap(), 50);
    model.check_invariants().unwrap();
}

#[test]
fn favorable_sub_ppm_zero_atom_profit_closes_with_principal_only() {
    let mut zero_atom = model();
    open_cash(&mut zero_atom, 1, 10, BTC, PositionSide::Long, 100);
    assert_eq!(
        oracle_cash_pnl(PositionSide::Long, 1, 2_000_000, 2_000_001),
        Ok(OracleCashPnl::Profit(0))
    );

    let close = zero_atom
        .close_position(
            1,
            CloseKind::Profit {
                raw_profit: 0,
                favorable_move_ppm: 0,
            },
        )
        .unwrap();
    assert_eq!(close.cash_principal_returned, 100);
    assert_eq!(close.raw_profit, 0);
    assert_eq!(close.haircut_withheld, 0);
    assert_eq!(close.win_fee, 0);
    assert_eq!(close.trader_profit, 0);
    assert_eq!(close.payout_paid, 0);
    assert_eq!(close.payout_queued, 0);
    assert_eq!(close.new_claim_sequence, None);
    assert_eq!(zero_atom.protected_principal(), 0);
    assert_eq!(zero_atom.total_queued(), 0);
    assert_eq!(zero_atom.quote_custody().unwrap(), 0);
    zero_atom.check_invariants().unwrap();

    assert_eq!(
        zero_atom.adjust_profit(BTC, 0, 0),
        Err(ModelError::ZeroAmount)
    );
}

#[test]
fn cash_funded_win_returns_principal_and_queues_only_adjusted_profit() {
    let mut model = model();
    open_cash(&mut model, 1, 10, BTC, PositionSide::Long, 100);

    let close = model
        .close_position(
            1,
            CloseKind::Profit {
                raw_profit: 10_000,
                favorable_move_ppm: 0,
            },
        )
        .unwrap();

    assert_eq!(close.cash_principal_returned, 100);
    assert_eq!(close.raw_profit, 10_000);
    assert_eq!(close.haircut_withheld, 1_000);
    assert_eq!(close.win_fee, 180);
    assert_eq!(close.trader_profit, 8_820);
    assert_eq!(close.payout_paid, 0);
    assert_eq!(close.payout_queued, 8_820);
    assert_eq!(model.protected_principal(), 0);
    assert_eq!(model.total_queued(), 8_820);
    assert_eq!(model.quote_custody().unwrap(), 0);
    assert_eq!(model.fee_accrual(), 180);
    model.check_invariants().unwrap();
}

#[test]
fn full_claim_open_preserves_liability_and_profitable_close_resets_priority() {
    let mut model = model();
    let old_sequence = create_claim(&mut model, 1, 10, ETH, 10_000);
    let equity_before = model.lp_equity().unwrap();

    let opened = model
        .open_claim_position(
            2,
            10,
            BTC,
            PositionSide::Short,
            old_sequence,
            8_820,
            8_820,
            1,
        )
        .unwrap();

    assert_eq!(opened.remaining_at_old_priority, 0);
    assert!(model.claim(old_sequence).is_none());
    assert_eq!(model.total_queued(), 0);
    assert_eq!(model.open_claim_collateral(), 8_820);
    assert_eq!(model.lp_equity().unwrap(), equity_before);

    let close = model
        .close_position(
            2,
            CloseKind::Profit {
                raw_profit: 1_000,
                favorable_move_ppm: 0,
            },
        )
        .unwrap();
    assert_eq!(close.cash_principal_returned, 0);
    assert_eq!(close.trader_profit, 882);
    assert_eq!(close.payout_queued, 9_702);
    assert!(close.new_claim_sequence.unwrap() > old_sequence);
    assert_eq!(model.open_claim_collateral(), 0);
    assert_eq!(model.total_queued(), 9_702);
    model.check_invariants().unwrap();
}

#[test]
fn partial_claim_consumption_keeps_remainder_in_place_and_new_value_goes_to_tail() {
    let mut model = model();
    let first = create_claim(&mut model, 1, 10, BTC, 1_000);
    let second = create_claim(&mut model, 2, 20, ETH, 1_000);
    assert_eq!(
        model
            .claims()
            .iter()
            .map(|c| c.sequence)
            .collect::<Vec<_>>(),
        vec![first, second]
    );

    let opened = model
        .open_claim_position(3, 10, BTC, PositionSide::Long, first, 400, 400, 1)
        .unwrap();
    assert_eq!(opened.remaining_at_old_priority, 482);
    assert_eq!(model.claims()[0].sequence, first);
    assert_eq!(model.claims()[0].remaining_face, 482);
    assert_eq!(model.claims()[1].sequence, second);

    let close = model.close_position(3, CloseKind::Flat).unwrap();
    let third = close.new_claim_sequence.unwrap();
    assert!(third > second);
    assert_eq!(close.payout_paid, 0);
    assert_eq!(close.payout_queued, 400);
    assert_eq!(
        model
            .claims()
            .iter()
            .map(|c| c.sequence)
            .collect::<Vec<_>>(),
        vec![first, second, third]
    );
    model.check_invariants().unwrap();
}

#[test]
fn claim_loss_reduces_liability_without_adding_cash() {
    let mut model = model();
    let sequence = create_claim(&mut model, 1, 10, BTC, 1_000);
    model
        .open_claim_position(2, 10, ETH, PositionSide::Long, sequence, 882, 882, 1)
        .unwrap();
    let custody_before = model.quote_custody().unwrap();

    let close = model.close_position(2, CloseKind::Loss(300)).unwrap();

    assert_eq!(close.claim_loss_extinguished, 300);
    assert_eq!(close.cash_loss_collected, 0);
    assert_eq!(close.payout_paid, 0);
    assert_eq!(close.payout_queued, 582);
    assert_eq!(model.claim_loss_reward_basis(), 300);
    assert_eq!(model.cash_loss_reward_basis(), 0);
    assert_eq!(model.quote_custody().unwrap(), custody_before);
    assert_eq!(model.total_queued(), 582);
    model.check_invariants().unwrap();
}

#[test]
fn claim_liquidation_destroys_the_whole_claim_without_cash() {
    let mut model = model();
    let sequence = create_claim(&mut model, 1, 10, BTC, 1_000);
    model
        .open_claim_position(2, 10, BTC, PositionSide::Long, sequence, 882, 88_200, 100)
        .unwrap();

    let close = model
        .close_position(2, CloseKind::Liquidation { gap_loss: 882 })
        .unwrap();

    assert_eq!(close.claim_loss_extinguished, 882);
    assert_eq!(close.payout_paid, 0);
    assert_eq!(close.payout_queued, 0);
    assert_eq!(model.total_queued(), 0);
    assert_eq!(model.open_claim_collateral(), 0);
    assert_eq!(model.claim_loss_reward_basis(), 882);
    assert_eq!(model.quote_custody().unwrap(), 0);
    model.check_invariants().unwrap();
}

#[test]
fn cash_liquidation_funds_the_bankroll_and_only_cash_reward_basis() {
    let mut model = model();
    model
        .open_cash_position(1, 10, BTC, PositionSide::Long, 100, 10_000, 100)
        .unwrap();

    let close = model
        .close_position(1, CloseKind::Liquidation { gap_loss: 100 })
        .unwrap();

    assert_eq!(close.cash_loss_collected, 100);
    assert_eq!(close.claim_loss_extinguished, 0);
    assert_eq!(model.free_cash(), 100);
    assert_eq!(model.cash_loss_reward_basis(), 100);
    assert_eq!(model.claim_loss_reward_basis(), 0);
    assert_eq!(model.quote_custody().unwrap(), 100);
    model.check_invariants().unwrap();
}

#[test]
fn shared_btc_losses_service_older_eth_claims_fifo() {
    let mut model = model();
    let eth_claim = create_claim(&mut model, 1, 10, ETH, 1_000);
    open_cash(&mut model, 2, 20, BTC, PositionSide::Short, 500);
    model.close_position(2, CloseKind::Loss(500)).unwrap();

    assert_eq!(model.free_cash(), 0);
    assert_eq!(model.side_bucket(), 500);
    assert_eq!(model.claims().front().unwrap().sequence, eth_claim);

    let harvest = model.harvest(1).unwrap();
    assert_eq!(harvest.total_paid, 500);
    assert_eq!(harvest.payments[0].sequence, eth_claim);
    assert_eq!(harvest.payments[0].remaining_claim, 382);
    assert_eq!(model.total_queued(), 382);
    assert_eq!(model.side_bucket(), 0);
    model.check_invariants().unwrap();
}

#[test]
fn consuming_the_last_claim_releases_reserved_side_cash() {
    let mut model = model();
    let sequence = create_claim(&mut model, 1, 10, BTC, 1_000);
    open_cash(&mut model, 2, 20, ETH, PositionSide::Short, 300);
    model.close_position(2, CloseKind::Loss(300)).unwrap();
    assert_eq!(model.side_bucket(), 300);

    model
        .open_claim_position(3, 10, BTC, PositionSide::Long, sequence, 882, 882, 1)
        .unwrap();

    assert!(model.claims().is_empty());
    assert_eq!(model.side_bucket(), 0);
    assert_eq!(model.free_cash(), 300);
    let flat = model.close_position(3, CloseKind::Flat).unwrap();
    assert_eq!(flat.payout_paid, 300);
    assert_eq!(flat.payout_queued, 582);
    model.check_invariants().unwrap();
}

#[test]
fn haircut_is_monotonic_size_neutral_and_split_resistant() {
    let mut cfg = config(64);
    cfg.markets[0].haircut.max_rate_ppm = 900_000;
    let model = Martingaler::new(cfg).unwrap();
    let small_move = model.adjust_profit(BTC, 10_000, 100).unwrap();
    let large_move = model.adjust_profit(BTC, 10_000, 10_000).unwrap();
    assert!(large_move.haircut_rate_ppm < small_move.haircut_rate_ppm);
    assert!(large_move.trader_profit > small_move.trader_profit);
    assert!(large_move.trader_profit <= large_move.raw_profit);

    let whole = model.adjust_profit(BTC, 10_000, 10_000).unwrap();
    let half_a = model.adjust_profit(BTC, 5_000, 10_000).unwrap();
    let half_b = model.adjust_profit(BTC, 5_000, 10_000).unwrap();
    assert!(half_a.trader_profit + half_b.trader_profit <= whole.trader_profit);
    assert_eq!(half_a.haircut_rate_ppm, whole.haircut_rate_ppm);
    assert_eq!(half_b.haircut_rate_ppm, whole.haircut_rate_ppm);
}

#[test]
fn fee_is_after_haircut_and_never_withdraws_cash_ahead_of_claims() {
    let mut model = model();
    let sequence = create_claim(&mut model, 1, 10, BTC, 10_000);
    assert_eq!(model.fee_accrual(), 180);
    assert_eq!(model.quote_custody().unwrap(), 0);
    assert_eq!(model.claim(sequence).unwrap().remaining_face, 8_820);

    fund_bankroll_with_loss(&mut model, 2, 20, ETH, 100);
    assert_eq!(model.side_bucket(), 100);
    assert_eq!(model.free_cash(), 0);
    assert_eq!(model.fee_accrual(), 180);
    model.check_invariants().unwrap();
}

#[test]
fn record_capacity_is_reserved_at_open_and_never_rejected_at_close() {
    let mut model = Martingaler::new(config(2)).unwrap();
    let first = create_claim(&mut model, 1, 10, BTC, 1_000);
    let second = create_claim(&mut model, 2, 20, ETH, 1_000);
    assert_eq!(model.record_count(), 2);

    let before_partial = model.clone();
    assert_eq!(
        model.open_claim_position(3, 10, BTC, PositionSide::Long, first, 100, 100, 1),
        Err(ModelError::RecordCapacityExceeded)
    );
    assert_eq!(model, before_partial);

    model
        .open_claim_position(3, 10, BTC, PositionSide::Long, first, 882, 882, 1)
        .unwrap();
    assert_eq!(model.record_count(), 2);
    let close = model
        .close_position(
            3,
            CloseKind::Profit {
                raw_profit: 100,
                favorable_move_ppm: 0,
            },
        )
        .unwrap();
    assert!(close.new_claim_sequence.unwrap() > second);
    assert_eq!(model.record_count(), 2);
    model.check_invariants().unwrap();
}

#[test]
fn capacity_blocks_new_cash_open_but_existing_close_remains_live() {
    let mut model = Martingaler::new(config(1)).unwrap();
    open_cash(&mut model, 1, 10, BTC, PositionSide::Long, 100);
    assert_eq!(
        model.open_cash_position(2, 20, ETH, PositionSide::Short, 100, 100, 1),
        Err(ModelError::RecordCapacityExceeded)
    );
    let close = model
        .close_position(
            1,
            CloseKind::Profit {
                raw_profit: 100,
                favorable_move_ppm: 0,
            },
        )
        .unwrap();
    assert_eq!(close.payout_queued, 88);
    assert_eq!(model.record_count(), 1);
}

#[test]
fn queue_gate_blocks_new_opens_but_not_existing_closes_or_harvest() {
    let mut cfg = config(8);
    cfg.queue_debt_open_gate = 50;
    cfg.total_liability_open_gate = 20_000;
    let mut model = Martingaler::new(cfg).unwrap();
    open_cash(&mut model, 1, 10, BTC, PositionSide::Long, 100);
    open_cash(&mut model, 2, 20, ETH, PositionSide::Short, 100);
    model
        .close_position(
            1,
            CloseKind::Profit {
                raw_profit: 100,
                favorable_move_ppm: 0,
            },
        )
        .unwrap();
    assert_eq!(model.total_queued(), 88);
    assert_eq!(
        model.open_cash_position(3, 30, BTC, PositionSide::Long, 100, 100, 1),
        Err(ModelError::QueueDebtGateExceeded)
    );

    let close = model.close_position(2, CloseKind::Loss(100)).unwrap();
    assert_eq!(close.cash_loss_collected, 100);
    let harvest = model.harvest(1).unwrap();
    assert_eq!(harvest.total_paid, 88);
    assert!(model.claims().is_empty());
    assert_eq!(model.free_cash(), 12);
    model.check_invariants().unwrap();
}

#[test]
fn maximum_profit_is_protocol_derived_and_oversized_win_still_closes() {
    let mut cfg = config(8);
    cfg.total_liability_open_gate = 150_000 * V1_QUOTE_ATOMS_PER_UNIT;
    cfg.queue_debt_open_gate = 150_000 * V1_QUOTE_ATOMS_PER_UNIT;
    let mut model = Martingaler::new(cfg).unwrap();

    model
        .open_cash_position(
            1,
            10,
            BTC,
            PositionSide::Long,
            1_500 * V1_QUOTE_ATOMS_PER_UNIT,
            150_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100,
        )
        .unwrap();
    assert_eq!(
        model.position(1).unwrap().max_net_profit,
        V1_MAX_NET_PROFIT_PER_POSITION_QUOTE
    );
    assert_eq!(
        model.reserved_profit_liability(),
        V1_MAX_NET_PROFIT_PER_POSITION_QUOTE
    );
    assert_eq!(
        model.total_potential_liabilities().unwrap(),
        V1_MAX_NET_PROFIT_PER_POSITION_QUOTE
    );
    assert_eq!(
        model.open_cash_position(
            2,
            20,
            ETH,
            PositionSide::Long,
            600 * V1_QUOTE_ATOMS_PER_UNIT,
            60_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100,
        ),
        Err(ModelError::TotalLiabilityGateExceeded)
    );

    let oversized_profit = 1_000_000 * V1_QUOTE_ATOMS_PER_UNIT;
    let uncapped = model
        .adjust_profit(BTC, oversized_profit, u32::MAX)
        .unwrap();
    assert!(uncapped.trader_profit > V1_MAX_NET_PROFIT_PER_POSITION_QUOTE);
    let close = model
        .close_position(
            1,
            CloseKind::Profit {
                raw_profit: oversized_profit,
                favorable_move_ppm: u32::MAX,
            },
        )
        .unwrap();
    assert_eq!(close.trader_profit, V1_MAX_NET_PROFIT_PER_POSITION_QUOTE);
    assert_eq!(
        close.profit_cap_withheld,
        uncapped.trader_profit - V1_MAX_NET_PROFIT_PER_POSITION_QUOTE
    );
    assert_eq!(close.payout_queued, V1_MAX_NET_PROFIT_PER_POSITION_QUOTE);
    assert_eq!(model.reserved_profit_liability(), 0);
    assert_eq!(
        model.total_potential_liabilities().unwrap(),
        V1_MAX_NET_PROFIT_PER_POSITION_QUOTE
    );
    model.check_invariants().unwrap();
}

#[test]
fn split_positions_cannot_bypass_controller_reserved_profit_cap() {
    let mut model = model();

    model
        .open_cash_position(
            1,
            10,
            BTC,
            PositionSide::Long,
            1_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100,
        )
        .unwrap();
    model
        .open_cash_position(
            2,
            10,
            ETH,
            PositionSide::Short,
            1_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100,
        )
        .unwrap();
    model
        .open_cash_position(
            3,
            10,
            BTC,
            PositionSide::Short,
            500 * V1_QUOTE_ATOMS_PER_UNIT,
            50_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100,
        )
        .unwrap();
    assert_eq!(
        model.reserved_profit_for_controller(10).unwrap(),
        V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE
    );

    let before_rejection = model.clone();
    assert_eq!(
        model.open_cash_position(4, 10, ETH, PositionSide::Long, 1, 1, 1),
        Err(ModelError::ControllerReservedProfitCapExceeded)
    );
    assert_eq!(model, before_rejection);

    model
        .open_cash_position(4, 20, ETH, PositionSide::Long, 1, 1, 1)
        .unwrap();
    assert_eq!(model.reserved_profit_for_controller(20).unwrap(), 1);

    model.close_position(1, CloseKind::Flat).unwrap();
    model
        .open_cash_position(
            5,
            10,
            BTC,
            PositionSide::Long,
            1_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100,
        )
        .unwrap();
    assert_eq!(
        model.reserved_profit_for_controller(10).unwrap(),
        V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE
    );
    model.check_invariants().unwrap();
}

#[test]
fn approved_display_caps_are_exact_six_decimal_quote_atom_values() {
    assert_eq!(V1_QUOTE_DECIMALS, 6);
    assert_eq!(V1_QUOTE_ATOMS_PER_UNIT, 1_000_000);
    assert_eq!(V1_MAX_POSITION_NOTIONAL_QUOTE, 1_000_000_000_000);
    assert_eq!(V1_MAX_NET_PROFIT_PER_POSITION_QUOTE, 100_000_000_000);
    assert_eq!(V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE, 250_000_000_000);
    assert_eq!(V1_TOTAL_POTENTIAL_LIABILITY_QUOTE, 1_000_000_000_000);
}

#[test]
fn v1_guarded_admission_enforces_ten_dollars_and_immutable_dynamic_factor() {
    let mut model = model();
    assert_eq!(
        model.open_cash_position_guarded(
            1,
            10,
            BTC,
            PositionSide::Long,
            V1_MIN_COLLATERAL_QUOTE - 1,
            (V1_MIN_COLLATERAL_QUOTE - 1) * 100,
            100,
            0,
        ),
        Err(ModelError::ZeroPrincipal)
    );

    for id in 1..=2 {
        model
            .open_cash_position_guarded(
                id,
                10,
                BTC,
                PositionSide::Long,
                1_000 * V1_QUOTE_ATOMS_PER_UNIT,
                100_000 * V1_QUOTE_ATOMS_PER_UNIT,
                100,
                10_000,
            )
            .unwrap();
    }
    let quote = model
        .quote_payout_factor(10, BTC, 100_000 * V1_QUOTE_ATOMS_PER_UNIT, 0)
        .unwrap();
    assert_eq!(quote.payout_factor_bps, 5_000);
    assert_eq!(quote.max_payable_profit, 50_000 * V1_QUOTE_ATOMS_PER_UNIT);
    let before_guard = model.clone();
    assert_eq!(
        model.open_cash_position_guarded(
            3,
            10,
            BTC,
            PositionSide::Short,
            1_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100,
            6_000,
        ),
        Err(ModelError::PayoutGuardNotMet)
    );
    assert_eq!(model, before_guard);
    model
        .open_cash_position_guarded(
            3,
            10,
            BTC,
            PositionSide::Short,
            1_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100_000 * V1_QUOTE_ATOMS_PER_UNIT,
            100,
            5_000,
        )
        .unwrap();
    assert_eq!(
        model.position(3).unwrap().max_net_profit,
        50_000 * V1_QUOTE_ATOMS_PER_UNIT
    );
    assert_eq!(
        model.quote_payout_factor(10, BTC, 100_000 * V1_QUOTE_ATOMS_PER_UNIT, 0),
        Err(ModelError::PayoutFactorBelowFloor)
    );
    model.check_invariants().unwrap();
}

#[test]
fn claim_funded_oversized_win_is_capped_and_settles_at_queue_tail() {
    let mut model = model();
    model
        .open_cash_position(1, 10, BTC, PositionSide::Long, 1_000, 100_000, 100)
        .unwrap();
    let first_close = model
        .close_position(
            1,
            CloseKind::Profit {
                raw_profit: 1_000_000,
                favorable_move_ppm: u32::MAX,
            },
        )
        .unwrap();
    assert_eq!(first_close.trader_profit, 100_000);
    let source_sequence = first_close.new_claim_sequence.unwrap();

    model
        .open_claim_position(
            2,
            10,
            ETH,
            PositionSide::Short,
            source_sequence,
            100_000,
            100_000,
            1,
        )
        .unwrap();
    let uncapped = model.adjust_profit(BTC, 1_000_000, u32::MAX).unwrap();
    let close = model
        .close_position(
            2,
            CloseKind::Profit {
                raw_profit: 1_000_000,
                favorable_move_ppm: u32::MAX,
            },
        )
        .unwrap();

    assert_eq!(close.trader_profit, 100_000);
    assert_eq!(close.profit_cap_withheld, uncapped.trader_profit - 100_000);
    assert_eq!(close.payout_queued, 200_000);
    assert!(close.new_claim_sequence.unwrap() > source_sequence);
    assert_eq!(model.open_claim_collateral(), 0);
    assert_eq!(model.reserved_profit_liability(), 0);
    assert_eq!(model.total_queued(), 200_000);
    model.check_invariants().unwrap();
}

#[test]
fn market_limits_are_independent_and_leverage_is_capped_at_500x() {
    let mut cfg = config(64);
    cfg.markets[0].max_position_notional = 150;
    cfg.markets[0].max_net_profit_per_position = 150;
    cfg.markets[0].max_long_open_interest = 150;
    cfg.markets[0].max_short_open_interest = 120;
    cfg.markets[1].max_leverage_x = 500;
    let mut model = Martingaler::new(cfg).unwrap();
    model
        .open_cash_position(1, 10, BTC, PositionSide::Long, 100, 100, 1)
        .unwrap();
    model
        .open_cash_position(2, 20, ETH, PositionSide::Long, 150, 150, 1)
        .unwrap();
    assert_eq!(model.market(BTC).unwrap().long_open_interest, 100);
    assert_eq!(model.market(ETH).unwrap().long_open_interest, 150);

    let before = model.clone();
    assert_eq!(
        model.open_cash_position(3, 30, BTC, PositionSide::Long, 151, 151, 1),
        Err(ModelError::PositionNotionalCapExceeded)
    );
    assert_eq!(model, before);

    assert_eq!(
        model.open_cash_position(4, 40, BTC, PositionSide::Long, 51, 51, 1),
        Err(ModelError::LongOpenInterestExceeded)
    );
    assert_eq!(model, before);

    model
        .open_cash_position(5, 50, BTC, PositionSide::Short, 120, 120, 1)
        .unwrap();
    let before_short_overflow = model.clone();
    assert_eq!(
        model.open_cash_position(6, 60, BTC, PositionSide::Short, 1, 1, 1),
        Err(ModelError::ShortOpenInterestExceeded)
    );
    assert_eq!(model, before_short_overflow);

    model
        .open_cash_position(7, 70, ETH, PositionSide::Short, 1, 500, 500)
        .unwrap();
    let before_leverage = model.clone();
    assert_eq!(
        model.open_cash_position(8, 80, ETH, PositionSide::Short, 1, 501, 501),
        Err(ModelError::InvalidLeverage)
    );
    assert_eq!(model, before_leverage);
    model.check_invariants().unwrap();
}

#[test]
fn fixed_point_zero_rate_fee_and_checked_arithmetic_boundaries() {
    let mut exact_cfg = config(8);
    exact_cfg.markets[0].haircut = HaircutConfig {
        base_rate_ppm: 0,
        deadband_move_ppm: 0,
        linear_coefficient_ppm: 0,
        quadratic_coefficient_ppm: 0,
        max_rate_ppm: 0,
        win_fee_ppm: 0,
    };
    let exact = Martingaler::new(exact_cfg).unwrap();
    assert_eq!(
        exact
            .adjust_profit(BTC, u128::MAX, 0)
            .unwrap()
            .trader_profit,
        u128::MAX
    );
    assert_eq!(
        exact
            .adjust_profit(BTC, u128::MAX - 1, 0)
            .unwrap()
            .trader_profit,
        u128::MAX - 1
    );
    assert_eq!(exact.adjust_profit(BTC, 0, 0), Err(ModelError::ZeroAmount));

    let mut capped_cfg = config(8);
    capped_cfg.markets[0].haircut.max_rate_ppm = 900_000;
    capped_cfg.markets[0].haircut.linear_coefficient_ppm = u32::MAX;
    capped_cfg.markets[0].haircut.quadratic_coefficient_ppm = u32::MAX;
    let capped = Martingaler::new(capped_cfg).unwrap();
    let smallest_move = capped.adjust_profit(BTC, u128::MAX, 0).unwrap();
    let largest_move = capped.adjust_profit(BTC, u128::MAX, u32::MAX).unwrap();
    assert_eq!(smallest_move.haircut_rate_ppm, 900_000);
    assert!(largest_move.haircut_rate_ppm >= 100_000);
    assert!(largest_move.haircut_rate_ppm < smallest_move.haircut_rate_ppm);

    let mut fee_cfg = config(8);
    fee_cfg.markets[0].haircut.base_rate_ppm = 0;
    fee_cfg.markets[0].haircut.max_rate_ppm = 0;
    fee_cfg.markets[0].haircut.win_fee_ppm = 1_000_000;
    let fee_model = Martingaler::new(fee_cfg).unwrap();
    let full_fee = fee_model.adjust_profit(BTC, u128::MAX, 0).unwrap();
    assert_eq!(full_fee.post_haircut_profit, u128::MAX);
    assert_eq!(full_fee.win_fee, u128::MAX);
    assert_eq!(full_fee.trader_profit, 0);

    let mut zero_model = model();
    assert_eq!(
        zero_model.open_cash_position(1, 1, BTC, PositionSide::Long, 0, 1, 1),
        Err(ModelError::ZeroPrincipal)
    );
    assert_eq!(
        zero_model.open_cash_position(1, 1, BTC, PositionSide::Long, 1, 0, 1),
        Err(ModelError::ZeroNotional)
    );
    assert_eq!(
        zero_model.open_cash_position(1, 1, BTC, PositionSide::Long, 1, 1, 0),
        Err(ModelError::InvalidLeverage)
    );
    assert_eq!(zero_model, model());
}

#[test]
fn divergence_breaker_blocks_opens_but_preserves_existing_close() {
    let mut model = model();
    open_cash(&mut model, 1, 10, BTC, PositionSide::Long, 100);
    model.trigger_divergence(BTC).unwrap();
    assert_eq!(model.market(BTC).unwrap().mode, MarketMode::CloseOnly);
    assert_eq!(
        model.open_cash_position(2, 20, BTC, PositionSide::Short, 100, 100, 1),
        Err(ModelError::MarketNotActive)
    );

    let close = model.close_position(1, CloseKind::Flat).unwrap();
    assert_eq!(close.cash_principal_returned, 100);
    model
        .advance_market_mode(BTC, MarketMode::ClaimsOnly)
        .unwrap();
    assert_eq!(model.market(BTC).unwrap().mode, MarketMode::ClaimsOnly);
    model.check_invariants().unwrap();
}

#[test]
fn claims_survive_wind_down_and_terminal_requires_a_clean_pool() {
    let mut model = model();
    create_claim(&mut model, 1, 10, BTC, 100);
    model.trigger_divergence(BTC).unwrap();
    model
        .advance_market_mode(BTC, MarketMode::ClaimsOnly)
        .unwrap();
    assert_eq!(
        model.advance_market_mode(BTC, MarketMode::Terminal),
        Err(ModelError::TerminalStateUnavailable)
    );
    assert_eq!(model.claims().len(), 1);

    fund_bankroll_with_loss(&mut model, 2, 20, ETH, 88);
    model.harvest(2).unwrap();
    assert!(model.claims().is_empty());
    assert_eq!(
        model.advance_market_mode(BTC, MarketMode::Terminal),
        Err(ModelError::TerminalStateUnavailable)
    );
    model.trigger_divergence(ETH).unwrap();
    model
        .advance_market_mode(ETH, MarketMode::ClaimsOnly)
        .unwrap();
    model
        .advance_market_mode(BTC, MarketMode::Terminal)
        .unwrap();
    assert_eq!(model.market(BTC).unwrap().mode, MarketMode::Terminal);
    model
        .advance_market_mode(ETH, MarketMode::Terminal)
        .unwrap();
    model.check_invariants().unwrap();
}

#[test]
fn claim_gap_loss_saturates_residual_without_blocking_close() {
    let mut cfg = config(8);
    cfg.residual_bad_debt_cap = 10;
    let mut model = Martingaler::new(cfg).unwrap();
    let sequence = create_claim(&mut model, 1, 10, BTC, 100);
    model
        .open_claim_position(2, 10, BTC, PositionSide::Long, sequence, 88, 88, 1)
        .unwrap();

    let close = model.close_position(2, CloseKind::Loss(1_000)).unwrap();
    assert_eq!(close.claim_loss_extinguished, 88);
    assert_eq!(close.residual_bad_debt_added, 912);
    assert_eq!(model.residual_bad_debt(), 10);
    assert!(model.residual_bad_debt_saturated());
    assert_eq!(
        model.total_liabilities(),
        Err(ModelError::ResidualBadDebtSaturated)
    );
    assert_eq!(model.lp_equity(), Err(ModelError::ResidualBadDebtSaturated));
    model.check_invariants().unwrap();
}

#[test]
fn reward_and_fee_counters_saturate_without_affecting_solvency() {
    let mut cfg = config(8);
    cfg.reward_basis_cap = 50;
    cfg.fee_accrual_cap = 10;
    let mut model = Martingaler::new(cfg).unwrap();
    open_cash(&mut model, 1, 10, BTC, PositionSide::Long, 100);
    model.close_position(1, CloseKind::Loss(100)).unwrap();
    assert_eq!(model.cash_loss_reward_basis(), 50);
    assert!(model.cash_loss_reward_basis_saturated());
    assert_eq!(model.free_cash(), 100);

    open_cash(&mut model, 2, 20, BTC, PositionSide::Long, 100);
    model
        .close_position(
            2,
            CloseKind::Profit {
                raw_profit: 10_000,
                favorable_move_ppm: 0,
            },
        )
        .unwrap();
    assert_eq!(model.fee_accrual(), 10);
    assert!(model.fee_accrual_saturated());
    model.check_invariants().unwrap();
}

#[test]
fn claim_owner_is_enforced_and_rejection_is_atomic() {
    let mut model = model();
    let sequence = create_claim(&mut model, 1, 10, BTC, 1_000);
    let before = model.clone();
    assert_eq!(
        model.open_claim_position(2, 99, BTC, PositionSide::Long, sequence, 100, 100, 1,),
        Err(ModelError::ClaimOwnerMismatch)
    );
    assert_eq!(model, before);
}

#[test]
fn bounded_harvest_preserves_fifo_and_dust() {
    let mut model = model();
    let first = create_claim(&mut model, 1, 10, BTC, 3);
    let second = create_claim(&mut model, 2, 20, ETH, 4);
    let third = create_claim(&mut model, 3, 30, BTC, 5);
    assert!(model.claims().iter().all(|claim| claim.is_dust));
    fund_bankroll_with_loss(&mut model, 4, 40, ETH, 12);

    let first_harvest = model.harvest(usize::MAX).unwrap();
    assert_eq!(first_harvest.entries_processed, 2);
    assert_eq!(first_harvest.payments[0].sequence, first);
    assert_eq!(first_harvest.payments[1].sequence, second);
    assert_eq!(model.claims().front().unwrap().sequence, third);
    let second_harvest = model.harvest(usize::MAX).unwrap();
    assert_eq!(second_harvest.entries_processed, 1);
    assert!(model.claims().is_empty());
    model.check_invariants().unwrap();
}

#[test]
fn early_winning_streak_from_zero_liquidity_keeps_closes_live() {
    let mut cfg = config(32);
    cfg.queue_debt_open_gate = 1_000_000;
    cfg.total_liability_open_gate = V1_TOTAL_POTENTIAL_LIABILITY_QUOTE;
    let mut model = Martingaler::new(cfg).unwrap();
    for id in 1..=20 {
        open_cash(&mut model, id, id, BTC, PositionSide::Long, 100);
        let close = model
            .close_position(
                id,
                CloseKind::Profit {
                    raw_profit: 1_000,
                    favorable_move_ppm: 0,
                },
            )
            .unwrap();
        assert_eq!(close.payout_queued, 882);
    }
    assert_eq!(model.claims().len(), 20);
    assert_eq!(model.total_queued(), 17_640);
    assert_eq!(model.quote_custody().unwrap(), 0);
    model.check_invariants().unwrap();
}

#[test]
fn same_controller_loss_mining_cycle_conserves_quote_value() {
    let mut model = model();
    open_cash(&mut model, 1, 99, BTC, PositionSide::Short, 100);
    model.close_position(1, CloseKind::Loss(100)).unwrap();
    assert_eq!(model.free_cash(), 100);

    open_cash(&mut model, 2, 99, BTC, PositionSide::Long, 100);
    let win = model
        .close_position(
            2,
            CloseKind::Profit {
                raw_profit: 100,
                favorable_move_ppm: 0,
            },
        )
        .unwrap();
    assert_eq!(win.trader_profit, 88);
    assert_eq!(win.payout_paid, 88);
    assert_eq!(model.free_cash(), 12);
    assert_eq!(model.cash_loss_reward_basis(), 100);
    assert_eq!(model.quote_custody().unwrap(), 12);
    assert_eq!(
        model.cumulative_external_inflows(),
        model.quote_custody().unwrap() + model.cumulative_external_outflows()
    );
    model.check_invariants().unwrap();
}

#[test]
fn separate_pool_instances_do_not_interfere() {
    let mut first = model();
    let mut second_cfg = config(64);
    second_cfg.risk_pool_id = 88;
    let mut second = Martingaler::new(second_cfg).unwrap();
    open_cash(&mut second, 1, 10, ETH, PositionSide::Long, 100);
    let second_before = second.clone();

    create_claim(&mut first, 1, 10, BTC, 1_000);
    fund_bankroll_with_loss(&mut first, 2, 20, ETH, 500);
    first.harvest(1).unwrap();

    assert_eq!(second, second_before);
    assert_eq!(first.risk_pool_id(), 77);
    assert_eq!(second.risk_pool_id(), 88);
}

#[test]
fn invalid_configuration_and_arithmetic_rejections_are_atomic() {
    let mut invalid = config(0);
    assert_eq!(
        Martingaler::new(invalid.clone()),
        Err(ModelError::InvalidConfiguration)
    );
    invalid.max_records = 1;
    invalid.residual_bad_debt_cap = 0;
    assert_eq!(
        Martingaler::new(invalid.clone()),
        Err(ModelError::InvalidConfiguration)
    );
    invalid.max_records = 4;
    invalid.residual_bad_debt_cap = 1_000_000;
    invalid.markets[0].max_leverage_x = 501;
    assert_eq!(
        Martingaler::new(invalid),
        Err(ModelError::InvalidConfiguration)
    );

    let mut above_position_cap = config(4);
    above_position_cap.markets[0].max_position_notional = V1_MAX_POSITION_NOTIONAL_QUOTE + 1;
    assert_eq!(
        Martingaler::new(above_position_cap),
        Err(ModelError::InvalidConfiguration)
    );

    let mut above_profit_cap = config(4);
    above_profit_cap.markets[0].max_net_profit_per_position =
        V1_MAX_NET_PROFIT_PER_POSITION_QUOTE + 1;
    assert_eq!(
        Martingaler::new(above_profit_cap),
        Err(ModelError::InvalidConfiguration)
    );

    let mut above_controller_cap = config(4);
    above_controller_cap.max_reserved_profit_per_controller =
        V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE + 1;
    assert_eq!(
        Martingaler::new(above_controller_cap),
        Err(ModelError::InvalidConfiguration)
    );

    let mut above_total_liability_cap = config(4);
    above_total_liability_cap.total_liability_open_gate = V1_TOTAL_POTENTIAL_LIABILITY_QUOTE + 1;
    assert_eq!(
        Martingaler::new(above_total_liability_cap),
        Err(ModelError::InvalidConfiguration)
    );

    let mut model = model();
    let before = model.clone();
    assert_eq!(
        model.open_cash_position(1, 1, BTC, PositionSide::Long, 1, u128::MAX, 100),
        Err(ModelError::InvalidLeverage)
    );
    assert_eq!(model, before);
}

#[test]
fn controller_deposits_are_idle_owned_collateral_and_surplus_is_not_bankroll() {
    let mut model = model();
    model.deposit_cash_collateral(10, 400).unwrap();
    model.deposit_cash_collateral(20, 150).unwrap();
    model.deposit_cash_collateral(10, 50).unwrap();

    assert_eq!(model.available_cash_for_controller(10), 450);
    assert_eq!(model.available_cash_for_controller(20), 150);
    assert_eq!(model.available_cash_for_controller(30), 0);
    assert_eq!(model.idle_cash_collateral(), 600);
    assert_eq!(model.unallocated_vault_surplus(), 0);
    assert_eq!(model.quote_custody().unwrap(), 600);
    assert_eq!(model.cumulative_external_inflows(), 600);
    assert_eq!(model.free_cash(), 0);
    assert_eq!(model.side_bucket(), 0);
    assert_eq!(model.protected_principal(), 0);
    assert_eq!(model.total_queued(), 0);
    assert_eq!(
        model.lp_equity().unwrap(),
        SignedMagnitude {
            is_negative: false,
            magnitude: 0,
        }
    );

    model.record_unallocated_vault_surplus(25).unwrap();
    assert_eq!(model.idle_cash_collateral(), 600);
    assert_eq!(model.unallocated_vault_surplus(), 25);
    assert_eq!(model.quote_custody().unwrap(), 625);
    assert_eq!(model.cumulative_external_inflows(), 625);
    assert_eq!(model.available_cash_for_controller(10), 450);
    assert_eq!(model.available_cash_for_controller(20), 150);
    assert_eq!(model.free_cash(), 0);
    assert_eq!(
        model.lp_equity().unwrap(),
        SignedMagnitude {
            is_negative: false,
            magnitude: 0,
        }
    );
    model.check_invariants().unwrap();
}

#[test]
fn deposited_cash_open_reclassifies_custody_without_double_counting_inflow() {
    let mut model = model();
    model.deposit_cash_collateral(10, 400).unwrap();
    model.deposit_cash_collateral(20, 150).unwrap();
    let custody_before = model.quote_custody().unwrap();
    let inflows_before = model.cumulative_external_inflows();

    model
        .open_deposited_cash_position(1, 10, BTC, PositionSide::Long, 100, 10_000, 100)
        .unwrap();

    assert_eq!(model.available_cash_for_controller(10), 300);
    assert_eq!(model.available_cash_for_controller(20), 150);
    assert_eq!(model.idle_cash_collateral(), 450);
    assert_eq!(model.protected_principal(), 100);
    assert_eq!(model.quote_custody().unwrap(), custody_before);
    assert_eq!(model.cumulative_external_inflows(), inflows_before);
    assert_eq!(model.cumulative_external_outflows(), 0);
    assert_eq!(model.market(BTC).unwrap().long_open_interest, 10_000);
    assert_eq!(model.reserved_profit_liability(), 10_000);
    model.check_invariants().unwrap();
}

#[test]
fn deposited_cash_open_is_owner_isolated_and_atomic_on_failure() {
    let mut model = model();
    model.deposit_cash_collateral(10, 100).unwrap();
    model.deposit_cash_collateral(20, 500).unwrap();

    let before = model.clone();
    assert_eq!(
        model.open_deposited_cash_position(1, 10, BTC, PositionSide::Short, 101, 10_100, 100,),
        Err(ModelError::InsufficientCashCollateral)
    );
    assert_eq!(model, before);

    assert_eq!(
        model.open_deposited_cash_position(1, 0, BTC, PositionSide::Short, 1, 1, 1,),
        Err(ModelError::InvalidController)
    );
    assert_eq!(model, before);
}

#[test]
fn controller_deposit_rejections_are_atomic_and_classic_spl_bounded() {
    let mut model = model();
    let pristine = model.clone();
    assert_eq!(
        model.deposit_cash_collateral(0, 1),
        Err(ModelError::InvalidController)
    );
    assert_eq!(model, pristine);
    assert_eq!(
        model.deposit_cash_collateral(1, 0),
        Err(ModelError::ZeroAmount)
    );
    assert_eq!(model, pristine);
    assert_eq!(
        model.record_unallocated_vault_surplus(0),
        Err(ModelError::ZeroAmount)
    );
    assert_eq!(model, pristine);

    model.deposit_cash_collateral(1, u64::MAX).unwrap();
    let full = model.clone();
    assert_eq!(
        model.deposit_cash_collateral(1, 1),
        Err(ModelError::CustodyCapacityExceeded)
    );
    assert_eq!(model, full);
    assert_eq!(
        model.record_unallocated_vault_surplus(1),
        Err(ModelError::CustodyCapacityExceeded)
    );
    assert_eq!(model, full);
    model.check_invariants().unwrap();
}

proptest! {
    #[test]
    fn random_operations_preserve_invariants_and_independent_custody(
        operations in prop::collection::vec(
            (0u8..7, 1u16..65, 1u16..500, any::<bool>(), any::<bool>()),
            1..250,
        )
    ) {
        let mut cfg = config(128);
        cfg.residual_bad_debt_cap = u128::MAX / 4;
        cfg.reward_basis_cap = u128::MAX;
        cfg.fee_accrual_cap = u128::MAX;
        let mut model = Martingaler::new(cfg).unwrap();
        let mut custody_oracle = 0u128;
        let mut inflow_oracle = 0u128;
        let mut outflow_oracle = 0u128;
        let mut cash_reward_oracle = 0u128;
        let mut claim_reward_oracle = 0u128;
        let mut residual_oracle = 0u128;
        let mut fee_oracle = 0u128;

        for (kind, raw_id, raw_amount, market_bit, side_bit) in operations {
            let id = u64::from(raw_id);
            let amount = u128::from(raw_amount);
            let market = if market_bit { BTC } else { ETH };
            let side = if side_bit { PositionSide::Long } else { PositionSide::Short };
            let before = model.clone();

            let result = match kind {
                0 => model
                    .open_cash_position(id, id, market, side, amount, amount, 1)
                    .map(|()| {
                        custody_oracle = custody_oracle.checked_add(amount).unwrap();
                        inflow_oracle = inflow_oracle.checked_add(amount).unwrap();
                    }),
                1 => model.close_position(id, CloseKind::Loss(amount)).map(|outcome| {
                    account_close_outcome(
                        &outcome,
                        &mut custody_oracle,
                        &mut outflow_oracle,
                        &mut cash_reward_oracle,
                        &mut claim_reward_oracle,
                        &mut residual_oracle,
                        &mut fee_oracle,
                    );
                }),
                2 => model
                    .close_position(
                        id,
                        CloseKind::Profit {
                            raw_profit: amount,
                            favorable_move_ppm: u32::from(raw_amount),
                        },
                    )
                    .map(|outcome| {
                        account_close_outcome(
                            &outcome,
                            &mut custody_oracle,
                            &mut outflow_oracle,
                            &mut cash_reward_oracle,
                            &mut claim_reward_oracle,
                            &mut residual_oracle,
                            &mut fee_oracle,
                        );
                    }),
                3 => model.close_position(id, CloseKind::Flat).map(|outcome| {
                    account_close_outcome(
                        &outcome,
                        &mut custody_oracle,
                        &mut outflow_oracle,
                        &mut cash_reward_oracle,
                        &mut claim_reward_oracle,
                        &mut residual_oracle,
                        &mut fee_oracle,
                    );
                }),
                4 => model
                    .close_position(id, CloseKind::Liquidation { gap_loss: amount })
                    .map(|outcome| {
                        account_close_outcome(
                            &outcome,
                            &mut custody_oracle,
                            &mut outflow_oracle,
                            &mut cash_reward_oracle,
                            &mut claim_reward_oracle,
                            &mut residual_oracle,
                            &mut fee_oracle,
                        );
                    }),
                5 => model.harvest(usize::from(raw_amount % 4)).map(|outcome| {
                    custody_oracle = custody_oracle.checked_sub(outcome.total_paid).unwrap();
                    outflow_oracle = outflow_oracle.checked_add(outcome.total_paid).unwrap();
                }),
                _ => {
                    let source = model.claims().front().copied();
                    match source {
                        Some(claim) => {
                            let consume = core::cmp::min(amount, claim.remaining_face);
                            model
                                .open_claim_position(
                                    id,
                                    claim.controller_id,
                                    market,
                                    side,
                                    claim.sequence,
                                    consume,
                                    consume,
                                    1,
                                )
                                .map(|_| ())
                        }
                        None => Err(ModelError::UnknownClaim),
                    }
                }
            };

            if result.is_err() {
                prop_assert_eq!(&model, &before);
            }
            prop_assert!(model.check_invariants().is_ok());
            prop_assert_eq!(model.quote_custody().unwrap(), custody_oracle);
            prop_assert_eq!(model.cumulative_external_inflows(), inflow_oracle);
            prop_assert_eq!(model.cumulative_external_outflows(), outflow_oracle);
            prop_assert_eq!(inflow_oracle, custody_oracle + outflow_oracle);
            let queued_sum: u128 = model.claims().iter().map(|claim| claim.remaining_face).sum();
            prop_assert_eq!(model.total_queued(), queued_sum);
            let positions: Vec<_> = (1..=64)
                .filter_map(|position_id| model.position(position_id).copied())
                .collect();
            let protected_sum: u128 = positions
                .iter()
                .map(|position| match position.funding {
                    FundingKind::Cash { protected_principal } => protected_principal,
                    FundingKind::Claim { .. } => 0,
                })
                .sum();
            let open_claim_sum: u128 = positions
                .iter()
                .map(|position| match position.funding {
                    FundingKind::Cash { .. } => 0,
                    FundingKind::Claim { contingent_collateral, .. } => contingent_collateral,
                })
                .sum();
            let reserved_sum: u128 = positions
                .iter()
                .map(|position| position.reserved_trader_profit)
                .sum();
            prop_assert_eq!(model.protected_principal(), protected_sum);
            prop_assert_eq!(model.open_claim_collateral(), open_claim_sum);
            prop_assert_eq!(model.reserved_profit_liability(), reserved_sum);
            prop_assert_eq!(model.cash_loss_reward_basis(), cash_reward_oracle);
            prop_assert_eq!(model.claim_loss_reward_basis(), claim_reward_oracle);
            prop_assert_eq!(model.residual_bad_debt(), residual_oracle);
            prop_assert_eq!(model.fee_accrual(), fee_oracle);
            prop_assert_eq!(
                model.total_liabilities().unwrap(),
                queued_sum + open_claim_sum + residual_oracle
            );
            prop_assert_eq!(
                model.total_potential_liabilities().unwrap(),
                queued_sum + open_claim_sum + residual_oracle + reserved_sum
            );

            let bankroll_cash = custody_oracle.checked_sub(protected_sum).unwrap();
            if model.claims().is_empty() {
                prop_assert_eq!(model.free_cash(), bankroll_cash);
                prop_assert_eq!(model.side_bucket(), 0);
            } else {
                prop_assert_eq!(model.free_cash(), 0);
                prop_assert_eq!(model.side_bucket(), bankroll_cash);
            }

            let mut previous_sequence = 0u128;
            for claim in model.claims() {
                prop_assert!(claim.sequence > previous_sequence);
                previous_sequence = claim.sequence;
            }

            for market_id in [BTC, ETH] {
                let long_sum: u128 = positions
                    .iter()
                    .filter(|position| {
                        position.market_id == market_id && position.side == PositionSide::Long
                    })
                    .map(|position| position.notional)
                    .sum();
                let short_sum: u128 = positions
                    .iter()
                    .filter(|position| {
                        position.market_id == market_id && position.side == PositionSide::Short
                    })
                    .map(|position| position.notional)
                    .sum();
                prop_assert_eq!(model.market(market_id).unwrap().long_open_interest, long_sum);
                prop_assert_eq!(model.market(market_id).unwrap().short_open_interest, short_sum);
            }
        }
    }
}

#[test]
fn underwater_equity_includes_open_and_queued_claim_liabilities() {
    let mut model = model();
    let sequence = create_claim(&mut model, 1, 10, BTC, 1_000);
    assert_eq!(
        model.lp_equity().unwrap(),
        SignedMagnitude {
            is_negative: true,
            magnitude: 882,
        }
    );
    model
        .open_claim_position(2, 10, ETH, PositionSide::Long, sequence, 400, 400, 1)
        .unwrap();
    assert_eq!(model.total_queued(), 482);
    assert_eq!(model.open_claim_collateral(), 400);
    assert_eq!(
        model.lp_equity().unwrap(),
        SignedMagnitude {
            is_negative: true,
            magnitude: 882,
        }
    );
}
