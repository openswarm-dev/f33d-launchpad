use anchor_lang::prelude::*;
use anchor_spl::token::Token;
use anchor_spl::token_interface::TokenInterface;

use crate::errors::LaunchpadError;
use crate::raydium_launchpad::{self, program::RaydiumLaunchpad};
use crate::state::*;

/// Metaplex Token Metadata, which LaunchLab uses for coin metadata.
pub const METADATA_PROGRAM_ID: Pubkey = pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct LaunchArgs {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    /// sha256 of the lowercased X handle, if the creator linked one. The attestor checked ownership.
    pub x_handle_hash: Option<[u8; 32]>,
}

/// Launches a coin on our LaunchLab platform, paired with `pair`'s token, and records it as ours.
/// The creator's first buy goes in the same transaction, as a LaunchLab buy right after this.
pub fn launch(ctx: Context<Launch>, args: LaunchArgs) -> Result<()> {
    let global = &ctx.accounts.global;
    require!(!global.paused, LaunchpadError::Paused);
    require!(ctx.accounts.pair.enabled, LaunchpadError::PairDisabled);
    if global.attestor_required {
        let attestor = ctx.accounts.attestor.as_ref().ok_or(LaunchpadError::MissingAttestor)?;
        require_keys_eq!(attestor.key(), global.attestor, LaunchpadError::MissingAttestor);
    }
    require!(args.x_handle_hash.is_some() == ctx.accounts.x_record.is_some(), LaunchpadError::MissingXRecord);

    // Names, tickers and images aren't unique: anyone can launch anything, duplicates included.
    let a = &ctx.accounts;
    raydium_launchpad::cpi::initialize_v2(
        CpiContext::new(
            a.launchlab_program.key(),
            raydium_launchpad::cpi::accounts::InitializeV2 {
                payer: a.creator.to_account_info(),
                creator: a.creator.to_account_info(),
                global_config: a.launch_config.to_account_info(),
                platform_config: a.platform_config.to_account_info(),
                authority: a.launchlab_authority.to_account_info(),
                pool_state: a.pool_state.to_account_info(),
                base_mint: a.base_mint.to_account_info(),
                quote_mint: a.quote_mint.to_account_info(),
                base_vault: a.base_vault.to_account_info(),
                quote_vault: a.quote_vault.to_account_info(),
                metadata_account: a.metadata_account.to_account_info(),
                base_token_program: a.base_token_program.to_account_info(),
                quote_token_program: a.quote_token_program.to_account_info(),
                metadata_program: a.metadata_program.to_account_info(),
                system_program: a.system_program.to_account_info(),
                rent_program: a.rent.to_account_info(),
                event_authority: a.launchlab_event_authority.to_account_info(),
                program: a.launchlab_program.to_account_info(),
            },
        ),
        raydium_launchpad::types::MintParams { decimals: COIN_DECIMALS, name: args.name, symbol: args.symbol.clone(), uri: args.uri },
        raydium_launchpad::types::CurveParams::Constant {
            data: raydium_launchpad::types::ConstantCurve {
                supply: COIN_SUPPLY,
                total_base_sell: COIN_CURVE_SELL,
                total_quote_fund_raising: a.pair.fund_raising,
                migrate_type: MIGRATE_TO_CPMM,
            },
        },
        raydium_launchpad::types::VestingParams { total_locked_amount: 0, cliff_period: 0, unlock_period: 0 },
        raydium_launchpad::types::AmmCreatorFeeOn::QuoteToken,
    )?;

    let mint = ctx.accounts.base_mint.key();
    let launch = &mut ctx.accounts.launch;
    launch.mint = mint;
    launch.creator = ctx.accounts.creator.key();
    launch.quote_mint = ctx.accounts.quote_mint.key();
    launch.pool = ctx.accounts.pool_state.key();
    launch.launched_at = Clock::get()?.unix_timestamp;
    launch.creator_vault_bump = ctx.bumps.creator_vault;
    launch.bump = ctx.bumps.launch;

    let vault = &mut ctx.accounts.creator_vault;
    vault.mint = mint;
    // Creators are paid in SOL whatever the coin trades against (other pair tokens' fees are swapped to SOL first).
    vault.quote_mint = anchor_spl::token::spl_token::native_mint::ID;
    vault.payout = launch.creator;
    vault.bump = ctx.bumps.creator_vault;

    if let Some(x_record) = ctx.accounts.x_record.as_mut() {
        x_record.mint = mint;
        x_record.bump = ctx.bumps.x_record.unwrap_or_default();
    }

    emit!(Launched { mint, quote_mint: launch.quote_mint, ticker: args.symbol, creator: launch.creator, pool: launch.pool });
    Ok(())
}

#[derive(Accounts)]
#[instruction(args: LaunchArgs)]
pub struct Launch<'info> {
    /// The creator pays and is paid; their payout wallet can be changed later.
    #[account(mut)]
    pub creator: Signer<'info>,
    /// Backend key that ran the off-chain checks. Required when `global.attestor_required`.
    pub attestor: Option<Signer<'info>>,

    #[account(seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Box<Account<'info, Global>>,
    /// The trading-fee level the creator picked; the coin trades on its platform for life.
    #[account(
        seeds = [FEE_TIER_SEED, &[fee_tier.index]], bump = fee_tier.bump, has_one = platform_config,
        constraint = fee_tier.enabled @ LaunchpadError::FeeTierDisabled,
    )]
    pub fee_tier: Box<Account<'info, FeeTier>>,
    /// The token this coin is paired with.
    #[account(seeds = [PAIR_SEED, quote_mint.key().as_ref()], bump = pair.bump, has_one = launch_config)]
    pub pair: Box<Account<'info, Pair>>,

    // `init` fails if the record exists, which is what makes X accounts unique.
    #[account(
        init,
        payer = creator,
        space = 8 + XRecord::INIT_SPACE,
        seeds = [X_SEED, args.x_handle_hash.unwrap_or_default().as_ref()],
        bump
    )]
    pub x_record: Option<Box<Account<'info, XRecord>>>,

    #[account(init, payer = creator, space = 8 + LaunchRecord::INIT_SPACE, seeds = [LAUNCH_SEED, base_mint.key().as_ref()], bump)]
    pub launch: Box<Account<'info, LaunchRecord>>,
    #[account(
        init,
        payer = creator,
        space = 8 + CreatorVault::INIT_SPACE,
        seeds = [CREATOR_VAULT_SEED, base_mint.key().as_ref()],
        bump
    )]
    pub creator_vault: Box<Account<'info, CreatorVault>>,

    /// CHECK: LaunchLab's global config for the pair token (pinned by the pair)
    pub launch_config: UncheckedAccount<'info>,
    /// CHECK: the fee level's platform config (pinned by fee_tier)
    pub platform_config: UncheckedAccount<'info>,
    /// CHECK: checked by LaunchLab
    pub launchlab_authority: UncheckedAccount<'info>,
    /// CHECK: created by LaunchLab
    #[account(mut)]
    pub pool_state: UncheckedAccount<'info>,
    #[account(mut)]
    pub base_mint: Signer<'info>,
    /// CHECK: the pair token (the pair PDA is derived from it)
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: created by LaunchLab
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: created by LaunchLab
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: created by LaunchLab through Metaplex
    #[account(mut)]
    pub metadata_account: UncheckedAccount<'info>,
    pub base_token_program: Program<'info, Token>,
    #[account(address = pair.quote_token_program)]
    pub quote_token_program: Interface<'info, TokenInterface>,
    /// CHECK: fixed address
    #[account(address = METADATA_PROGRAM_ID)]
    pub metadata_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
    /// CHECK: checked by LaunchLab
    pub launchlab_event_authority: UncheckedAccount<'info>,
    pub launchlab_program: Program<'info, RaydiumLaunchpad>,
}
