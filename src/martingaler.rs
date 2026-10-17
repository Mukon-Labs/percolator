//! Pure reference accounting for Ninja Martingaler V1.
//!
//! This module deliberately has no Solana account or instruction semantics.
//! It models shared quote custody, isolated cash- and claim-funded positions,
//! a strict FIFO payout queue, and bounded control surfaces before an on-chain
//! layout is designed.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use core::cmp::Ordering;

pub use crate::wide_math::{mul_div_ceil_u256, wide_mul_div_floor_u128, U256};

pub const RATE_SCALE_PPM: u128 = 1_000_000;
pub const V1_MAX_LEVERAGE_X: u16 = 100;
pub const V1_QUOTE_DECIMALS: u8 = 6;
pub const V1_QUOTE_ATOMS_PER_UNIT: u128 = 1_000_000;
pub const V1_MAX_POSITION_NOTIONAL_QUOTE_UNITS: u128 = 1_000_000;
pub const V1_MAX_NET_PROFIT_PER_POSITION_QUOTE_UNITS: u128 = 100_000;
pub const V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE_UNITS: u128 = 250_000;
pub const V1_TOTAL_POTENTIAL_LIABILITY_QUOTE_UNITS: u128 = 1_000_000;
pub const V1_MAX_POSITION_NOTIONAL_QUOTE: u128 =
    V1_MAX_POSITION_NOTIONAL_QUOTE_UNITS * V1_QUOTE_ATOMS_PER_UNIT;
pub const V1_MAX_NET_PROFIT_PER_POSITION_QUOTE: u128 =
    V1_MAX_NET_PROFIT_PER_POSITION_QUOTE_UNITS * V1_QUOTE_ATOMS_PER_UNIT;
pub const V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE: u128 =
    V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE_UNITS * V1_QUOTE_ATOMS_PER_UNIT;
pub const V1_TOTAL_POTENTIAL_LIABILITY_QUOTE: u128 =
    V1_TOTAL_POTENTIAL_LIABILITY_QUOTE_UNITS * V1_QUOTE_ATOMS_PER_UNIT;

pub type PositionId = u64;
pub type ControllerId = u64;
pub type MarketId = u16;
pub type RiskPoolId = u64;
pub type ClaimSequence = u128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionSide {
    Long,
    Short,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarketMode {
    Active,
    CloseOnly,
    ClaimsOnly,
    Terminal,
}

impl MarketMode {
    fn rank(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::CloseOnly => 1,
            Self::ClaimsOnly => 2,
            Self::Terminal => 3,
        }
    }

    fn permits_close(self) -> bool {
        matches!(self, Self::Active | Self::CloseOnly)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarketConfig {
    pub market_id: MarketId,
    pub max_leverage_x: u16,
    pub max_position_notional: u128,
    pub max_net_profit_per_position: u128,
    pub max_long_open_interest: u128,
    pub max_short_open_interest: u128,
    pub haircut: HaircutConfig,
    pub initial_mode: MarketMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarketState {
    pub config: MarketConfig,
    pub mode: MarketMode,
    pub long_open_interest: u128,
    pub short_open_interest: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HaircutConfig {
    /// Winning-profit reduction approached as favorable movement grows.
    pub base_rate_ppm: u32,
    /// Favorable relative move ignored by the decay terms.
    pub deadband_move_ppm: u32,
    /// Linear coefficient that accelerates decay above the deadband.
    pub linear_coefficient_ppm: u32,
    /// Quadratic coefficient that accelerates decay above the deadband.
    pub quadratic_coefficient_ppm: u32,
    /// Haircut rate at or inside the deadband. Must remain below 100%.
    pub max_rate_ppm: u32,
    /// Fee charged after the haircut, in parts per million.
    pub win_fee_ppm: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MartingalerConfig {
    pub risk_pool_id: RiskPoolId,
    pub max_records: usize,
    pub max_harvest_entries: usize,
    pub claim_dust_threshold: u128,
    pub residual_bad_debt_cap: u128,
    pub queue_debt_open_gate: u128,
    pub max_reserved_profit_per_controller: u128,
    pub total_liability_open_gate: u128,
    pub reward_basis_cap: u128,
    pub fee_accrual_cap: u128,
    pub markets: Vec<MarketConfig>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FundingKind {
    Cash {
        protected_principal: u128,
    },
    Claim {
        contingent_collateral: u128,
        source_sequence: ClaimSequence,
    },
}

impl FundingKind {
    pub fn collateral(self) -> u128 {
        match self {
            Self::Cash {
                protected_principal,
            } => protected_principal,
            Self::Claim {
                contingent_collateral,
                ..
            } => contingent_collateral,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Position {
    pub id: PositionId,
    pub controller_id: ControllerId,
    pub market_id: MarketId,
    pub side: PositionSide,
    pub notional: u128,
    pub leverage_x: u16,
    pub max_net_profit: u128,
    pub reserved_trader_profit: u128,
    pub funding: FundingKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Claim {
    pub sequence: ClaimSequence,
    pub origin_position_id: PositionId,
    pub origin_market_id: MarketId,
    pub controller_id: ControllerId,
    pub remaining_face: u128,
    pub is_dust: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseKind {
    Loss(u128),
    Profit {
        raw_profit: u128,
        favorable_move_ppm: u32,
    },
    Flat,
    /// Liquidation forfeits the entire funding collateral. `gap_loss` is the
    /// total adverse PnL used to identify any amount beyond collateral.
    Liquidation {
        gap_loss: u128,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OracleCashPnl {
    Profit(u128),
    Flat,
    Loss(u128),
}

/// Classifies cash-position PnL from authenticated integer oracle prices.
/// Adverse magnitudes are conservatively rounded up so a fractional loss is
/// never omitted. Favorable magnitudes round down so settlement never creates
/// an unsupported profit atom.
pub fn oracle_cash_pnl(
    side: PositionSide,
    notional: u128,
    entry_price_e6: u64,
    exit_price_e6: u64,
) -> Result<OracleCashPnl, ModelError> {
    if notional == 0 || entry_price_e6 == 0 || exit_price_e6 == 0 {
        return Err(ModelError::InvalidConfiguration);
    }
    if entry_price_e6 == exit_price_e6 {
        return Ok(OracleCashPnl::Flat);
    }
    let favorable = match side {
        PositionSide::Long => exit_price_e6 > entry_price_e6,
        PositionSide::Short => exit_price_e6 < entry_price_e6,
    };
    let delta = u128::from(entry_price_e6.abs_diff(exit_price_e6));
    let denominator = crate::wide_math::U256::from_u128(u128::from(entry_price_e6));
    let magnitude = if favorable {
        crate::wide_math::U256::from_u128(notional)
            .checked_mul(crate::wide_math::U256::from_u128(delta))
            .ok_or(ModelError::ArithmeticOverflow)?
            / denominator
    } else {
        crate::wide_math::mul_div_ceil_u256(
            crate::wide_math::U256::from_u128(notional),
            crate::wide_math::U256::from_u128(delta),
            denominator,
        )
    }
    .try_into_u128()
    .ok_or(ModelError::ArithmeticOverflow)?;
    Ok(if favorable {
        OracleCashPnl::Profit(magnitude)
    } else {
        OracleCashPnl::Loss(magnitude)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfitAdjustment {
    pub raw_profit: u128,
    pub haircut_rate_ppm: u32,
    pub haircut_withheld: u128,
    pub post_haircut_profit: u128,
    pub win_fee: u128,
    /// Net profit after haircut and fee, before a position cap is applied.
    pub trader_profit: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenClaimOutcome {
    pub source_sequence: ClaimSequence,
    pub consumed: u128,
    pub remaining_at_old_priority: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CloseOutcome {
    pub position_id: PositionId,
    pub funding: FundingKind,
    pub cash_principal_returned: u128,
    pub payout_paid: u128,
    pub payout_queued: u128,
    pub new_claim_sequence: Option<ClaimSequence>,
    pub raw_profit: u128,
    pub haircut_withheld: u128,
    pub win_fee: u128,
    /// Net profit owed after haircut, fee, and the immutable position cap.
    pub trader_profit: u128,
    /// Post-fee profit above the immutable cap that was never made payable.
    pub profit_cap_withheld: u128,
    pub gross_loss: u128,
    pub cash_loss_collected: u128,
    pub claim_loss_extinguished: u128,
    pub residual_bad_debt_added: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaimPayment {
    pub sequence: ClaimSequence,
    pub controller_id: ControllerId,
    pub amount: u128,
    pub remaining_claim: u128,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarvestOutcome {
    pub entries_processed: usize,
    pub total_paid: u128,
    pub payments: Vec<ClaimPayment>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedMagnitude {
    pub is_negative: bool,
    pub magnitude: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvariantError {
    InvalidConfiguration,
    MarketMapMismatch,
    MarketModeMismatch,
    MarketOpenInterestMismatch,
    UnknownPositionMarket,
    DuplicateRecordId,
    EmptyPosition,
    PositionRiskLimitExceeded,
    IdleCashCollateralMismatch,
    ProtectedPrincipalMismatch,
    OpenClaimCollateralMismatch,
    ReservedProfitMismatch,
    ControllerReservedProfitCapExceeded,
    RecordCapacityExceeded,
    EmptyClaim,
    ClaimSequenceOrder,
    QueueAggregateMismatch,
    FreeCashAheadOfQueue,
    StrandedSideBucket,
    ResidualBadDebtCapExceeded,
    ResidualBadDebtSaturationMismatch,
    CashRewardSaturationMismatch,
    ClaimRewardSaturationMismatch,
    FeeAccrualSaturationMismatch,
    CustodyOverflow,
    ConservationOverflow,
    ConservationMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelError {
    InvalidConfiguration,
    DuplicatePosition,
    UnknownPosition,
    UnknownMarket,
    MarketNotActive,
    MarketNotClosable,
    InvalidModeTransition,
    MarketHasOpenPositions,
    TerminalStateUnavailable,
    ZeroPrincipal,
    ZeroNotional,
    ZeroAmount,
    InvalidController,
    InsufficientCashCollateral,
    CustodyCapacityExceeded,
    InvalidLeverage,
    PositionNotionalCapExceeded,
    LongOpenInterestExceeded,
    ShortOpenInterestExceeded,
    RecordCapacityExceeded,
    QueueDebtGateExceeded,
    ControllerReservedProfitCapExceeded,
    TotalLiabilityGateExceeded,
    UnknownClaim,
    ClaimOwnerMismatch,
    ClaimAmountExceeded,
    ResidualBadDebtSaturated,
    ArithmeticOverflow,
    InvariantViolation(InvariantError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Martingaler {
    config: MartingalerConfig,
    markets: BTreeMap<MarketId, MarketState>,
    positions: BTreeMap<PositionId, Position>,
    used_position_ids: BTreeSet<PositionId>,
    claims: VecDeque<Claim>,
    next_claim_sequence: ClaimSequence,
    controller_cash: BTreeMap<ControllerId, u128>,
    idle_cash_collateral: u128,
    unallocated_vault_surplus: u128,
    protected_principal: u128,
    open_claim_collateral: u128,
    reserved_profit_liability: u128,
    free_cash: u128,
    side_bucket: u128,
    total_queued: u128,
    residual_bad_debt: u128,
    residual_bad_debt_saturated: bool,
    cash_loss_reward_basis: u128,
    cash_loss_reward_basis_saturated: bool,
    claim_loss_reward_basis: u128,
    claim_loss_reward_basis_saturated: bool,
    fee_accrual: u128,
    fee_accrual_saturated: bool,
    cumulative_external_inflows: u128,
    cumulative_external_outflows: u128,
}

impl Martingaler {
    pub fn new(config: MartingalerConfig) -> Result<Self, ModelError> {
        Self::validate_config(&config).map_err(|_| ModelError::InvalidConfiguration)?;

        let mut markets = BTreeMap::new();
        for market in &config.markets {
            markets.insert(
                market.market_id,
                MarketState {
                    config: *market,
                    mode: market.initial_mode,
                    long_open_interest: 0,
                    short_open_interest: 0,
                },
            );
        }

        let model = Self {
            config,
            markets,
            positions: BTreeMap::new(),
            used_position_ids: BTreeSet::new(),
            claims: VecDeque::new(),
            next_claim_sequence: 1,
            controller_cash: BTreeMap::new(),
            idle_cash_collateral: 0,
            unallocated_vault_surplus: 0,
            protected_principal: 0,
            open_claim_collateral: 0,
            reserved_profit_liability: 0,
            free_cash: 0,
            side_bucket: 0,
            total_queued: 0,
            residual_bad_debt: 0,
            residual_bad_debt_saturated: false,
            cash_loss_reward_basis: 0,
            cash_loss_reward_basis_saturated: false,
            claim_loss_reward_basis: 0,
            claim_loss_reward_basis_saturated: false,
            fee_accrual: 0,
            fee_accrual_saturated: false,
            cumulative_external_inflows: 0,
            cumulative_external_outflows: 0,
        };
        model
            .check_invariants()
            .map_err(ModelError::InvariantViolation)?;
        Ok(model)
    }

    pub fn risk_pool_id(&self) -> RiskPoolId {
        self.config.risk_pool_id
    }

    pub fn market(&self, market_id: MarketId) -> Option<&MarketState> {
        self.markets.get(&market_id)
    }

    pub fn position(&self, position_id: PositionId) -> Option<&Position> {
        self.positions.get(&position_id)
    }

    pub fn claims(&self) -> &VecDeque<Claim> {
        &self.claims
    }

    pub fn claim(&self, sequence: ClaimSequence) -> Option<&Claim> {
        self.claims.iter().find(|claim| claim.sequence == sequence)
    }

    pub fn positions_len(&self) -> usize {
        self.positions.len()
    }

    pub fn record_count(&self) -> usize {
        self.positions.len() + self.claims.len()
    }

    pub fn protected_principal(&self) -> u128 {
        self.protected_principal
    }

    pub fn available_cash_for_controller(&self, controller_id: ControllerId) -> u128 {
        self.controller_cash
            .get(&controller_id)
            .copied()
            .unwrap_or(0)
    }

    pub fn idle_cash_collateral(&self) -> u128 {
        self.idle_cash_collateral
    }

    pub fn unallocated_vault_surplus(&self) -> u128 {
        self.unallocated_vault_surplus
    }

    pub fn open_claim_collateral(&self) -> u128 {
        self.open_claim_collateral
    }

    pub fn reserved_profit_liability(&self) -> u128 {
        self.reserved_profit_liability
    }

    pub fn reserved_profit_for_controller(
        &self,
        controller_id: ControllerId,
    ) -> Result<u128, ModelError> {
        self.positions
            .values()
            .filter(|position| position.controller_id == controller_id)
            .try_fold(0u128, |total, position| {
                total
                    .checked_add(position.reserved_trader_profit)
                    .ok_or(ModelError::ArithmeticOverflow)
            })
    }

    pub fn free_cash(&self) -> u128 {
        self.free_cash
    }

    pub fn side_bucket(&self) -> u128 {
        self.side_bucket
    }

    pub fn total_queued(&self) -> u128 {
        self.total_queued
    }

    pub fn residual_bad_debt(&self) -> u128 {
        self.residual_bad_debt
    }

    pub fn residual_bad_debt_saturated(&self) -> bool {
        self.residual_bad_debt_saturated
    }

    pub fn cash_loss_reward_basis(&self) -> u128 {
        self.cash_loss_reward_basis
    }

    pub fn cash_loss_reward_basis_saturated(&self) -> bool {
        self.cash_loss_reward_basis_saturated
    }

    pub fn claim_loss_reward_basis(&self) -> u128 {
        self.claim_loss_reward_basis
    }

    pub fn claim_loss_reward_basis_saturated(&self) -> bool {
        self.claim_loss_reward_basis_saturated
    }

    pub fn fee_accrual(&self) -> u128 {
        self.fee_accrual
    }

    pub fn fee_accrual_saturated(&self) -> bool {
        self.fee_accrual_saturated
    }

    pub fn cumulative_external_inflows(&self) -> u128 {
        self.cumulative_external_inflows
    }

    pub fn cumulative_external_outflows(&self) -> u128 {
        self.cumulative_external_outflows
    }

    pub fn deposit_cash_collateral(
        &mut self,
        controller_id: ControllerId,
        amount: u64,
    ) -> Result<(), ModelError> {
        if controller_id == 0 {
            return Err(ModelError::InvalidController);
        }
        if amount == 0 {
            return Err(ModelError::ZeroAmount);
        }
        let amount = u128::from(amount);
        self.transact(|next| {
            let projected_custody = next
                .quote_custody()?
                .checked_add(amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            if projected_custody > u128::from(u64::MAX) {
                return Err(ModelError::CustodyCapacityExceeded);
            }
            let controller_cash = next
                .controller_cash
                .get(&controller_id)
                .copied()
                .unwrap_or(0)
                .checked_add(amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.idle_cash_collateral = next
                .idle_cash_collateral
                .checked_add(amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.cumulative_external_inflows = next
                .cumulative_external_inflows
                .checked_add(amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.controller_cash.insert(controller_id, controller_cash);
            Ok(())
        })
    }

    pub fn record_unallocated_vault_surplus(&mut self, amount: u64) -> Result<(), ModelError> {
        if amount == 0 {
            return Err(ModelError::ZeroAmount);
        }
        let amount = u128::from(amount);
        self.transact(|next| {
            let projected_custody = next
                .quote_custody()?
                .checked_add(amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            if projected_custody > u128::from(u64::MAX) {
                return Err(ModelError::CustodyCapacityExceeded);
            }
            next.unallocated_vault_surplus = next
                .unallocated_vault_surplus
                .checked_add(amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.cumulative_external_inflows = next
                .cumulative_external_inflows
                .checked_add(amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            Ok(())
        })
    }

    /// Opens a cash-funded position by reclassifying collateral that was
    /// already deposited for this controller. Unlike `open_cash_position`,
    /// this executable-path model does not create a second external inflow.
    #[allow(clippy::too_many_arguments)]
    pub fn open_deposited_cash_position(
        &mut self,
        position_id: PositionId,
        controller_id: ControllerId,
        market_id: MarketId,
        side: PositionSide,
        protected_principal: u128,
        notional: u128,
        leverage_x: u16,
    ) -> Result<(), ModelError> {
        if controller_id == 0 {
            return Err(ModelError::InvalidController);
        }
        self.transact(|next| {
            let reserved_trader_profit = next.validate_open(
                position_id,
                controller_id,
                market_id,
                side,
                protected_principal,
                notional,
                leverage_x,
            )?;
            if next.record_count() >= next.config.max_records {
                return Err(ModelError::RecordCapacityExceeded);
            }

            let available = next
                .controller_cash
                .get(&controller_id)
                .copied()
                .unwrap_or(0);
            let remaining = available
                .checked_sub(protected_principal)
                .ok_or(ModelError::InsufficientCashCollateral)?;
            let custody_before = next.quote_custody()?;

            if remaining == 0 {
                next.controller_cash.remove(&controller_id);
            } else {
                next.controller_cash.insert(controller_id, remaining);
            }
            next.idle_cash_collateral = next
                .idle_cash_collateral
                .checked_sub(protected_principal)
                .ok_or(ModelError::InsufficientCashCollateral)?;
            next.protected_principal = next
                .protected_principal
                .checked_add(protected_principal)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.reserved_profit_liability = next
                .reserved_profit_liability
                .checked_add(reserved_trader_profit)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.add_open_interest(market_id, side, notional)?;
            next.positions.insert(
                position_id,
                Position {
                    id: position_id,
                    controller_id,
                    market_id,
                    side,
                    notional,
                    leverage_x,
                    max_net_profit: reserved_trader_profit,
                    reserved_trader_profit,
                    funding: FundingKind::Cash {
                        protected_principal,
                    },
                },
            );
            next.used_position_ids.insert(position_id);
            if next.quote_custody()? != custody_before {
                return Err(ModelError::InvariantViolation(
                    InvariantError::ConservationMismatch,
                ));
            }
            Ok(())
        })
    }

    pub fn quote_custody(&self) -> Result<u128, ModelError> {
        self.idle_cash_collateral
            .checked_add(self.unallocated_vault_surplus)
            .and_then(|value| value.checked_add(self.protected_principal))
            .and_then(|value| value.checked_add(self.free_cash))
            .and_then(|value| value.checked_add(self.side_bucket))
            .ok_or(ModelError::ArithmeticOverflow)
    }

    pub fn total_liabilities(&self) -> Result<u128, ModelError> {
        if self.residual_bad_debt_saturated {
            return Err(ModelError::ResidualBadDebtSaturated);
        }
        self.total_queued
            .checked_add(self.open_claim_collateral)
            .and_then(|value| value.checked_add(self.residual_bad_debt))
            .ok_or(ModelError::ArithmeticOverflow)
    }

    pub fn total_potential_liabilities(&self) -> Result<u128, ModelError> {
        self.total_liabilities()?
            .checked_add(self.reserved_profit_liability)
            .ok_or(ModelError::ArithmeticOverflow)
    }

    pub fn lp_equity(&self) -> Result<SignedMagnitude, ModelError> {
        let assets = self
            .free_cash
            .checked_add(self.side_bucket)
            .ok_or(ModelError::ArithmeticOverflow)?;
        let liabilities = self.total_liabilities()?;
        Ok(match assets.cmp(&liabilities) {
            Ordering::Less => SignedMagnitude {
                is_negative: true,
                magnitude: liabilities - assets,
            },
            Ordering::Equal => SignedMagnitude {
                is_negative: false,
                magnitude: 0,
            },
            Ordering::Greater => SignedMagnitude {
                is_negative: false,
                magnitude: assets - liabilities,
            },
        })
    }

    pub fn adjust_profit(
        &self,
        market_id: MarketId,
        raw_profit: u128,
        favorable_move_ppm: u32,
    ) -> Result<ProfitAdjustment, ModelError> {
        if raw_profit == 0 {
            return Err(ModelError::ZeroAmount);
        }
        self.adjust_profit_inner(market_id, raw_profit, favorable_move_ppm)
    }

    fn adjust_profit_inner(
        &self,
        market_id: MarketId,
        raw_profit: u128,
        favorable_move_ppm: u32,
    ) -> Result<ProfitAdjustment, ModelError> {
        let haircut = self
            .markets
            .get(&market_id)
            .ok_or(ModelError::UnknownMarket)?
            .config
            .haircut;
        let haircut_rate_ppm = Self::haircut_rate(haircut, favorable_move_ppm)?;
        let retained_rate = RATE_SCALE_PPM
            .checked_sub(u128::from(haircut_rate_ppm))
            .ok_or(ModelError::ArithmeticOverflow)?;
        let post_haircut_profit = Self::mul_div_floor(raw_profit, retained_rate, RATE_SCALE_PPM)?;
        let trader_profit = Self::mul_div_floor(
            post_haircut_profit,
            RATE_SCALE_PPM - u128::from(haircut.win_fee_ppm),
            RATE_SCALE_PPM,
        )?;
        let haircut_withheld = raw_profit
            .checked_sub(post_haircut_profit)
            .ok_or(ModelError::ArithmeticOverflow)?;
        let win_fee = post_haircut_profit
            .checked_sub(trader_profit)
            .ok_or(ModelError::ArithmeticOverflow)?;
        Ok(ProfitAdjustment {
            raw_profit,
            haircut_rate_ppm,
            haircut_withheld,
            post_haircut_profit,
            win_fee,
            trader_profit,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_cash_position(
        &mut self,
        position_id: PositionId,
        controller_id: ControllerId,
        market_id: MarketId,
        side: PositionSide,
        protected_principal: u128,
        notional: u128,
        leverage_x: u16,
    ) -> Result<(), ModelError> {
        self.transact(|next| {
            let reserved_trader_profit = next.validate_open(
                position_id,
                controller_id,
                market_id,
                side,
                protected_principal,
                notional,
                leverage_x,
            )?;
            if next.record_count() >= next.config.max_records {
                return Err(ModelError::RecordCapacityExceeded);
            }

            next.protected_principal = next
                .protected_principal
                .checked_add(protected_principal)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.cumulative_external_inflows = next
                .cumulative_external_inflows
                .checked_add(protected_principal)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.reserved_profit_liability = next
                .reserved_profit_liability
                .checked_add(reserved_trader_profit)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.add_open_interest(market_id, side, notional)?;
            next.positions.insert(
                position_id,
                Position {
                    id: position_id,
                    controller_id,
                    market_id,
                    side,
                    notional,
                    leverage_x,
                    max_net_profit: reserved_trader_profit,
                    reserved_trader_profit,
                    funding: FundingKind::Cash {
                        protected_principal,
                    },
                },
            );
            next.used_position_ids.insert(position_id);
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_claim_position(
        &mut self,
        position_id: PositionId,
        controller_id: ControllerId,
        market_id: MarketId,
        side: PositionSide,
        source_sequence: ClaimSequence,
        claim_amount: u128,
        notional: u128,
        leverage_x: u16,
    ) -> Result<OpenClaimOutcome, ModelError> {
        self.transact(|next| {
            let reserved_trader_profit = next.validate_open(
                position_id,
                controller_id,
                market_id,
                side,
                claim_amount,
                notional,
                leverage_x,
            )?;
            let claim_index = next
                .claims
                .iter()
                .position(|claim| claim.sequence == source_sequence)
                .ok_or(ModelError::UnknownClaim)?;
            let source = next.claims[claim_index];
            if source.controller_id != controller_id {
                return Err(ModelError::ClaimOwnerMismatch);
            }
            if claim_amount > source.remaining_face {
                return Err(ModelError::ClaimAmountExceeded);
            }
            let consumes_whole_record = claim_amount == source.remaining_face;
            if !consumes_whole_record && next.record_count() >= next.config.max_records {
                return Err(ModelError::RecordCapacityExceeded);
            }

            let remaining = source
                .remaining_face
                .checked_sub(claim_amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.total_queued = next
                .total_queued
                .checked_sub(claim_amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            if consumes_whole_record {
                next.claims.remove(claim_index);
            } else {
                let claim = next
                    .claims
                    .get_mut(claim_index)
                    .ok_or(ModelError::UnknownClaim)?;
                claim.remaining_face = remaining;
                claim.is_dust = remaining < next.config.claim_dust_threshold;
            }
            if next.claims.is_empty() {
                next.release_side_bucket()?;
            }

            next.open_claim_collateral = next
                .open_claim_collateral
                .checked_add(claim_amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.reserved_profit_liability = next
                .reserved_profit_liability
                .checked_add(reserved_trader_profit)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.add_open_interest(market_id, side, notional)?;
            next.positions.insert(
                position_id,
                Position {
                    id: position_id,
                    controller_id,
                    market_id,
                    side,
                    notional,
                    leverage_x,
                    max_net_profit: reserved_trader_profit,
                    reserved_trader_profit,
                    funding: FundingKind::Claim {
                        contingent_collateral: claim_amount,
                        source_sequence,
                    },
                },
            );
            next.used_position_ids.insert(position_id);
            Ok(OpenClaimOutcome {
                source_sequence,
                consumed: claim_amount,
                remaining_at_old_priority: remaining,
            })
        })
    }

    pub fn close_position(
        &mut self,
        position_id: PositionId,
        close: CloseKind,
    ) -> Result<CloseOutcome, ModelError> {
        self.transact(|next| {
            let position = *next
                .positions
                .get(&position_id)
                .ok_or(ModelError::UnknownPosition)?;
            let market = next
                .markets
                .get(&position.market_id)
                .ok_or(ModelError::UnknownMarket)?;
            if !market.mode.permits_close() {
                return Err(ModelError::MarketNotClosable);
            }
            match close {
                CloseKind::Loss(0) | CloseKind::Liquidation { gap_loss: 0 } => {
                    return Err(ModelError::ZeroAmount);
                }
                CloseKind::Loss(_)
                | CloseKind::Profit { .. }
                | CloseKind::Flat
                | CloseKind::Liquidation { .. } => {}
            }

            let adjustment = match close {
                CloseKind::Profit {
                    raw_profit,
                    favorable_move_ppm,
                } => Some(next.adjust_profit_inner(
                    position.market_id,
                    raw_profit,
                    favorable_move_ppm,
                )?),
                CloseKind::Loss(_) | CloseKind::Flat | CloseKind::Liquidation { .. } => None,
            };
            let capped_trader_profit = adjustment
                .as_ref()
                .map(|value| core::cmp::min(value.trader_profit, position.max_net_profit));

            next.positions.remove(&position_id);
            next.remove_open_interest(position.market_id, position.side, position.notional)?;
            next.reserved_profit_liability = next
                .reserved_profit_liability
                .checked_sub(position.reserved_trader_profit)
                .ok_or(ModelError::ArithmeticOverflow)?;

            let mut outcome = CloseOutcome {
                position_id,
                funding: position.funding,
                cash_principal_returned: 0,
                payout_paid: 0,
                payout_queued: 0,
                new_claim_sequence: None,
                raw_profit: 0,
                haircut_withheld: 0,
                win_fee: 0,
                trader_profit: 0,
                profit_cap_withheld: 0,
                gross_loss: 0,
                cash_loss_collected: 0,
                claim_loss_extinguished: 0,
                residual_bad_debt_added: 0,
            };

            match position.funding {
                FundingKind::Cash {
                    protected_principal,
                } => {
                    next.protected_principal = next
                        .protected_principal
                        .checked_sub(protected_principal)
                        .ok_or(ModelError::ArithmeticOverflow)?;
                    match close {
                        CloseKind::Flat => {
                            outcome.cash_principal_returned = protected_principal;
                        }
                        CloseKind::Profit { .. } => {
                            let adjusted = adjustment.ok_or(ModelError::ArithmeticOverflow)?;
                            let trader_profit =
                                capped_trader_profit.ok_or(ModelError::ArithmeticOverflow)?;
                            next.record_adjustment(&adjusted);
                            outcome.cash_principal_returned = protected_principal;
                            outcome.raw_profit = adjusted.raw_profit;
                            outcome.haircut_withheld = adjusted.haircut_withheld;
                            outcome.win_fee = adjusted.win_fee;
                            outcome.trader_profit = trader_profit;
                            outcome.profit_cap_withheld = adjusted
                                .trader_profit
                                .checked_sub(trader_profit)
                                .ok_or(ModelError::ArithmeticOverflow)?;
                            let settlement = next.settle_payable(
                                position.id,
                                position.market_id,
                                position.controller_id,
                                trader_profit,
                            )?;
                            outcome.payout_paid = settlement.0;
                            outcome.payout_queued = settlement.1;
                            outcome.new_claim_sequence = settlement.2;
                        }
                        CloseKind::Loss(loss) => {
                            let collected = core::cmp::min(loss, protected_principal);
                            let residual = loss - collected;
                            outcome.gross_loss = loss;
                            outcome.cash_loss_collected = collected;
                            outcome.cash_principal_returned = protected_principal - collected;
                            next.route_cash_loss(collected)?;
                            next.record_cash_reward(collected);
                            next.record_residual(residual);
                            outcome.residual_bad_debt_added = residual;
                        }
                        CloseKind::Liquidation { gap_loss } => {
                            let residual = gap_loss.saturating_sub(protected_principal);
                            outcome.gross_loss = core::cmp::max(gap_loss, protected_principal);
                            outcome.cash_loss_collected = protected_principal;
                            next.route_cash_loss(protected_principal)?;
                            next.record_cash_reward(protected_principal);
                            next.record_residual(residual);
                            outcome.residual_bad_debt_added = residual;
                        }
                    }
                }
                FundingKind::Claim {
                    contingent_collateral,
                    ..
                } => {
                    next.open_claim_collateral = next
                        .open_claim_collateral
                        .checked_sub(contingent_collateral)
                        .ok_or(ModelError::ArithmeticOverflow)?;
                    let payable = match close {
                        CloseKind::Flat => contingent_collateral,
                        CloseKind::Profit { .. } => {
                            let adjusted = adjustment.ok_or(ModelError::ArithmeticOverflow)?;
                            let trader_profit =
                                capped_trader_profit.ok_or(ModelError::ArithmeticOverflow)?;
                            next.record_adjustment(&adjusted);
                            outcome.raw_profit = adjusted.raw_profit;
                            outcome.haircut_withheld = adjusted.haircut_withheld;
                            outcome.win_fee = adjusted.win_fee;
                            outcome.trader_profit = trader_profit;
                            outcome.profit_cap_withheld = adjusted
                                .trader_profit
                                .checked_sub(trader_profit)
                                .ok_or(ModelError::ArithmeticOverflow)?;
                            contingent_collateral
                                .checked_add(trader_profit)
                                .ok_or(ModelError::ArithmeticOverflow)?
                        }
                        CloseKind::Loss(loss) => {
                            let extinguished = core::cmp::min(loss, contingent_collateral);
                            let residual = loss - extinguished;
                            outcome.gross_loss = loss;
                            outcome.claim_loss_extinguished = extinguished;
                            outcome.residual_bad_debt_added = residual;
                            next.record_claim_reward(extinguished);
                            next.record_residual(residual);
                            contingent_collateral - extinguished
                        }
                        CloseKind::Liquidation { gap_loss } => {
                            let residual = gap_loss.saturating_sub(contingent_collateral);
                            outcome.gross_loss = core::cmp::max(gap_loss, contingent_collateral);
                            outcome.claim_loss_extinguished = contingent_collateral;
                            outcome.residual_bad_debt_added = residual;
                            next.record_claim_reward(contingent_collateral);
                            next.record_residual(residual);
                            0
                        }
                    };
                    let settlement = next.settle_payable(
                        position.id,
                        position.market_id,
                        position.controller_id,
                        payable,
                    )?;
                    outcome.payout_paid = settlement.0;
                    outcome.payout_queued = settlement.1;
                    outcome.new_claim_sequence = settlement.2;
                }
            }

            let external_outflow = outcome
                .cash_principal_returned
                .checked_add(outcome.payout_paid)
                .ok_or(ModelError::ArithmeticOverflow)?;
            next.cumulative_external_outflows = next
                .cumulative_external_outflows
                .checked_add(external_outflow)
                .ok_or(ModelError::ArithmeticOverflow)?;
            Ok(outcome)
        })
    }

    pub fn harvest(&mut self, max_entries: usize) -> Result<HarvestOutcome, ModelError> {
        self.transact(|next| {
            let entry_limit = core::cmp::min(max_entries, next.config.max_harvest_entries);
            let mut outcome = HarvestOutcome {
                entries_processed: 0,
                total_paid: 0,
                payments: Vec::new(),
            };

            while outcome.entries_processed < entry_limit && next.side_bucket != 0 {
                let Some(head) = next.claims.front_mut() else {
                    break;
                };
                let amount = core::cmp::min(head.remaining_face, next.side_bucket);
                head.remaining_face = head
                    .remaining_face
                    .checked_sub(amount)
                    .ok_or(ModelError::ArithmeticOverflow)?;
                head.is_dust = head.remaining_face != 0
                    && head.remaining_face < next.config.claim_dust_threshold;
                next.side_bucket = next
                    .side_bucket
                    .checked_sub(amount)
                    .ok_or(ModelError::ArithmeticOverflow)?;
                next.total_queued = next
                    .total_queued
                    .checked_sub(amount)
                    .ok_or(ModelError::ArithmeticOverflow)?;
                outcome.total_paid = outcome
                    .total_paid
                    .checked_add(amount)
                    .ok_or(ModelError::ArithmeticOverflow)?;
                outcome.entries_processed += 1;
                outcome.payments.push(ClaimPayment {
                    sequence: head.sequence,
                    controller_id: head.controller_id,
                    amount,
                    remaining_claim: head.remaining_face,
                });
                if head.remaining_face == 0 {
                    next.claims.pop_front();
                } else {
                    break;
                }
            }

            next.cumulative_external_outflows = next
                .cumulative_external_outflows
                .checked_add(outcome.total_paid)
                .ok_or(ModelError::ArithmeticOverflow)?;
            if next.claims.is_empty() {
                next.release_side_bucket()?;
            }
            Ok(outcome)
        })
    }

    pub fn trigger_divergence(&mut self, market_id: MarketId) -> Result<(), ModelError> {
        self.transact(|next| {
            let market = next
                .markets
                .get_mut(&market_id)
                .ok_or(ModelError::UnknownMarket)?;
            if market.mode != MarketMode::Active {
                return Err(ModelError::InvalidModeTransition);
            }
            market.mode = MarketMode::CloseOnly;
            Ok(())
        })
    }

    pub fn advance_market_mode(
        &mut self,
        market_id: MarketId,
        new_mode: MarketMode,
    ) -> Result<(), ModelError> {
        self.transact(|next| {
            let current = next
                .markets
                .get(&market_id)
                .ok_or(ModelError::UnknownMarket)?
                .mode;
            if new_mode.rank() != current.rank() + 1 {
                return Err(ModelError::InvalidModeTransition);
            }
            if matches!(new_mode, MarketMode::ClaimsOnly | MarketMode::Terminal)
                && next
                    .positions
                    .values()
                    .any(|position| position.market_id == market_id)
            {
                return Err(ModelError::MarketHasOpenPositions);
            }
            if new_mode == MarketMode::Terminal
                && (!next.positions.is_empty()
                    || !next.claims.is_empty()
                    || next.idle_cash_collateral != 0
                    || next.unallocated_vault_surplus != 0
                    || next.protected_principal != 0
                    || next.open_claim_collateral != 0
                    || next.reserved_profit_liability != 0
                    || next.side_bucket != 0
                    || next.residual_bad_debt != 0
                    || next.residual_bad_debt_saturated
                    || next.markets.iter().any(|(other_id, market)| {
                        *other_id != market_id
                            && !matches!(market.mode, MarketMode::ClaimsOnly | MarketMode::Terminal)
                    }))
            {
                return Err(ModelError::TerminalStateUnavailable);
            }
            next.markets
                .get_mut(&market_id)
                .ok_or(ModelError::UnknownMarket)?
                .mode = new_mode;
            Ok(())
        })
    }

    pub fn check_invariants(&self) -> Result<(), InvariantError> {
        Self::validate_config(&self.config)?;
        if self.markets.len() != self.config.markets.len() {
            return Err(InvariantError::MarketMapMismatch);
        }
        for config in &self.config.markets {
            let state = self
                .markets
                .get(&config.market_id)
                .ok_or(InvariantError::MarketMapMismatch)?;
            if state.config != *config {
                return Err(InvariantError::MarketMapMismatch);
            }
        }

        let mut protected_principal = 0u128;
        let mut open_claim_collateral = 0u128;
        let mut reserved_profit_liability = 0u128;
        let mut controller_reserved_profit = BTreeMap::<ControllerId, u128>::new();
        let mut long_oi = BTreeMap::<MarketId, u128>::new();
        let mut short_oi = BTreeMap::<MarketId, u128>::new();
        for (position_id, position) in &self.positions {
            if position.id == 0
                || position.id != *position_id
                || position.notional == 0
                || position.leverage_x == 0
                || position.max_net_profit == 0
                || position.funding.collateral() == 0
            {
                return Err(InvariantError::EmptyPosition);
            }
            if !self.used_position_ids.contains(&position.id) {
                return Err(InvariantError::DuplicateRecordId);
            }
            let market = self
                .markets
                .get(&position.market_id)
                .ok_or(InvariantError::UnknownPositionMarket)?;
            let collateral = position.funding.collateral();
            let minimum_leverage =
                position.notional / collateral + u128::from(position.notional % collateral != 0);
            if position.leverage_x > market.config.max_leverage_x
                || minimum_leverage > u128::from(position.leverage_x)
                || position.notional > market.config.max_position_notional
                || position.max_net_profit > market.config.max_net_profit_per_position
            {
                return Err(InvariantError::PositionRiskLimitExceeded);
            }
            let expected_reserve =
                core::cmp::min(position.notional, market.config.max_net_profit_per_position);
            if expected_reserve != position.max_net_profit
                || expected_reserve != position.reserved_trader_profit
            {
                return Err(InvariantError::ReservedProfitMismatch);
            }
            reserved_profit_liability = reserved_profit_liability
                .checked_add(position.reserved_trader_profit)
                .ok_or(InvariantError::ReservedProfitMismatch)?;
            let controller_total = controller_reserved_profit
                .get(&position.controller_id)
                .copied()
                .unwrap_or(0)
                .checked_add(position.reserved_trader_profit)
                .ok_or(InvariantError::ControllerReservedProfitCapExceeded)?;
            if controller_total > self.config.max_reserved_profit_per_controller {
                return Err(InvariantError::ControllerReservedProfitCapExceeded);
            }
            controller_reserved_profit.insert(position.controller_id, controller_total);
            match position.funding {
                FundingKind::Cash {
                    protected_principal: principal,
                } => {
                    protected_principal = protected_principal
                        .checked_add(principal)
                        .ok_or(InvariantError::CustodyOverflow)?;
                }
                FundingKind::Claim {
                    contingent_collateral,
                    ..
                } => {
                    open_claim_collateral = open_claim_collateral
                        .checked_add(contingent_collateral)
                        .ok_or(InvariantError::QueueAggregateMismatch)?;
                }
            }
            let map = match position.side {
                PositionSide::Long => &mut long_oi,
                PositionSide::Short => &mut short_oi,
            };
            let current = map.get(&position.market_id).copied().unwrap_or(0);
            map.insert(
                position.market_id,
                current
                    .checked_add(position.notional)
                    .ok_or(InvariantError::MarketOpenInterestMismatch)?,
            );
        }
        if protected_principal != self.protected_principal {
            return Err(InvariantError::ProtectedPrincipalMismatch);
        }
        if open_claim_collateral != self.open_claim_collateral {
            return Err(InvariantError::OpenClaimCollateralMismatch);
        }
        if reserved_profit_liability != self.reserved_profit_liability {
            return Err(InvariantError::ReservedProfitMismatch);
        }

        let controller_cash =
            self.controller_cash
                .iter()
                .try_fold(0u128, |total, (controller_id, amount)| {
                    if *controller_id == 0 || *amount == 0 {
                        return Err(InvariantError::IdleCashCollateralMismatch);
                    }
                    total
                        .checked_add(*amount)
                        .ok_or(InvariantError::CustodyOverflow)
                })?;
        if controller_cash != self.idle_cash_collateral {
            return Err(InvariantError::IdleCashCollateralMismatch);
        }

        for (market_id, market) in &self.markets {
            if market.long_open_interest != long_oi.get(market_id).copied().unwrap_or(0)
                || market.short_open_interest != short_oi.get(market_id).copied().unwrap_or(0)
            {
                return Err(InvariantError::MarketOpenInterestMismatch);
            }
            if market.long_open_interest > market.config.max_long_open_interest
                || market.short_open_interest > market.config.max_short_open_interest
            {
                return Err(InvariantError::PositionRiskLimitExceeded);
            }
            if matches!(market.mode, MarketMode::ClaimsOnly | MarketMode::Terminal)
                && (market.long_open_interest != 0 || market.short_open_interest != 0)
            {
                return Err(InvariantError::MarketModeMismatch);
            }
            if market.mode == MarketMode::Terminal
                && (!self.positions.is_empty()
                    || !self.claims.is_empty()
                    || self.idle_cash_collateral != 0
                    || self.unallocated_vault_surplus != 0
                    || self.protected_principal != 0
                    || self.open_claim_collateral != 0
                    || self.reserved_profit_liability != 0
                    || self.side_bucket != 0
                    || self.residual_bad_debt != 0
                    || self.residual_bad_debt_saturated
                    || self.markets.values().any(|other| {
                        !matches!(other.mode, MarketMode::ClaimsOnly | MarketMode::Terminal)
                    }))
            {
                return Err(InvariantError::MarketModeMismatch);
            }
        }

        let mut queued = 0u128;
        let mut previous_sequence = 0u128;
        for claim in &self.claims {
            if claim.remaining_face == 0 {
                return Err(InvariantError::EmptyClaim);
            }
            if !self.markets.contains_key(&claim.origin_market_id) {
                return Err(InvariantError::UnknownPositionMarket);
            }
            if claim.sequence <= previous_sequence || claim.sequence >= self.next_claim_sequence {
                return Err(InvariantError::ClaimSequenceOrder);
            }
            if claim.is_dust != (claim.remaining_face < self.config.claim_dust_threshold) {
                return Err(InvariantError::EmptyClaim);
            }
            previous_sequence = claim.sequence;
            queued = queued
                .checked_add(claim.remaining_face)
                .ok_or(InvariantError::QueueAggregateMismatch)?;
        }
        if queued != self.total_queued {
            return Err(InvariantError::QueueAggregateMismatch);
        }
        if self.record_count() > self.config.max_records {
            return Err(InvariantError::RecordCapacityExceeded);
        }
        if !self.claims.is_empty() && self.free_cash != 0 {
            return Err(InvariantError::FreeCashAheadOfQueue);
        }
        if self.claims.is_empty() && self.side_bucket != 0 {
            return Err(InvariantError::StrandedSideBucket);
        }

        Self::check_saturated_counter(
            self.residual_bad_debt,
            self.residual_bad_debt_saturated,
            self.config.residual_bad_debt_cap,
            InvariantError::ResidualBadDebtCapExceeded,
            InvariantError::ResidualBadDebtSaturationMismatch,
        )?;
        Self::check_saturated_counter(
            self.cash_loss_reward_basis,
            self.cash_loss_reward_basis_saturated,
            self.config.reward_basis_cap,
            InvariantError::CashRewardSaturationMismatch,
            InvariantError::CashRewardSaturationMismatch,
        )?;
        Self::check_saturated_counter(
            self.claim_loss_reward_basis,
            self.claim_loss_reward_basis_saturated,
            self.config.reward_basis_cap,
            InvariantError::ClaimRewardSaturationMismatch,
            InvariantError::ClaimRewardSaturationMismatch,
        )?;
        Self::check_saturated_counter(
            self.fee_accrual,
            self.fee_accrual_saturated,
            self.config.fee_accrual_cap,
            InvariantError::FeeAccrualSaturationMismatch,
            InvariantError::FeeAccrualSaturationMismatch,
        )?;

        let custody = self
            .idle_cash_collateral
            .checked_add(self.unallocated_vault_surplus)
            .and_then(|value| value.checked_add(self.protected_principal))
            .and_then(|value| value.checked_add(self.free_cash))
            .and_then(|value| value.checked_add(self.side_bucket))
            .ok_or(InvariantError::CustodyOverflow)?;
        let conserved = custody
            .checked_add(self.cumulative_external_outflows)
            .ok_or(InvariantError::ConservationOverflow)?;
        if conserved != self.cumulative_external_inflows {
            return Err(InvariantError::ConservationMismatch);
        }
        Ok(())
    }

    fn validate_config(config: &MartingalerConfig) -> Result<(), InvariantError> {
        if config.max_records == 0
            || config.max_harvest_entries == 0
            || config.claim_dust_threshold == 0
            || config.residual_bad_debt_cap == 0
            || config.reward_basis_cap == 0
            || config.fee_accrual_cap == 0
            || config.max_reserved_profit_per_controller == 0
            || config.max_reserved_profit_per_controller
                > V1_MAX_RESERVED_PROFIT_PER_CONTROLLER_QUOTE
            || config.total_liability_open_gate == 0
            || config.total_liability_open_gate > V1_TOTAL_POTENTIAL_LIABILITY_QUOTE
            || config.queue_debt_open_gate > config.total_liability_open_gate
            || config.markets.is_empty()
        {
            return Err(InvariantError::InvalidConfiguration);
        }
        let mut ids = BTreeSet::new();
        for market in &config.markets {
            if !ids.insert(market.market_id)
                || market.max_leverage_x == 0
                || market.max_leverage_x > V1_MAX_LEVERAGE_X
                || market.max_position_notional == 0
                || market.max_position_notional > V1_MAX_POSITION_NOTIONAL_QUOTE
                || market.max_net_profit_per_position == 0
                || market.max_net_profit_per_position > V1_MAX_NET_PROFIT_PER_POSITION_QUOTE
                || market.max_net_profit_per_position > market.max_position_notional
                || market.max_long_open_interest == 0
                || market.max_short_open_interest == 0
                || market.haircut.base_rate_ppm > market.haircut.max_rate_ppm
                || u128::from(market.haircut.max_rate_ppm) >= RATE_SCALE_PPM
                || u128::from(market.haircut.win_fee_ppm) > RATE_SCALE_PPM
            {
                return Err(InvariantError::InvalidConfiguration);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_open(
        &self,
        position_id: PositionId,
        controller_id: ControllerId,
        market_id: MarketId,
        side: PositionSide,
        collateral: u128,
        notional: u128,
        leverage_x: u16,
    ) -> Result<u128, ModelError> {
        if position_id == 0 || self.used_position_ids.contains(&position_id) {
            return Err(ModelError::DuplicatePosition);
        }
        if collateral == 0 {
            return Err(ModelError::ZeroPrincipal);
        }
        if notional == 0 {
            return Err(ModelError::ZeroNotional);
        }
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ModelError::UnknownMarket)?;
        if market.mode != MarketMode::Active {
            return Err(ModelError::MarketNotActive);
        }
        if leverage_x == 0 || leverage_x > market.config.max_leverage_x {
            return Err(ModelError::InvalidLeverage);
        }
        let minimum_leverage = notional / collateral + u128::from(notional % collateral != 0);
        if minimum_leverage > u128::from(leverage_x) {
            return Err(ModelError::InvalidLeverage);
        }
        if notional > market.config.max_position_notional {
            return Err(ModelError::PositionNotionalCapExceeded);
        }
        let next_oi = match side {
            PositionSide::Long => market.long_open_interest.checked_add(notional),
            PositionSide::Short => market.short_open_interest.checked_add(notional),
        }
        .ok_or(ModelError::ArithmeticOverflow)?;
        match side {
            PositionSide::Long if next_oi > market.config.max_long_open_interest => {
                return Err(ModelError::LongOpenInterestExceeded);
            }
            PositionSide::Short if next_oi > market.config.max_short_open_interest => {
                return Err(ModelError::ShortOpenInterestExceeded);
            }
            PositionSide::Long | PositionSide::Short => {}
        }
        if self.total_queued > self.config.queue_debt_open_gate {
            return Err(ModelError::QueueDebtGateExceeded);
        }
        let reserved_trader_profit =
            core::cmp::min(notional, market.config.max_net_profit_per_position);
        let controller_reserved_profit = self
            .reserved_profit_for_controller(controller_id)?
            .checked_add(reserved_trader_profit)
            .ok_or(ModelError::ArithmeticOverflow)?;
        if controller_reserved_profit > self.config.max_reserved_profit_per_controller {
            return Err(ModelError::ControllerReservedProfitCapExceeded);
        }
        let potential_liabilities = self
            .total_potential_liabilities()?
            .checked_add(reserved_trader_profit)
            .ok_or(ModelError::ArithmeticOverflow)?;
        if potential_liabilities > self.config.total_liability_open_gate {
            return Err(ModelError::TotalLiabilityGateExceeded);
        }
        Ok(reserved_trader_profit)
    }

    fn add_open_interest(
        &mut self,
        market_id: MarketId,
        side: PositionSide,
        notional: u128,
    ) -> Result<(), ModelError> {
        let market = self
            .markets
            .get_mut(&market_id)
            .ok_or(ModelError::UnknownMarket)?;
        let target = match side {
            PositionSide::Long => &mut market.long_open_interest,
            PositionSide::Short => &mut market.short_open_interest,
        };
        *target = target
            .checked_add(notional)
            .ok_or(ModelError::ArithmeticOverflow)?;
        Ok(())
    }

    fn remove_open_interest(
        &mut self,
        market_id: MarketId,
        side: PositionSide,
        notional: u128,
    ) -> Result<(), ModelError> {
        let market = self
            .markets
            .get_mut(&market_id)
            .ok_or(ModelError::UnknownMarket)?;
        let target = match side {
            PositionSide::Long => &mut market.long_open_interest,
            PositionSide::Short => &mut market.short_open_interest,
        };
        *target = target
            .checked_sub(notional)
            .ok_or(ModelError::ArithmeticOverflow)?;
        Ok(())
    }

    fn route_cash_loss(&mut self, amount: u128) -> Result<(), ModelError> {
        if self.claims.is_empty() {
            self.free_cash = self
                .free_cash
                .checked_add(amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
        } else {
            self.side_bucket = self
                .side_bucket
                .checked_add(amount)
                .ok_or(ModelError::ArithmeticOverflow)?;
        }
        Ok(())
    }

    fn settle_payable(
        &mut self,
        origin_position_id: PositionId,
        origin_market_id: MarketId,
        controller_id: ControllerId,
        amount: u128,
    ) -> Result<(u128, u128, Option<ClaimSequence>), ModelError> {
        if amount == 0 {
            return Ok((0, 0, None));
        }
        let paid = if self.claims.is_empty() {
            core::cmp::min(self.free_cash, amount)
        } else {
            0
        };
        self.free_cash = self
            .free_cash
            .checked_sub(paid)
            .ok_or(ModelError::ArithmeticOverflow)?;
        let queued = amount - paid;
        if queued == 0 {
            return Ok((paid, 0, None));
        }

        let sequence = self.next_claim_sequence;
        self.next_claim_sequence = self
            .next_claim_sequence
            .checked_add(1)
            .ok_or(ModelError::ArithmeticOverflow)?;
        self.total_queued = self
            .total_queued
            .checked_add(queued)
            .ok_or(ModelError::ArithmeticOverflow)?;
        self.claims.push_back(Claim {
            sequence,
            origin_position_id,
            origin_market_id,
            controller_id,
            remaining_face: queued,
            is_dust: queued < self.config.claim_dust_threshold,
        });
        Ok((paid, queued, Some(sequence)))
    }

    fn release_side_bucket(&mut self) -> Result<(), ModelError> {
        self.free_cash = self
            .free_cash
            .checked_add(self.side_bucket)
            .ok_or(ModelError::ArithmeticOverflow)?;
        self.side_bucket = 0;
        Ok(())
    }

    fn record_adjustment(&mut self, adjustment: &ProfitAdjustment) {
        Self::increase_saturating_counter(
            &mut self.fee_accrual,
            &mut self.fee_accrual_saturated,
            self.config.fee_accrual_cap,
            adjustment.win_fee,
        );
    }

    fn record_cash_reward(&mut self, amount: u128) {
        Self::increase_saturating_counter(
            &mut self.cash_loss_reward_basis,
            &mut self.cash_loss_reward_basis_saturated,
            self.config.reward_basis_cap,
            amount,
        );
    }

    fn record_claim_reward(&mut self, amount: u128) {
        Self::increase_saturating_counter(
            &mut self.claim_loss_reward_basis,
            &mut self.claim_loss_reward_basis_saturated,
            self.config.reward_basis_cap,
            amount,
        );
    }

    fn record_residual(&mut self, amount: u128) {
        Self::increase_saturating_counter(
            &mut self.residual_bad_debt,
            &mut self.residual_bad_debt_saturated,
            self.config.residual_bad_debt_cap,
            amount,
        );
    }

    fn increase_saturating_counter(
        current: &mut u128,
        saturated: &mut bool,
        cap: u128,
        amount: u128,
    ) {
        if *saturated {
            return;
        }
        let room = cap - *current;
        if amount >= room {
            *current = cap;
            *saturated = true;
        } else {
            *current += amount;
        }
    }

    fn check_saturated_counter(
        value: u128,
        saturated: bool,
        cap: u128,
        cap_error: InvariantError,
        saturation_error: InvariantError,
    ) -> Result<(), InvariantError> {
        if value > cap {
            return Err(cap_error);
        }
        if saturated && value != cap {
            return Err(saturation_error);
        }
        Ok(())
    }

    fn haircut_rate(config: HaircutConfig, favorable_move_ppm: u32) -> Result<u32, ModelError> {
        let excess = favorable_move_ppm.saturating_sub(config.deadband_move_ppm);
        let linear = Self::mul_div_floor(
            u128::from(excess),
            u128::from(config.linear_coefficient_ppm),
            RATE_SCALE_PPM,
        )?;
        let squared_move =
            Self::mul_div_floor(u128::from(excess), u128::from(excess), RATE_SCALE_PPM)?;
        let quadratic = Self::mul_div_floor(
            squared_move,
            u128::from(config.quadratic_coefficient_ppm),
            RATE_SCALE_PPM,
        )?;
        let decay_denominator = RATE_SCALE_PPM
            .checked_add(linear)
            .and_then(|value| value.checked_add(quadratic))
            .ok_or(ModelError::ArithmeticOverflow)?;
        let rate_spread = u128::from(config.max_rate_ppm - config.base_rate_ppm);
        let decaying_spread = Self::mul_div_floor(rate_spread, RATE_SCALE_PPM, decay_denominator)?;
        let rate = u128::from(config.base_rate_ppm)
            .checked_add(decaying_spread)
            .ok_or(ModelError::ArithmeticOverflow)?;
        u32::try_from(rate).map_err(|_| ModelError::ArithmeticOverflow)
    }

    fn mul_div_floor(a: u128, b: u128, denominator: u128) -> Result<u128, ModelError> {
        if denominator == 0 {
            return Err(ModelError::InvalidConfiguration);
        }
        Ok(crate::wide_math::wide_mul_div_floor_u128(a, b, denominator))
    }

    fn transact<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, ModelError>,
    ) -> Result<T, ModelError> {
        let mut next = self.clone();
        let output = operation(&mut next)?;
        next.check_invariants()
            .map_err(ModelError::InvariantViolation)?;
        *self = next;
        Ok(output)
    }
}
