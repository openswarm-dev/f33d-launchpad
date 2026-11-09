use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_spl::token_interface::Mint;

use crate::errors::LaunchpadError;
use crate::raydium_launchpad::{self, accounts::GlobalConfig, program::RaydiumLaunchpad};
use crate::state::*;

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitGlobalArgs {
    pub attestor: Pubkey,
    pub attestor_required: bool,
    pub treasury: Pubkey,
    pub distributor: Pubkey,
    pub split: FeeSplit,
}

/// Every field is optional; `None` leaves it unchanged. The distributor and the flagship have their own
/// instructions (`propose_distributor` / `accept_distributor`, `set_flagship`).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct UpdateGlobalArgs {
    pub attestor: Option<Pubkey>,
    pub attestor_required: Option<bool>,
    pub treasury: Option<Pubkey>,
    pub split: Option<FeeSplit>,
    pub paused: Option<bool>,
    /// The payout bot's emergency stop (see `Global::payouts_paused`).
    pub payouts_paused: Option<bool>,
}

/// A new pair's outflow window (the cap starts at 0: no payouts until the admin sets it).
pub const DEFAULT_OUTFLOW_WINDOW_SECS: u32 = 86_400;
pub const MIN_OUTFLOW_WINDOW_SECS: u32 = 60;

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct AddFeeTierArgs {
    pub index: u8,
    /// Our fee on every curve trade, on LaunchLab's scale (1,000,000 = 100%), on top of Raydium's 0.25%:
    /// 7,500 / 12,500 / 17,500 / 22,500 make 1 / 1.5 / 2 / 2.5% in total.
    pub fee_rate: u64,
    pub name: String,
    pub web: String,
    pub img: String,
    /// SOL for the tier-admin PDA, which pays the platform config's rent.
    pub admin_lamports: u64,
    /// SOL moved into the authority so it can pay rent for accounts LaunchLab creates for it (may be 0).
    pub authority_lamports: u64,
}

pub fn init_global(ctx: Context<InitGlobal>, args: InitGlobalArgs) -> Result<()> {
    require!(args.split.is_valid(), LaunchpadError::InvalidSplit);
    require!(args.treasury != Pubkey::default(), LaunchpadError::InvalidTreasury);
    let global = &mut ctx.accounts.global;
    global.admin = ctx.accounts.admin.key();
    global.pending_admin = Pubkey::default();
    global.attestor = args.attestor;
    global.attestor_required = args.attestor_required;
    global.treasury = args.treasury;
    global.distributor = args.distributor;
    global.split = args.split;
    global.legacy_platform_config = Pubkey::default();
    global.pending_distributor = Pubkey::default();
    global.flagship_mint = Pubkey::default();
    global.paused = false;
    global.payouts_paused = false;
    global.authority_bump = ctx.bumps.authority;
    global.bump = ctx.bumps.global;
    Ok(())
}

pub fn update_global(ctx: Context<AdminOnly>, args: UpdateGlobalArgs) -> Result<()> {
    let global = &mut ctx.accounts.global;
    if let Some(attestor) = args.attestor {
        global.attestor = attestor;
    }
    if let Some(required) = args.attestor_required {
        global.attestor_required = required;
    }
    if let Some(treasury) = args.treasury {
        require!(treasury != Pubkey::default(), LaunchpadError::InvalidTreasury);
        global.treasury = treasury;
    }
    if let Some(split) = args.split {
        require!(split.is_valid(), LaunchpadError::InvalidSplit);
        global.split = split;
    }
    if let Some(paused) = args.paused {
        global.paused = paused;
    }
    if let Some(paused) = args.payouts_paused {
        global.payouts_paused = paused;
    }
    emit!(GlobalUpdated {
        attestor: global.attestor,
        attestor_required: global.attestor_required,
        treasury: global.treasury,
        split: global.split,
        paused: global.paused,
        payouts_paused: global.payouts_paused,
    });
    Ok(())
}

/// Adds a trading-fee level: a LaunchLab platform config whose admin is this level's tier-admin PDA, and whose
/// fee wallet and LP-NFT wallet are the authority, so every level's fees and locked-LP fee NFTs end up in the
/// same place. When a coin graduates, all of its locked LP's fee rights come to us (none to the creator, none
/// burned). There's no separate LaunchLab creator fee: the creator's share comes out of ours.
pub fn add_fee_tier(ctx: Context<AddFeeTier>, args: AddFeeTierArgs) -> Result<()> {
    let a = &ctx.accounts;
    for (to, lamports) in [(a.tier_admin.to_account_info(), args.admin_lamports), (a.authority.to_account_info(), args.authority_lamports)] {
        if lamports > 0 {
            system_program::transfer(
                CpiContext::new(a.system_program.key(), system_program::Transfer { from: a.admin.to_account_info(), to }),
                lamports,
            )?;
        }
    }
    let authority = a.authority.to_account_info();
    let index = [args.index];
    let seeds: &[&[u8]] = &[TIER_ADMIN_SEED, &index, &[ctx.bumps.tier_admin]];
    raydium_launchpad::cpi::create_platform_config(
        CpiContext::new_with_signer(
            a.launchlab_program.key(),
            raydium_launchpad::cpi::accounts::CreatePlatformConfig {
                platform_admin: a.tier_admin.to_account_info(),
                platform_fee_wallet: authority.clone(),
                platform_nft_wallet: authority.clone(),
                platform_config: a.platform_config.to_account_info(),
                cpswap_config: a.cpmm_config.to_account_info(),
                system_program: a.system_program.to_account_info(),
                transfer_fee_extension_authority: authority.clone(),
                platform_vesting_wallet: authority,
            },
            &[seeds],
        ),
        raydium_launchpad::types::PlatformParams {
            migrate_nft_info: raydium_launchpad::types::MigrateNftInfo { platform_scale: 1_000_000, creator_scale: 0, burn_scale: 0 },
            fee_rate: args.fee_rate,
            name: args.name,
            web: args.web,
            img: args.img,
            creator_fee_rate: 0,
            platform_vesting_scale: 0,
        },
    )?;
    let tier = &mut ctx.accounts.fee_tier;
    tier.index = args.index;
    tier.fee_rate = args.fee_rate;
    tier.platform_config = ctx.accounts.platform_config.key();
    tier.cpmm_config = ctx.accounts.cpmm_config.key();
    tier.enabled = true;
    tier.bump = ctx.bumps.fee_tier;
    tier.admin_bump = ctx.bumps.tier_admin;
    Ok(())
}

/// Changes a fee level. `enabled` gates new launches on it. `fee_rate` changes the rate its platform charges,
/// which applies to every coin already on it too (LaunchLab reads the rate at each trade), so it's for
/// corrections; a new level is the way to offer a new rate.
pub fn set_fee_tier(ctx: Context<SetFeeTier>, fee_rate: Option<u64>, enabled: Option<bool>) -> Result<()> {
    if let Some(rate) = fee_rate {
        let a = &ctx.accounts;
        let index = [a.fee_tier.index];
        let seeds: &[&[u8]] = &[TIER_ADMIN_SEED, &index, &[a.fee_tier.admin_bump]];
        raydium_launchpad::cpi::update_platform_config(
            CpiContext::new_with_signer(
                a.launchlab_program.key(),
                raydium_launchpad::cpi::accounts::UpdatePlatformConfig {
                    platform_admin: a.tier_admin.to_account_info(),
                    platform_config: a.platform_config.to_account_info(),
                },
                &[seeds],
            ),
            raydium_launchpad::types::PlatformConfigParam::FeeRate(rate),
        )?;
        ctx.accounts.fee_tier.fee_rate = rate;
    }
    if let Some(enabled) = enabled {
        ctx.accounts.fee_tier.enabled = enabled;
    }
    Ok(())
}

/// Adds a token coins can be paired with. LaunchLab must already have a global config for it.
pub fn add_pair(ctx: Context<AddPair>, fund_raising: u64, enabled: bool) -> Result<()> {
    let config = &ctx.accounts.launch_config;
    require!(
        config.quote_mint == ctx.accounts.quote_mint.key() && config.curve_type == 0 && fund_raising >= config.min_quote_fund_raising,
        LaunchpadError::InvalidLaunchConfig
    );
    let pair = &mut ctx.accounts.pair;
    pair.quote_mint = ctx.accounts.quote_mint.key();
    pair.quote_token_program = *ctx.accounts.quote_mint.to_account_info().owner;
    pair.launch_config = config.key();
    pair.fund_raising = fund_raising;
    pair.enabled = enabled;
    pair.bump = ctx.bumps.pair;
    pair.pending_split = ctx.accounts.global.split;
    // No payouts until the admin sets the pair's outflow limit.
    pair.outflow_cap = 0;
    pair.outflow_window_secs = DEFAULT_OUTFLOW_WINDOW_SECS;
    Ok(())
}

/// Admin: what the payout bot may move out of a pair's pools per window (see `Pair::outflow_cap`). Takes effect
/// at once; the current window keeps what it has used.
pub fn set_outflow_limit(ctx: Context<SetOutflowLimit>, cap: u64, window_secs: u32) -> Result<()> {
    require!(window_secs >= MIN_OUTFLOW_WINDOW_SECS, LaunchpadError::InvalidWindow);
    let pair = &mut ctx.accounts.pair;
    pair.outflow_cap = cap;
    pair.outflow_window_secs = window_secs;
    emit!(OutflowLimitSet { quote_mint: pair.quote_mint, cap, window_secs });
    Ok(())
}

/// Admin: moves `amount` of a pair's unspent flagship pool to its treasury, for flagship shares that can't be
/// bought back: after a distributor rotation (the flagship's creator no longer matches the distributor), or in a
/// pair other than the flagship's own. Booked as flagship out and treasury in; the funds stay with the authority.
pub fn release_flagship(ctx: Context<ReleaseFlagship>, amount: u64) -> Result<()> {
    let pair = &mut ctx.accounts.pair;
    require!(amount > 0 && amount <= pair.flagship_in - pair.flagship_out, LaunchpadError::ExceedsPool);
    pair.flagship_out += amount;
    pair.treasury_in += amount;
    emit!(FlagshipReleased { quote_mint: pair.quote_mint, amount });
    Ok(())
}

pub fn set_pair(ctx: Context<SetPair>, fund_raising: Option<u64>, enabled: Option<bool>) -> Result<()> {
    let pair = &mut ctx.accounts.pair;
    if let Some(amount) = fund_raising {
        require!(amount >= ctx.accounts.launch_config.min_quote_fund_raising, LaunchpadError::InvalidLaunchConfig);
        pair.fund_raising = amount;
    }
    if let Some(enabled) = enabled {
        pair.enabled = enabled;
    }
    Ok(())
}

pub fn propose_admin(ctx: Context<AdminOnly>, new_admin: Pubkey) -> Result<()> {
    ctx.accounts.global.pending_admin = new_admin;
    Ok(())
}

pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
    let global = &mut ctx.accounts.global;
    require_keys_eq!(ctx.accounts.new_admin.key(), global.pending_admin, LaunchpadError::NotPendingAdmin);
    global.admin = global.pending_admin;
    global.pending_admin = Pubkey::default();
    Ok(())
}

/// Admin: proposes a new distributor (the developer wallet that launched the flagship). It takes over only
/// when it signs `accept_distributor`, so a typo can't hand the payouts to a key nobody holds. Proposing
/// the default key cancels a proposal.
pub fn propose_distributor(ctx: Context<AdminOnly>, new_distributor: Pubkey) -> Result<()> {
    ctx.accounts.global.pending_distributor = new_distributor;
    emit!(DistributorProposed { proposed: new_distributor });
    Ok(())
}

pub fn accept_distributor(ctx: Context<AcceptDistributor>) -> Result<()> {
    let global = &mut ctx.accounts.global;
    let new_distributor = ctx.accounts.new_distributor.key();
    require!(
        global.pending_distributor != Pubkey::default() && new_distributor == global.pending_distributor,
        LaunchpadError::NotPendingDistributor
    );
    let previous = global.distributor;
    global.distributor = new_distributor;
    global.pending_distributor = Pubkey::default();
    emit!(DistributorChanged { previous, distributor: new_distributor });
    Ok(())
}

/// Admin: makes a coin launched through us the flagship, which the flagship pools buy and burn. Its creator
/// (the "DEV" every terminal shows) must be the distributor or the proposed one, because the distributor is
/// the wallet that makes those buys; the buy instructions check it again each time.
pub fn set_flagship(ctx: Context<SetFlagship>) -> Result<()> {
    let launch = &ctx.accounts.flagship_launch;
    let global = &mut ctx.accounts.global;
    require!(may_be_flagship(&launch.creator, &global.distributor, &global.pending_distributor), LaunchpadError::NotFlagshipCreator);
    global.flagship_mint = launch.mint;
    emit!(FlagshipSet { mint: launch.mint, creator: launch.creator });
    Ok(())
}

#[derive(Accounts)]
pub struct InitGlobal<'info> {
    /// Must be the program's upgrade authority, so nobody can take the program over between deploy and setup.
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [crate::ID.as_ref()], bump, seeds::program = anchor_lang::solana_program::bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address == Some(admin.key()) @ LaunchpadError::NotUpgradeAuthority,
    )]
    pub program_data: Account<'info, ProgramData>,
    #[account(init, payer = admin, space = 8 + Global::INIT_SPACE, seeds = [GLOBAL_SEED], bump)]
    pub global: Account<'info, Global>,
    /// Our LaunchLab platform and the holder of all our fees.
    #[account(seeds = [AUTHORITY_SEED], bump)]
    pub authority: SystemAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(args: AddFeeTierArgs)]
pub struct AddFeeTier<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump, has_one = admin)]
    pub global: Account<'info, Global>,
    #[account(init, payer = admin, space = 8 + FeeTier::INIT_SPACE, seeds = [FEE_TIER_SEED, &[args.index]], bump)]
    pub fee_tier: Account<'info, FeeTier>,
    /// This level's LaunchLab platform admin.
    #[account(mut, seeds = [TIER_ADMIN_SEED, &[args.index]], bump)]
    pub tier_admin: SystemAccount<'info>,
    /// Fee wallet and LP-NFT wallet of every level.
    #[account(mut, seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    /// CHECK: created by LaunchLab at its PDA of the tier admin
    #[account(mut)]
    pub platform_config: UncheckedAccount<'info>,
    /// CHECK: the CPMM config this level's coins graduate into; checked by LaunchLab
    pub cpmm_config: UncheckedAccount<'info>,
    pub launchlab_program: Program<'info, RaydiumLaunchpad>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetFeeTier<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump, has_one = admin)]
    pub global: Account<'info, Global>,
    #[account(mut, seeds = [FEE_TIER_SEED, &[fee_tier.index]], bump = fee_tier.bump, has_one = platform_config)]
    pub fee_tier: Account<'info, FeeTier>,
    #[account(seeds = [TIER_ADMIN_SEED, &[fee_tier.index]], bump = fee_tier.admin_bump)]
    pub tier_admin: SystemAccount<'info>,
    /// CHECK: this level's platform config (has_one above); LaunchLab checks the rest
    #[account(mut)]
    pub platform_config: UncheckedAccount<'info>,
    pub launchlab_program: Program<'info, RaydiumLaunchpad>,
}

#[derive(Accounts)]
pub struct AddPair<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump, has_one = admin)]
    pub global: Account<'info, Global>,
    #[account(init, payer = admin, space = 8 + Pair::INIT_SPACE, seeds = [PAIR_SEED, quote_mint.key().as_ref()], bump)]
    pub pair: Account<'info, Pair>,
    pub quote_mint: InterfaceAccount<'info, Mint>,
    pub launch_config: Account<'info, GlobalConfig>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetPair<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump, has_one = admin)]
    pub global: Account<'info, Global>,
    #[account(mut, seeds = [PAIR_SEED, pair.quote_mint.as_ref()], bump = pair.bump, has_one = launch_config)]
    pub pair: Account<'info, Pair>,
    pub launch_config: Account<'info, GlobalConfig>,
}

#[derive(Accounts)]
pub struct SetOutflowLimit<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump, has_one = admin)]
    pub global: Account<'info, Global>,
    #[account(mut, seeds = [PAIR_SEED, pair.quote_mint.as_ref()], bump = pair.bump)]
    pub pair: Account<'info, Pair>,
}

#[derive(Accounts)]
pub struct ReleaseFlagship<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump, has_one = admin)]
    pub global: Account<'info, Global>,
    #[account(mut, seeds = [PAIR_SEED, pair.quote_mint.as_ref()], bump = pair.bump)]
    pub pair: Account<'info, Pair>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [GLOBAL_SEED], bump = global.bump, has_one = admin)]
    pub global: Account<'info, Global>,
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    pub new_admin: Signer<'info>,
    #[account(mut, seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Account<'info, Global>,
}

#[derive(Accounts)]
pub struct AcceptDistributor<'info> {
    pub new_distributor: Signer<'info>,
    #[account(mut, seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Account<'info, Global>,
}

#[derive(Accounts)]
pub struct SetFlagship<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [GLOBAL_SEED], bump = global.bump, has_one = admin)]
    pub global: Account<'info, Global>,
    /// The coin's launch record: it must have been launched through us.
    #[account(seeds = [LAUNCH_SEED, flagship_launch.mint.as_ref()], bump = flagship_launch.bump)]
    pub flagship_launch: Account<'info, LaunchRecord>,
}
