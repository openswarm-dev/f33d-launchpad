use anchor_lang::prelude::*;

pub mod errors;
pub mod instructions;
pub mod state;
pub mod token_utils;

use instructions::*;
pub use state::*;

declare_id!("4CQamM8uVrGmQ8non8jEgq32topQK1cMyR2VDBf6rnLB");

// Raydium LaunchLab's IDL. The `devnet` feature uses the same IDL at LaunchLab's devnet address
// (idls/raydium_launchpad_devnet.json, written by scripts/devnet-idl.sh).
#[cfg(not(feature = "devnet"))]
declare_program!(raydium_launchpad);
#[cfg(feature = "devnet")]
declare_program!(raydium_launchpad_devnet);
#[cfg(feature = "devnet")]
pub use raydium_launchpad_devnet as raydium_launchpad;

#[program]
pub mod launchpad {
    use super::*;

    pub fn init_global(ctx: Context<InitGlobal>, args: InitGlobalArgs) -> Result<()> {
        instructions::admin::init_global(ctx, args)
    }

    pub fn update_global(ctx: Context<AdminOnly>, args: UpdateGlobalArgs) -> Result<()> {
        instructions::admin::update_global(ctx, args)
    }

    pub fn add_fee_tier(ctx: Context<AddFeeTier>, args: AddFeeTierArgs) -> Result<()> {
        instructions::admin::add_fee_tier(ctx, args)
    }

    pub fn set_fee_tier(ctx: Context<SetFeeTier>, fee_rate: Option<u64>, enabled: Option<bool>) -> Result<()> {
        instructions::admin::set_fee_tier(ctx, fee_rate, enabled)
    }

    pub fn add_pair(ctx: Context<AddPair>, fund_raising: u64, enabled: bool) -> Result<()> {
        instructions::admin::add_pair(ctx, fund_raising, enabled)
    }

    pub fn set_pair(ctx: Context<SetPair>, fund_raising: Option<u64>, enabled: Option<bool>) -> Result<()> {
        instructions::admin::set_pair(ctx, fund_raising, enabled)
    }

    pub fn propose_admin(ctx: Context<AdminOnly>, new_admin: Pubkey) -> Result<()> {
        instructions::admin::propose_admin(ctx, new_admin)
    }

    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        instructions::admin::accept_admin(ctx)
    }

    pub fn propose_distributor(ctx: Context<AdminOnly>, new_distributor: Pubkey) -> Result<()> {
        instructions::admin::propose_distributor(ctx, new_distributor)
    }

    pub fn accept_distributor(ctx: Context<AcceptDistributor>) -> Result<()> {
        instructions::admin::accept_distributor(ctx)
    }

    pub fn set_flagship(ctx: Context<SetFlagship>) -> Result<()> {
        instructions::admin::set_flagship(ctx)
    }

    pub fn launch(ctx: Context<Launch>, args: LaunchArgs) -> Result<()> {
        instructions::launch::launch(ctx, args)
    }

    pub fn claim_pair_fees(ctx: Context<ClaimPairFees>) -> Result<()> {
        instructions::fees::claim_pair_fees(ctx)
    }

    pub fn collect_lp_fees(ctx: Context<CollectLpFees>) -> Result<()> {
        instructions::fees::collect_lp_fees(ctx)
    }

    pub fn withdraw_creator_fees(ctx: Context<WithdrawCreatorFees>) -> Result<()> {
        instructions::fees::withdraw_creator_fees(ctx)
    }

    pub fn set_payout_wallet(ctx: Context<SetPayoutWallet>, new_payout: Pubkey) -> Result<()> {
        instructions::fees::set_payout_wallet(ctx, new_payout)
    }

    pub fn accept_payout_wallet(ctx: Context<AcceptPayoutWallet>) -> Result<()> {
        instructions::fees::accept_payout_wallet(ctx)
    }

    pub fn set_outflow_limit(ctx: Context<SetOutflowLimit>, cap: u64, window_secs: u32) -> Result<()> {
        instructions::admin::set_outflow_limit(ctx, cap, window_secs)
    }

    pub fn release_flagship(ctx: Context<ReleaseFlagship>, amount: u64) -> Result<()> {
        instructions::admin::release_flagship(ctx, amount)
    }

    pub fn withdraw_treasury(ctx: Context<WithdrawTreasury>, amount: u64) -> Result<()> {
        instructions::fees::withdraw_treasury(ctx, amount)
    }

    pub fn attribute_fees(ctx: Context<AttributeFees>, coin_fee: u64, source_amount: u64) -> Result<()> {
        instructions::payouts::attribute_fees(ctx, coin_fee, source_amount)
    }

    pub fn release_pending(ctx: Context<ReleasePending>, amount: u64) -> Result<()> {
        instructions::payouts::release_pending(ctx, amount)
    }

    pub fn pay_holders(ctx: Context<PayHolders>, amount: u64) -> Result<()> {
        instructions::payouts::pay_holders(ctx, amount)
    }

    pub fn pay_traders(ctx: Context<PayTraders>, amount: u64) -> Result<()> {
        instructions::payouts::pay_traders(ctx, amount)
    }

    pub fn pay_creator(ctx: Context<PayCreator>) -> Result<()> {
        instructions::payouts::pay_creator(ctx)
    }

    pub fn spend_flagship(ctx: Context<SpendFlagship>, amount: u64, min_out: u64) -> Result<()> {
        instructions::payouts::spend_flagship(ctx, amount, min_out)
    }

    pub fn spend_flagship_cpmm(ctx: Context<SpendFlagshipCpmm>, amount: u64, min_out: u64) -> Result<()> {
        instructions::payouts::spend_flagship_cpmm(ctx, amount, min_out)
    }

    pub fn convert_begin(ctx: Context<ConvertBegin>, amount: u64) -> Result<()> {
        instructions::convert::convert_begin(ctx, amount)
    }

    pub fn convert_end(ctx: Context<ConvertEnd>, min_out: u64) -> Result<()> {
        instructions::convert::convert_end(ctx, min_out)
    }
}
