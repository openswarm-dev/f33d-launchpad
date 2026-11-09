//! Collecting our fees from LaunchLab and Burn & Earn, and splitting them. Everything we collect goes into
//! the authority's token account for the pair token; the books on `Pair`, `LaunchRecord` and
//! `CreatorVault` say whose it is.
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{instruction::Instruction, program::invoke_signed};
use anchor_spl::associated_token::{get_associated_token_address_with_program_id, AssociatedToken};
use anchor_spl::token::Token;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::errors::LaunchpadError;
use crate::raydium_launchpad::{self, program::RaydiumLaunchpad};
use crate::state::*;
use crate::token_utils::{burn_tokens, create_ata, mint_supply, pubkey_at, require_ata, token_amount, transfer_tokens};

// Raydium's CPMM and Burn & Earn; the `devnet` feature builds against their devnet deployments.
#[cfg(not(feature = "devnet"))]
pub const CPMM_PROGRAM_ID: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
#[cfg(not(feature = "devnet"))]
pub const LOCK_PROGRAM_ID: Pubkey = pubkey!("LockrWmn6K5twhz3y9w1dQERbmgSaRkfnTeTKbpofwE");
#[cfg(feature = "devnet")]
pub const CPMM_PROGRAM_ID: Pubkey = pubkey!("DRaycpLY18LhpbydsBWbVJtxpNv9oXPgjRSfpF2bWpYb");
#[cfg(feature = "devnet")]
pub const LOCK_PROGRAM_ID: Pubkey = pubkey!("DRay25Usp3YJAi7beckgpGUC7mGJ2cR1AVPxhYfwVCUX");
pub const MEMO_PROGRAM_ID: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
/// Burn & Earn `collect_cp_fees` (its IDL predates Anchor discriminators in IDLs).
const COLLECT_CP_FEES: [u8; 8] = [8, 30, 51, 199, 209, 184, 247, 133];
/// `LockedCpLiquidityState`: disc 8, five u64s and a u128, then pool_id at 64 and fee_nft_mint at 96.
const LOCK_POOL_ID_OFFSET: usize = 64;
const LOCK_FEE_NFT_OFFSET: usize = 96;

/// Credits fees that belong to one coin: creator and holders to the coin, the rest to its pair.
fn credit_coin(split: &FeeSplit, pair: &mut Pair, launch: &mut LaunchRecord, vault: &mut CreatorVault, amount: u64, source: u8) {
    let s = split.apply(amount);
    vault.harvested += s.creator;
    launch.holders_in += s.holders;
    launch.fees += amount;
    credit_pair(pair, &s, amount);
    emit!(FeesCollected {
        mint: launch.mint,
        quote_mint: pair.quote_mint,
        source,
        amount,
        creator: s.creator,
        holders: s.holders,
        pending: 0,
        flagship: s.flagship,
        traders: s.traders,
        treasury: s.treasury,
    });
}

fn credit_pair(pair: &mut Pair, s: &SplitAmounts, amount: u64) {
    pair.collected += amount;
    pair.flagship_in += s.flagship;
    pair.traders_in += s.traders;
    pair.treasury_in += s.treasury;
}

/// Permissionless: claims our curve fees for one pair token from LaunchLab's platform fee vault, where every
/// trade on our platform pays them (LaunchLab rejects trades that don't pass it). They aren't labelled by coin, so the per-coin shares (creator, holders) wait as `pending` for the
/// payout bot to attribute; the per-pair shares are booked now.
pub fn claim_pair_fees(ctx: Context<ClaimPairFees>) -> Result<()> {
    let a = &ctx.accounts;
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[a.global.authority_bump]];
    let before = token_amount(&a.authority_quote_ata)?;
    raydium_launchpad::cpi::claim_platform_fee_from_vault(CpiContext::new_with_signer(
        a.launchlab_program.key(),
        raydium_launchpad::cpi::accounts::ClaimPlatformFeeFromVault {
            platform_fee_wallet: a.authority.to_account_info(),
            fee_vault_authority: a.fee_vault_authority.to_account_info(),
            platform_config: a.platform_config.to_account_info(),
            platform_fee_vault: a.platform_fee_vault.to_account_info(),
            recipient_token_account: a.authority_quote_ata.to_account_info(),
            quote_mint: a.quote_mint.to_account_info(),
            token_program: a.token_program.to_account_info(),
            system_program: a.system_program.to_account_info(),
            associated_token_program: a.associated_token_program.to_account_info(),
        },
        &[seeds],
    ))?;
    let amount = token_amount(&a.authority_quote_ata)?.saturating_sub(before);
    let split = a.global.split;
    let pair = &mut ctx.accounts.pair;
    // Every payout is in SOL: a pair token other than SOL waits whole in `pending` (its unconverted fees) until the
    // payout bot swaps it to SOL (convert_begin / convert_end), which books the SOL like a SOL claim.
    if pair.quote_mint != anchor_spl::token::spl_token::native_mint::ID {
        pair.collected += amount;
        pair.pending += amount;
        emit!(FeesCollected {
            mint: Pubkey::default(), quote_mint: pair.quote_mint, source: SOURCE_FEE_VAULT, amount,
            creator: 0, holders: 0, pending: amount, flagship: 0, traders: 0, treasury: 0,
        });
        return Ok(());
    }
    // Pending is always booked under one split, the one attribution will use: a new split takes effect for
    // curve fees once the fees claimed under the old one are attributed or released.
    if pair.pending == 0 {
        pair.pending_split = split;
    }
    let s = pair.pending_split.apply(amount);
    let pending = s.creator + s.holders;
    pair.pending += pending;
    credit_pair(pair, &s, amount);
    emit!(FeesCollected {
        mint: Pubkey::default(),
        quote_mint: pair.quote_mint,
        source: SOURCE_FEE_VAULT,
        amount,
        creator: 0,
        holders: 0,
        pending,
        flagship: s.flagship,
        traders: s.traders,
        treasury: s.treasury,
    });
    Ok(())
}

/// Permissionless: after a coin graduates, collects the trading fees its locked LP earned (our fee NFT
/// in Burn & Earn). The pair-token half splits like any other fee; the coin half is burned.
pub fn collect_lp_fees(mut ctx: Context<CollectLpFees>) -> Result<()> {
    let a = &ctx.accounts;
    // The lock record must be for this coin's graduated pool, and its fee NFT must be ours.
    let (coin, quote) = (a.launch.mint, a.launch.quote_mint);
    let (token0, token1) = if coin < quote { (coin, quote) } else { (quote, coin) };
    let pool = Pubkey::find_program_address(
        &[b"pool", a.fee_tier.cpmm_config.as_ref(), token0.as_ref(), token1.as_ref()],
        &CPMM_PROGRAM_ID,
    )
    .0;
    {
        let lock = a.locked_liquidity.try_borrow_data()?;
        require!(
            *a.locked_liquidity.owner == LOCK_PROGRAM_ID
                && a.cpmm_pool.key() == pool
                && pubkey_at(&lock, LOCK_POOL_ID_OFFSET) == pool
                && pubkey_at(&lock, LOCK_FEE_NFT_OFFSET) == a.fee_nft_mint.key(),
            LaunchpadError::InvalidLpLock
        );
    }
    require_ata(&a.fee_nft_account, a.authority.key, &a.fee_nft_mint.key(), &a.fee_nft_token_program.key())?;

    let authority = a.authority.to_account_info();
    let cranker = a.cranker.to_account_info();
    let system = a.system_program.to_account_info();
    let ata_program = a.associated_token_program.to_account_info();
    create_ata(&cranker, &a.authority_quote_ata, &authority, &a.quote_mint.to_account_info(), &system, &a.quote_token_program.to_account_info(), &ata_program)?;
    create_ata(&cranker, &a.authority_coin_ata, &authority, &a.coin_mint.to_account_info(), &system, &a.coin_token_program.to_account_info(), &ata_program)?;
    let quote_before = token_amount(&a.authority_quote_ata)?;
    let coin_before = token_amount(&a.authority_coin_ata)?;

    let quote_is_0 = quote == token0;
    let (recipient0, recipient1) =
        if quote_is_0 { (&a.authority_quote_ata, &a.authority_coin_ata) } else { (&a.authority_coin_ata, &a.authority_quote_ata) };
    let (mint0, mint1) = if quote_is_0 {
        (a.quote_mint.to_account_info(), a.coin_mint.to_account_info())
    } else {
        (a.coin_mint.to_account_info(), a.quote_mint.to_account_info())
    };
    // Ask for up to half the LP supply: Burn & Earn pays out what the position has earned, no more.
    let fee_lp_amount = mint_supply(&a.lp_mint)? / 2;
    let mut data = COLLECT_CP_FEES.to_vec();
    data.extend_from_slice(&fee_lp_amount.to_le_bytes());
    let metas = vec![
        AccountMeta::new_readonly(a.lock_authority.key(), false),
        AccountMeta::new_readonly(a.authority.key(), true),
        AccountMeta::new_readonly(a.fee_nft_account.key(), false),
        AccountMeta::new(a.locked_liquidity.key(), false),
        AccountMeta::new_readonly(CPMM_PROGRAM_ID, false),
        AccountMeta::new_readonly(a.cpmm_authority.key(), false),
        AccountMeta::new(a.cpmm_pool.key(), false),
        AccountMeta::new(a.lp_mint.key(), false),
        AccountMeta::new(recipient0.key(), false),
        AccountMeta::new(recipient1.key(), false),
        AccountMeta::new(a.token0_vault.key(), false),
        AccountMeta::new(a.token1_vault.key(), false),
        AccountMeta::new_readonly(mint0.key(), false),
        AccountMeta::new_readonly(mint1.key(), false),
        AccountMeta::new(a.locked_lp_vault.key(), false),
        AccountMeta::new_readonly(a.coin_token_program.key(), false),
        AccountMeta::new_readonly(anchor_spl::token_2022::ID, false),
        AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
    ];
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[a.global.authority_bump]];
    invoke_signed(
        &Instruction { program_id: LOCK_PROGRAM_ID, accounts: metas, data },
        &[
            a.lock_authority.to_account_info(),
            authority.clone(),
            a.fee_nft_account.to_account_info(),
            a.locked_liquidity.to_account_info(),
            a.cpmm_program.to_account_info(),
            a.cpmm_authority.to_account_info(),
            a.cpmm_pool.to_account_info(),
            a.lp_mint.to_account_info(),
            recipient0.to_account_info(),
            recipient1.to_account_info(),
            a.token0_vault.to_account_info(),
            a.token1_vault.to_account_info(),
            mint0,
            mint1,
            a.locked_lp_vault.to_account_info(),
            a.coin_token_program.to_account_info(),
            a.token_program_2022.to_account_info(),
            a.memo_program.to_account_info(),
            a.lock_program.to_account_info(),
        ],
        &[seeds],
    )?;

    let quote_gain = token_amount(&a.authority_quote_ata)?.saturating_sub(quote_before);
    let coin_gain = token_amount(&a.authority_coin_ata)?.saturating_sub(coin_before);
    burn_tokens(&a.authority_coin_ata, &a.coin_mint.to_account_info(), &authority, &a.coin_token_program.to_account_info(), &[seeds], coin_gain)?;

    let split = a.global.split;
    let c = &mut ctx.accounts;
    c.launch.burned += coin_gain;
    emit!(LpTokensBurned { mint: c.launch.mint, amount: coin_gain });
    // A coin on another pair token: its LP fees wait unconverted, like its curve fees; once swapped to SOL, the payout
    // bot attributes the coin's share from the indexed events.
    if c.pair.quote_mint != anchor_spl::token::spl_token::native_mint::ID {
        c.pair.collected += quote_gain;
        c.pair.pending += quote_gain;
        emit!(FeesCollected {
            mint: c.launch.mint, quote_mint: c.pair.quote_mint, source: SOURCE_LP, amount: quote_gain,
            creator: 0, holders: 0, pending: quote_gain, flagship: 0, traders: 0, treasury: 0,
        });
        return Ok(());
    }
    credit_coin(&split, &mut c.pair, &mut c.launch, &mut c.creator_vault, quote_gain, SOURCE_LP);
    Ok(())
}

/// The payout wallet withdraws everything its coin has earned so far, to a token account of its choice.
/// (For SOL, that's wrapped SOL; the app unwraps it in the same transaction.)
pub fn withdraw_creator_fees(ctx: Context<WithdrawCreatorFees>) -> Result<()> {
    let a = &ctx.accounts;
    let amount = a.creator_vault.harvested.saturating_sub(a.creator_vault.withdrawn);
    require!(amount > 0, LaunchpadError::NothingToWithdraw);
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[a.global.authority_bump]];
    transfer_tokens(
        &a.authority_quote_ata.to_account_info(),
        &a.destination.to_account_info(),
        &a.quote_mint.to_account_info(),
        &a.authority.to_account_info(),
        &a.token_program.to_account_info(),
        &[seeds],
        amount,
    )?;
    let vault = &mut ctx.accounts.creator_vault;
    vault.withdrawn += amount;
    emit!(CreatorFeesWithdrawn { mint: vault.mint, payout: vault.payout, amount });
    Ok(())
}

/// The payout wallet changes where its coin's creator earnings go. The treasury takes over at once; any other
/// wallet is proposed and takes over when it signs `accept_payout_wallet`, so a mistyped address can never
/// receive them. The default key cancels a proposal.
pub fn set_payout_wallet(ctx: Context<SetPayoutWallet>, new_payout: Pubkey) -> Result<()> {
    let treasury = ctx.accounts.global.treasury;
    let vault = &mut ctx.accounts.creator_vault;
    let previous = vault.payout;
    (vault.payout, vault.pending_payout) = payout_change(previous, new_payout, treasury);
    if vault.payout != previous {
        emit!(PayoutWalletChanged { mint: vault.mint, previous, payout: vault.payout });
    } else {
        emit!(PayoutWalletProposed { mint: vault.mint, payout: previous, proposed: vault.pending_payout });
    }
    Ok(())
}

/// The proposed payout wallet takes over.
pub fn accept_payout_wallet(ctx: Context<AcceptPayoutWallet>) -> Result<()> {
    let vault = &mut ctx.accounts.creator_vault;
    let new_payout = ctx.accounts.new_payout.key();
    require!(vault.pending_payout != Pubkey::default() && new_payout == vault.pending_payout, LaunchpadError::NotPendingPayout);
    let previous = vault.payout;
    vault.payout = new_payout;
    vault.pending_payout = Pubkey::default();
    emit!(PayoutWalletChanged { mint: vault.mint, previous, payout: new_payout });
    Ok(())
}

/// Admin: sends one pair token's treasury to the treasury wallet's token account.
pub fn withdraw_treasury(ctx: Context<WithdrawTreasury>, amount: u64) -> Result<()> {
    let a = &ctx.accounts;
    require!(amount > 0 && amount <= a.pair.treasury_in - a.pair.treasury_out, LaunchpadError::InsufficientTreasury);
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[a.global.authority_bump]];
    transfer_tokens(
        &a.authority_quote_ata.to_account_info(),
        &a.destination.to_account_info(),
        &a.quote_mint.to_account_info(),
        &a.authority.to_account_info(),
        &a.token_program.to_account_info(),
        &[seeds],
        amount,
    )?;
    ctx.accounts.pair.treasury_out += amount;
    Ok(())
}

#[derive(Accounts)]
pub struct ClaimPairFees<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Box<Account<'info, Global>>,
    /// The fee level whose platform vault is claimed; each level has its own vault per pair token.
    #[account(seeds = [FEE_TIER_SEED, &[fee_tier.index]], bump = fee_tier.bump, has_one = platform_config)]
    pub fee_tier: Box<Account<'info, FeeTier>>,
    #[account(mut, seeds = [PAIR_SEED, quote_mint.key().as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, Pair>>,
    #[account(mut, seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    /// CHECK: checked by LaunchLab
    pub fee_vault_authority: UncheckedAccount<'info>,
    /// CHECK: the fee level's platform config (pinned by fee_tier)
    pub platform_config: UncheckedAccount<'info>,
    /// CHECK: LaunchLab's fee vault for that platform and this token; checked by LaunchLab
    #[account(mut)]
    pub platform_fee_vault: UncheckedAccount<'info>,
    /// CHECK: the authority's account for the pair token (LaunchLab creates it on the first claim)
    #[account(mut, address = get_associated_token_address_with_program_id(authority.key, &pair.quote_mint, &pair.quote_token_program))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    #[account(address = pair.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = pair.quote_token_program)]
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub launchlab_program: Program<'info, RaydiumLaunchpad>,
}

#[derive(Accounts)]
pub struct CollectLpFees<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Box<Account<'info, Global>>,
    /// The coin's fee level: its CPMM config fixes the graduated pool's address (checked against the lock below).
    #[account(seeds = [FEE_TIER_SEED, &[fee_tier.index]], bump = fee_tier.bump)]
    pub fee_tier: Box<Account<'info, FeeTier>>,
    #[account(mut, seeds = [PAIR_SEED, launch.quote_mint.as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, Pair>>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, LaunchRecord>>,
    #[account(mut, seeds = [CREATOR_VAULT_SEED, launch.mint.as_ref()], bump = creator_vault.bump)]
    pub creator_vault: Box<Account<'info, CreatorVault>>,
    #[account(mut, seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    /// CHECK: the coin; we burn its LP-fee half
    #[account(mut, address = launch.mint)]
    pub coin_mint: UncheckedAccount<'info>,
    /// CHECK: the pair token
    #[account(address = launch.quote_mint)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the authority's account for the pair token, created here
    #[account(mut)]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the authority's account for the coin, created here
    #[account(mut)]
    pub authority_coin_ata: UncheckedAccount<'info>,
    /// CHECK: our fee NFT for this coin's locked LP (checked against the lock record)
    pub fee_nft_mint: UncheckedAccount<'info>,
    /// CHECK: the authority's account holding the fee NFT
    pub fee_nft_account: UncheckedAccount<'info>,
    /// CHECK: Burn & Earn's lock record (owner, pool and NFT checked in the handler)
    #[account(mut)]
    pub locked_liquidity: UncheckedAccount<'info>,
    /// CHECK: checked by Burn & Earn
    pub lock_authority: UncheckedAccount<'info>,
    /// CHECK: checked by Burn & Earn
    #[account(mut)]
    pub locked_lp_vault: UncheckedAccount<'info>,
    /// CHECK: checked by Burn & Earn
    pub cpmm_authority: UncheckedAccount<'info>,
    /// CHECK: the coin's graduated pool (derived in the handler)
    #[account(mut)]
    pub cpmm_pool: UncheckedAccount<'info>,
    /// CHECK: checked by Burn & Earn
    #[account(mut)]
    pub lp_mint: UncheckedAccount<'info>,
    /// CHECK: checked by CPMM
    #[account(mut)]
    pub token0_vault: UncheckedAccount<'info>,
    /// CHECK: checked by CPMM
    #[account(mut)]
    pub token1_vault: UncheckedAccount<'info>,
    #[account(address = pair.quote_token_program)]
    pub quote_token_program: Interface<'info, TokenInterface>,
    /// The coin's token program (always SPL Token).
    pub coin_token_program: Program<'info, Token>,
    /// CHECK: the fee NFT's token program
    pub fee_nft_token_program: UncheckedAccount<'info>,
    /// CHECK: fixed address
    #[account(address = anchor_spl::token_2022::ID)]
    pub token_program_2022: UncheckedAccount<'info>,
    /// CHECK: fixed address
    #[account(address = MEMO_PROGRAM_ID)]
    pub memo_program: UncheckedAccount<'info>,
    /// CHECK: fixed address
    #[account(address = CPMM_PROGRAM_ID)]
    pub cpmm_program: UncheckedAccount<'info>,
    /// CHECK: fixed address
    #[account(address = LOCK_PROGRAM_ID)]
    pub lock_program: UncheckedAccount<'info>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawCreatorFees<'info> {
    #[account(address = creator_vault.payout)]
    pub payout: Signer<'info>,
    #[account(mut, seeds = [CREATOR_VAULT_SEED, creator_vault.mint.as_ref()], bump = creator_vault.bump)]
    pub creator_vault: Box<Account<'info, CreatorVault>>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Box<Account<'info, Global>>,
    #[account(seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = authority, associated_token::token_program = token_program)]
    pub authority_quote_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Any account for the pair token the payout wallet picks.
    #[account(mut, token::mint = quote_mint, token::token_program = token_program)]
    pub destination: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = creator_vault.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct SetPayoutWallet<'info> {
    #[account(address = creator_vault.payout)]
    pub payout: Signer<'info>,
    #[account(mut, seeds = [CREATOR_VAULT_SEED, creator_vault.mint.as_ref()], bump = creator_vault.bump)]
    pub creator_vault: Box<Account<'info, CreatorVault>>,
    /// For the treasury, which takes over without accepting.
    #[account(seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Box<Account<'info, Global>>,
}

#[derive(Accounts)]
pub struct AcceptPayoutWallet<'info> {
    pub new_payout: Signer<'info>,
    #[account(mut, seeds = [CREATOR_VAULT_SEED, creator_vault.mint.as_ref()], bump = creator_vault.bump)]
    pub creator_vault: Box<Account<'info, CreatorVault>>,
}

#[derive(Accounts)]
pub struct WithdrawTreasury<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [GLOBAL_SEED], bump = global.bump, has_one = admin, has_one = treasury)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [PAIR_SEED, pair.quote_mint.as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, Pair>>,
    /// CHECK: must be `global.treasury`
    pub treasury: UncheckedAccount<'info>,
    #[account(seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = authority, associated_token::token_program = token_program)]
    pub authority_quote_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = treasury, associated_token::token_program = token_program)]
    pub destination: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = pair.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = pair.quote_token_program)]
    pub token_program: Interface<'info, TokenInterface>,
}
