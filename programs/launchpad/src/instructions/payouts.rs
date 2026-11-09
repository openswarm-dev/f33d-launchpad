//! The payout bot's instructions: attributing pooled curve fees to coins, paying holders, traders and
//! creators, and spending the flagship pool. Only `global.distributor` can call them; it is the developer
//! wallet that launched the flagship, so it signs (and pays for) every payout, buyback and burn. Each is
//! capped by the books: the bot decides *who* gets paid from the holder and trader pools, never how much a
//! pool holds; creators are only ever paid to their own payout wallet, and the treasury is out of reach.
//! Payouts come from the authority's token accounts, not from the dev's. Two more bounds for a leaked dev key:
//! the admin's `payouts_paused` switch refuses every one of these instructions, and each pair's outflow limit
//! caps what leaves its pools at the bot's discretion (creator credits, holder and trader payouts, flagship
//! buys) per window.
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{instruction::Instruction, program::invoke};
use anchor_spl::associated_token::{get_associated_token_address_with_program_id, AssociatedToken};
use anchor_spl::token::Token;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::errors::LaunchpadError;
use crate::instructions::fees::CPMM_PROGRAM_ID;
use crate::raydium_launchpad::{self, accounts::PoolState, program::RaydiumLaunchpad};
use crate::state::*;
use crate::token_utils::{burn_tokens_signed, create_ata, token_amount, transfer_tokens, transfer_tokens_signed};

/// CPMM `swap_base_input` (CPMM publishes no IDL).
const SWAP_BASE_INPUT: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];
/// A LaunchLab curve's `status` once it has migrated to its CPMM pool (0 is trading, 1 is completed).
const LAUNCHLAB_MIGRATED: u8 = 2;

/// Attributes a coin's curve fees, which arrived unlabelled in the pair's pending pool. `coin_fee` is the
/// platform fee the coin's trades paid (from LaunchLab's trade events); its per-coin shares (creator and
/// holders) move from pending to the coin, divided by the split they were claimed under. Can't move more than
/// is pending, and the creator's part counts against the pair's outflow limit (the payout wallet can withdraw
/// it at once).
/// Every coin's fees are attributed in SOL, from the SOL pair's pending: for a coin on another pair token, `coin_fee` is
/// the SOL its fees converted to, and `source_amount` the pair-token fees that covers (for the books; equal for SOL coins).
pub fn attribute_fees(ctx: Context<AttributeFees>, coin_fee: u64, source_amount: u64) -> Result<()> {
    let pair = &mut ctx.accounts.pair;
    let s = pair.pending_split.apply(coin_fee);
    let per_coin = s.creator + s.holders;
    require!(per_coin <= pair.pending, LaunchpadError::ExceedsPending);
    pair.consume_outflow(s.creator, Clock::get()?.unix_timestamp)?;
    pair.pending -= per_coin;
    ctx.accounts.creator_vault.harvested += s.creator;
    let launch = &mut ctx.accounts.launch;
    launch.holders_in += s.holders;
    launch.fees += coin_fee;
    emit!(FeesAttributed { mint: launch.mint, coin_fee, creator: s.creator, holders: s.holders, source_amount });
    Ok(())
}

/// Pending fees that came from pools that aren't ours (anyone can launch on our public platform config)
/// go to the treasury: no creator or holders of ours to pay.
pub fn release_pending(ctx: Context<ReleasePending>, amount: u64) -> Result<()> {
    let pair = &mut ctx.accounts.pair;
    require!(amount <= pair.pending, LaunchpadError::ExceedsPending);
    pair.pending -= amount;
    pair.treasury_in += amount;
    emit!(PendingReleased { quote_mint: pair.quote_mint, amount });
    Ok(())
}

fn pay<'info>(a: &PayAccounts<'info>, amount: u64) -> Result<()> {
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[a.global.authority_bump]];
    transfer_tokens(
        &a.authority_quote_ata.to_account_info(),
        &a.destination.to_account_info(),
        &a.quote_mint.to_account_info(),
        &a.authority.to_account_info(),
        &a.token_program.to_account_info(),
        &[seeds],
        amount,
    )
}

/// Pays one holder from a coin's holder pool.
pub fn pay_holders(ctx: Context<PayHolders>, amount: u64) -> Result<()> {
    let launch = &ctx.accounts.launch;
    require!(amount > 0 && amount <= launch.holders_in - launch.holders_out, LaunchpadError::ExceedsPool);
    ctx.accounts.pair.consume_outflow(amount, Clock::get()?.unix_timestamp)?;
    pay(&ctx.accounts.pay, amount)?;
    let launch = &mut ctx.accounts.launch;
    launch.holders_out += amount;
    emit!(HolderRewardPaid { mint: launch.mint, owner: ctx.accounts.pay.destination.owner, amount });
    Ok(())
}

/// Pays one trader from a pair's trader pool.
pub fn pay_traders(ctx: Context<PayTraders>, amount: u64) -> Result<()> {
    let pair = &ctx.accounts.pair;
    require!(amount > 0 && amount <= pair.traders_in - pair.traders_out, LaunchpadError::ExceedsPool);
    ctx.accounts.pair.consume_outflow(amount, Clock::get()?.unix_timestamp)?;
    pay(&ctx.accounts.pay, amount)?;
    let pair = &mut ctx.accounts.pair;
    pair.traders_out += amount;
    emit!(TraderRewardPaid { quote_mint: pair.quote_mint, owner: ctx.accounts.pay.destination.owner, amount });
    Ok(())
}

/// Pushes a coin's creator earnings to its payout wallet: everything earned and not yet paid, from the same
/// books `withdraw_creator_fees` uses (the payout wallet can still pull them itself). Only the distributor
/// sends it, and only to the payout wallet's own associated account for the pair token (created here if
/// needed, at the distributor's cost): it decides when creators are paid, never where to or how much.
pub fn pay_creator(ctx: Context<PayCreator>) -> Result<()> {
    let a = &ctx.accounts;
    let amount = a.creator_vault.harvested.saturating_sub(a.creator_vault.withdrawn);
    require!(amount > 0, LaunchpadError::NothingToWithdraw);
    create_ata(
        &a.distributor.to_account_info(),
        &a.destination,
        &a.payout,
        &a.quote_mint.to_account_info(),
        &a.system_program.to_account_info(),
        &a.token_program.to_account_info(),
        &a.associated_token_program.to_account_info(),
    )?;
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[a.global.authority_bump]];
    transfer_tokens(
        &a.authority_quote_ata.to_account_info(),
        &a.destination,
        &a.quote_mint.to_account_info(),
        &a.authority.to_account_info(),
        &a.token_program.to_account_info(),
        &[seeds],
        amount,
    )?;
    let pusher = a.distributor.key();
    let vault = &mut ctx.accounts.creator_vault;
    vault.withdrawn += amount;
    emit!(CreatorPaid { mint: vault.mint, payout: vault.payout, amount, pusher });
    Ok(())
}

// The flagship buy and burn is made by the developer wallet (the distributor, which launched the flagship),
// so that terminals show the dev buying and burning. In one instruction: the authority moves the flagship
// pool's funds to the dev's own pair-token account, the dev buys with them into its own flagship account,
// anything the buy didn't spend goes back to the authority, and the dev burns exactly what it bought. The
// dev's own balances of both tokens must end where they started, so it can neither keep any of the pool
// nor have its own funds or tokens used.
//
// TODO: the flagship share of a pair other than the flagship's own (e.g. USDC fees when the flagship trades
// against SOL) can't be spent yet: these instructions only take the flagship's pair. Converting it needs a
// swap into the flagship's pair token on a pool the admin pins (the bot picks the price limit, so an open
// choice of pool would let it trade the share away), which is left for a separate change. The share stays
// booked in `flagship_in` meanwhile.

/// The developer wallet's side of a flagship buy, and where the flagship pool's funds sit.
struct DevLeg<'info> {
    dev: AccountInfo<'info>,
    dev_quote_ata: AccountInfo<'info>,
    dev_flagship_ata: AccountInfo<'info>,
    authority: AccountInfo<'info>,
    authority_quote_ata: AccountInfo<'info>,
    quote_mint: AccountInfo<'info>,
    flagship_mint: AccountInfo<'info>,
    token_program: AccountInfo<'info>,
    base_token_program: AccountInfo<'info>,
    system_program: AccountInfo<'info>,
    associated_token_program: AccountInfo<'info>,
    authority_bump: u8,
}

/// The dev's two balances before the flagship pool's funds arrive.
struct DevBefore {
    quote: u64,
    flagship: u64,
}

macro_rules! dev_leg {
    ($a:expr) => {
        DevLeg {
            dev: $a.distributor.to_account_info(),
            dev_quote_ata: $a.dev_quote_ata.to_account_info(),
            dev_flagship_ata: $a.dev_flagship_ata.to_account_info(),
            authority: $a.authority.to_account_info(),
            authority_quote_ata: $a.authority_quote_ata.to_account_info(),
            quote_mint: $a.quote_mint.to_account_info(),
            flagship_mint: $a.flagship_mint.to_account_info(),
            token_program: $a.token_program.to_account_info(),
            base_token_program: $a.base_token_program.to_account_info(),
            system_program: $a.system_program.to_account_info(),
            associated_token_program: $a.associated_token_program.to_account_info(),
            authority_bump: $a.global.authority_bump,
        }
    };
}

impl<'info> DevLeg<'info> {
    /// Creates the dev's two accounts if needed (the dev pays the rent), notes their balances, and moves
    /// `amount` of the flagship pool from the authority to the dev's pair-token account.
    fn fund(&self, amount: u64) -> Result<DevBefore> {
        for (ata, mint, program) in [
            (&self.dev_quote_ata, &self.quote_mint, &self.token_program),
            (&self.dev_flagship_ata, &self.flagship_mint, &self.base_token_program),
        ] {
            create_ata(&self.dev, ata, &self.dev, mint, &self.system_program, program, &self.associated_token_program)?;
        }
        let before = DevBefore { quote: token_amount(&self.dev_quote_ata)?, flagship: token_amount(&self.dev_flagship_ata)? };
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[self.authority_bump]];
        transfer_tokens(&self.authority_quote_ata, &self.dev_quote_ata, &self.quote_mint, &self.authority, &self.token_program, &[seeds], amount)?;
        Ok(before)
    }

    /// After the buy: returns what it didn't spend (the dev signs), checks the dev's pair-token balance is
    /// back where it started, burns exactly what was bought (the dev signs), checks the dev's flagship
    /// balance is back too, and books it.
    fn settle(&self, pair: &mut Pair, before: DevBefore, amount: u64) -> Result<()> {
        let (spent, refund) = flagship_leg(before.quote, amount, token_amount(&self.dev_quote_ata)?).ok_or(LaunchpadError::Overspent)?;
        transfer_tokens_signed(&self.dev_quote_ata, &self.authority_quote_ata, &self.quote_mint, &self.dev, &self.token_program, refund)?;
        // Only what was spent counts against the outflow limit (the whole amount was counted up front).
        pair.outflow_used = pair.outflow_used.saturating_sub(refund);
        require!(token_amount(&self.dev_quote_ata)? == before.quote, LaunchpadError::DevBalanceChanged);
        let bought = token_amount(&self.dev_flagship_ata)?.checked_sub(before.flagship).ok_or(LaunchpadError::DevBalanceChanged)?;
        burn_tokens_signed(&self.dev_flagship_ata, &self.flagship_mint, &self.dev, &self.base_token_program, bought)?;
        require!(token_amount(&self.dev_flagship_ata)? == before.flagship, LaunchpadError::DevBalanceChanged);
        pair.flagship_out += spent;
        pair.flagship_burned += bought;
        emit!(FlagshipBought { quote_mint: pair.quote_mint, buyer: self.dev.key(), spent, burned: bought });
        Ok(())
    }
}

/// Spends up to `amount` of a pair's flagship pool: the developer wallet buys the flagship on its own bonding
/// curve and burns every token bought (see above). The flagship must have been launched through us by the
/// distributor, paired with this pair's token. If the buy completes the curve, LaunchLab takes only what it
/// needs and the rest goes back to the pool.
pub fn spend_flagship(ctx: Context<SpendFlagship>, amount: u64, min_out: u64) -> Result<()> {
    // The whole amount counts against the outflow limit; settle() gives back what the buy didn't spend.
    ctx.accounts.pair.consume_outflow(amount, Clock::get()?.unix_timestamp)?;
    let a = &ctx.accounts;
    require!(amount > 0 && amount <= a.pair.flagship_in - a.pair.flagship_out, LaunchpadError::ExceedsPool);
    let leg = dev_leg!(a);
    let before = leg.fund(amount)?;
    // The dev signed this transaction, and that signature carries into LaunchLab: it is the buyer.
    raydium_launchpad::cpi::buy_exact_in(
        CpiContext::new(
            a.launchlab_program.key(),
            raydium_launchpad::cpi::accounts::BuyExactIn {
                payer: leg.dev.clone(),
                authority: a.launchlab_authority.to_account_info(),
                global_config: a.launch_config.to_account_info(),
                platform_config: a.platform_config.to_account_info(),
                pool_state: a.pool_state.to_account_info(),
                user_base_token: leg.dev_flagship_ata.clone(),
                user_quote_token: leg.dev_quote_ata.clone(),
                base_vault: a.base_vault.to_account_info(),
                quote_vault: a.quote_vault.to_account_info(),
                base_token_mint: a.flagship_mint.to_account_info(),
                quote_token_mint: a.quote_mint.to_account_info(),
                base_token_program: a.base_token_program.to_account_info(),
                quote_token_program: a.token_program.to_account_info(),
                event_authority: a.launchlab_event_authority.to_account_info(),
                program: a.launchlab_program.to_account_info(),
            },
        )
        // LaunchLab takes these beyond its IDL: the system program, then the platform and creator fee vaults.
        .with_remaining_accounts(vec![
            a.system_program.to_account_info(),
            a.platform_fee_vault.to_account_info(),
            a.creator_fee_vault.to_account_info(),
        ]),
        amount,
        min_out,
        0,
    )?;
    leg.settle(&mut ctx.accounts.pair, before, amount)
}

/// The same once the flagship has graduated: the developer wallet buys on its CPMM pool and burns.
pub fn spend_flagship_cpmm(ctx: Context<SpendFlagshipCpmm>, amount: u64, min_out: u64) -> Result<()> {
    // The whole amount counts against the outflow limit; settle() gives back what the buy didn't spend.
    ctx.accounts.pair.consume_outflow(amount, Clock::get()?.unix_timestamp)?;
    let a = &ctx.accounts;
    require!(amount > 0 && amount <= a.pair.flagship_in - a.pair.flagship_out, LaunchpadError::ExceedsPool);
    let (flagship, quote) = (a.flagship_mint.key(), a.quote_mint.key());
    let (token0, token1) = if flagship < quote { (flagship, quote) } else { (quote, flagship) };
    let pool = Pubkey::find_program_address(
        &[b"pool", a.fee_tier.cpmm_config.as_ref(), token0.as_ref(), token1.as_ref()],
        &CPMM_PROGRAM_ID,
    )
    .0;
    require_keys_eq!(a.cpmm_pool.key(), pool, LaunchpadError::InvalidPool);
    let leg = dev_leg!(a);
    let before = leg.fund(amount)?;
    let mut data = SWAP_BASE_INPUT.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    // The dev is the swap's owner and signer, trading from and into its own accounts.
    let metas = vec![
        AccountMeta::new_readonly(leg.dev.key(), true),
        AccountMeta::new_readonly(a.cpmm_authority.key(), false),
        AccountMeta::new_readonly(a.amm_config.key(), false),
        AccountMeta::new(pool, false),
        AccountMeta::new(leg.dev_quote_ata.key(), false),
        AccountMeta::new(leg.dev_flagship_ata.key(), false),
        AccountMeta::new(a.quote_vault.key(), false),
        AccountMeta::new(a.flagship_vault.key(), false),
        AccountMeta::new_readonly(a.token_program.key(), false),
        AccountMeta::new_readonly(a.base_token_program.key(), false),
        AccountMeta::new_readonly(quote, false),
        AccountMeta::new_readonly(flagship, false),
        AccountMeta::new(a.observation.key(), false),
    ];
    invoke(
        &Instruction { program_id: CPMM_PROGRAM_ID, accounts: metas, data },
        &[
            leg.dev.clone(),
            a.cpmm_authority.to_account_info(),
            a.amm_config.to_account_info(),
            a.cpmm_pool.to_account_info(),
            leg.dev_quote_ata.clone(),
            leg.dev_flagship_ata.clone(),
            a.quote_vault.to_account_info(),
            a.flagship_vault.to_account_info(),
            a.token_program.to_account_info(),
            a.base_token_program.to_account_info(),
            a.quote_mint.to_account_info(),
            a.flagship_mint.to_account_info(),
            a.observation.to_account_info(),
            a.cpmm_program.to_account_info(),
        ],
    )?;
    leg.settle(&mut ctx.accounts.pair, before, amount)
}

#[derive(Accounts)]
pub struct AttributeFees<'info> {
    pub distributor: Signer<'info>,
    #[account(
        seeds = [GLOBAL_SEED], bump = global.bump, has_one = distributor,
        constraint = !global.payouts_paused @ LaunchpadError::PayoutsPaused,
    )]
    pub global: Box<Account<'info, Global>>,
    /// The SOL pair: every coin's fees are attributed in SOL.
    #[account(mut, seeds = [PAIR_SEED, anchor_spl::token::spl_token::native_mint::ID.as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, Pair>>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, LaunchRecord>>,
    #[account(mut, seeds = [CREATOR_VAULT_SEED, launch.mint.as_ref()], bump = creator_vault.bump)]
    pub creator_vault: Box<Account<'info, CreatorVault>>,
}

#[derive(Accounts)]
pub struct ReleasePending<'info> {
    pub distributor: Signer<'info>,
    #[account(
        seeds = [GLOBAL_SEED], bump = global.bump, has_one = distributor,
        constraint = !global.payouts_paused @ LaunchpadError::PayoutsPaused,
    )]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [PAIR_SEED, pair.quote_mint.as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, Pair>>,
}

/// Paying out of the authority's token account for a pair token, to any account for that token.
#[derive(Accounts)]
pub struct PayAccounts<'info> {
    pub distributor: Signer<'info>,
    #[account(
        seeds = [GLOBAL_SEED], bump = global.bump, has_one = distributor,
        constraint = !global.payouts_paused @ LaunchpadError::PayoutsPaused,
    )]
    pub global: Box<Account<'info, Global>>,
    #[account(seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = authority, associated_token::token_program = token_program)]
    pub authority_quote_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = quote_mint, token::token_program = token_program)]
    pub destination: Box<InterfaceAccount<'info, TokenAccount>>,
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct PayHolders<'info> {
    pub pay: PayAccounts<'info>,
    #[account(
        mut,
        seeds = [LAUNCH_SEED, launch.mint.as_ref()],
        bump = launch.bump,
        // Holder pools are in SOL, whatever the coin trades against.
        constraint = pay.quote_mint.key() == anchor_spl::token::spl_token::native_mint::ID @ LaunchpadError::InvalidPool,
    )]
    pub launch: Box<Account<'info, LaunchRecord>>,
    /// The coin's pair, whose outflow limit the payout counts against.
    #[account(mut, seeds = [PAIR_SEED, pay.quote_mint.key().as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, Pair>>,
}

#[derive(Accounts)]
pub struct PayTraders<'info> {
    pub pay: PayAccounts<'info>,
    #[account(mut, seeds = [PAIR_SEED, pay.quote_mint.key().as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, Pair>>,
}

#[derive(Accounts)]
pub struct PayCreator<'info> {
    /// Pays the transaction and, if needed, the payout wallet's token account.
    #[account(mut)]
    pub distributor: Signer<'info>,
    #[account(
        seeds = [GLOBAL_SEED], bump = global.bump, has_one = distributor,
        constraint = !global.payouts_paused @ LaunchpadError::PayoutsPaused,
    )]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [CREATOR_VAULT_SEED, creator_vault.mint.as_ref()], bump = creator_vault.bump)]
    pub creator_vault: Box<Account<'info, CreatorVault>>,
    /// CHECK: the coin's payout wallet (pinned by the creator vault); it doesn't sign
    #[account(address = creator_vault.payout)]
    pub payout: UncheckedAccount<'info>,
    #[account(seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = authority, associated_token::token_program = token_program)]
    pub authority_quote_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the payout wallet's associated account for the pair token, created here if needed
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(payout.key, &quote_mint.key(), token_program.key) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub destination: UncheckedAccount<'info>,
    #[account(address = creator_vault.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SpendFlagship<'info> {
    /// The developer wallet: the distributor and the flagship's creator. It buys with its own token accounts
    /// and burns what it bought, and pays for the transaction and any account rent.
    #[account(mut)]
    pub distributor: Signer<'info>,
    #[account(
        seeds = [GLOBAL_SEED], bump = global.bump, has_one = distributor, has_one = flagship_mint,
        constraint = !global.payouts_paused @ LaunchpadError::PayoutsPaused,
    )]
    pub global: Box<Account<'info, Global>>,
    /// The flagship's fee level: its platform must be ours (LaunchLab checks it's the pool's).
    #[account(seeds = [FEE_TIER_SEED, &[fee_tier.index]], bump = fee_tier.bump, has_one = platform_config)]
    pub fee_tier: Box<Account<'info, FeeTier>>,
    /// The flagship's own pair: its curve trades against this token.
    #[account(mut, seeds = [PAIR_SEED, flagship_launch.quote_mint.as_ref()], bump = pair.bump, has_one = launch_config)]
    pub pair: Box<Account<'info, Pair>>,
    /// The flagship's launch record: the distributor must be the wallet that launched it.
    #[account(
        seeds = [LAUNCH_SEED, flagship_mint.key().as_ref()], bump = flagship_launch.bump,
        constraint = flagship_launch.creator == distributor.key() @ LaunchpadError::NotFlagshipCreator,
    )]
    pub flagship_launch: Box<Account<'info, LaunchRecord>>,
    #[account(seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    /// CHECK: the flagship token (pinned by global)
    #[account(mut)]
    pub flagship_mint: UncheckedAccount<'info>,
    #[account(address = flagship_launch.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = authority, associated_token::token_program = token_program)]
    pub authority_quote_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the dev's associated account for the pair token, created here if needed
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(distributor.key, &quote_mint.key(), token_program.key) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub dev_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the dev's associated flagship account, created here if needed
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(distributor.key, flagship_mint.key, &anchor_spl::token::ID) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub dev_flagship_ata: UncheckedAccount<'info>,
    /// CHECK: LaunchLab's config for the pair token (pinned by the pair)
    pub launch_config: UncheckedAccount<'info>,
    /// CHECK: the flagship's platform config (pinned by fee_tier)
    pub platform_config: UncheckedAccount<'info>,
    /// CHECK: checked by LaunchLab
    pub launchlab_authority: UncheckedAccount<'info>,
    /// CHECK: the flagship's curve
    #[account(mut, address = flagship_launch.pool @ LaunchpadError::InvalidPool)]
    pub pool_state: UncheckedAccount<'info>,
    /// CHECK: checked by LaunchLab
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: checked by LaunchLab
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: our platform fee vault for the pair token; checked by LaunchLab
    #[account(mut)]
    pub platform_fee_vault: UncheckedAccount<'info>,
    /// CHECK: the flagship creator's (the dev's) fee vault; checked by LaunchLab
    #[account(mut)]
    pub creator_fee_vault: UncheckedAccount<'info>,
    pub base_token_program: Program<'info, Token>,
    #[account(address = pair.quote_token_program)]
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: checked by LaunchLab
    pub launchlab_event_authority: UncheckedAccount<'info>,
    pub launchlab_program: Program<'info, RaydiumLaunchpad>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SpendFlagshipCpmm<'info> {
    /// The developer wallet, as in `SpendFlagship`.
    #[account(mut)]
    pub distributor: Signer<'info>,
    #[account(
        seeds = [GLOBAL_SEED], bump = global.bump, has_one = distributor, has_one = flagship_mint,
        constraint = !global.payouts_paused @ LaunchpadError::PayoutsPaused,
    )]
    pub global: Box<Account<'info, Global>>,
    /// The flagship's fee level (checked against its curve below): its CPMM config fixes the graduated pool's
    /// address.
    #[account(seeds = [FEE_TIER_SEED, &[fee_tier.index]], bump = fee_tier.bump)]
    pub fee_tier: Box<Account<'info, FeeTier>>,
    #[account(mut, seeds = [PAIR_SEED, flagship_launch.quote_mint.as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, Pair>>,
    /// The flagship's launch record: the distributor must be the wallet that launched it.
    #[account(
        seeds = [LAUNCH_SEED, flagship_mint.key().as_ref()], bump = flagship_launch.bump,
        constraint = flagship_launch.creator == distributor.key() @ LaunchpadError::NotFlagshipCreator,
    )]
    pub flagship_launch: Box<Account<'info, LaunchRecord>>,
    /// The flagship's LaunchLab curve. It must have migrated, and `fee_tier` must be the level it launched on,
    /// so the pool derived below is the one LaunchLab graduated it into. Anyone can open a CPMM pool for the
    /// same two tokens on another level's config (or on this one before migration), at any price, and a buy
    /// there would hand the flagship pool to whoever holds that pool's liquidity.
    #[account(
        address = flagship_launch.pool @ LaunchpadError::InvalidPool,
        constraint = pool_state.platform_config == fee_tier.platform_config @ LaunchpadError::InvalidPool,
        constraint = pool_state.status == LAUNCHLAB_MIGRATED @ LaunchpadError::InvalidPool,
    )]
    pub pool_state: Box<Account<'info, PoolState>>,
    #[account(seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    /// CHECK: the flagship token (pinned by global)
    #[account(mut)]
    pub flagship_mint: UncheckedAccount<'info>,
    #[account(address = flagship_launch.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = authority, associated_token::token_program = token_program)]
    pub authority_quote_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the dev's associated account for the pair token, created here if needed
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(distributor.key, &quote_mint.key(), token_program.key) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub dev_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the dev's associated flagship account, created here if needed
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(distributor.key, flagship_mint.key, &anchor_spl::token::ID) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub dev_flagship_ata: UncheckedAccount<'info>,
    /// CHECK: checked by CPMM
    pub cpmm_authority: UncheckedAccount<'info>,
    /// CHECK: the CPMM config of the flagship's fee level
    #[account(address = fee_tier.cpmm_config)]
    pub amm_config: UncheckedAccount<'info>,
    /// CHECK: the flagship's graduated pool (derived in the handler)
    #[account(mut)]
    pub cpmm_pool: UncheckedAccount<'info>,
    /// CHECK: checked by CPMM
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: checked by CPMM
    #[account(mut)]
    pub flagship_vault: UncheckedAccount<'info>,
    /// CHECK: checked by CPMM
    #[account(mut)]
    pub observation: UncheckedAccount<'info>,
    pub base_token_program: Program<'info, Token>,
    #[account(address = pair.quote_token_program)]
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: fixed address
    #[account(address = CPMM_PROGRAM_ID)]
    pub cpmm_program: UncheckedAccount<'info>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}
