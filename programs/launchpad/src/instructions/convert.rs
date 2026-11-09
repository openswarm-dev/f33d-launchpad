//! Swapping a pair token's fees to SOL, so every payout is in SOL whatever a coin trades against. Coins can pair with
//! any token LaunchLab has a config for; their fees arrive in that token and wait whole in its pair's `pending`
//! (claim_pair_fees, collect_lp_fees). The payout bot (the developer wallet) converts them in one transaction:
//!
//!   convert_begin(amount)   the authority moves `amount` of the pair token to the dev's own account for it
//!   <the dev's swap>        any route (Jupiter) from the pair token into the dev's wrapped-SOL account
//!   convert_end(min_out)    the dev's pair-token balance must be back where it was (anything unspent returns to the
//!                           authority), and the SOL it gained, at least `min_out`, goes to the authority's wrapped-SOL
//!                           account; the dev's wrapped-SOL balance must be back where it was too
//!
//! Each half checks the other is in the same transaction (instructions sysvar), so the pair token never stays with
//! the dev. The SOL is booked into the SOL pair exactly like a SOL fee claim: split by `global.split`, the per-coin
//! part (creator and holders) into its pending for attribution. The dev picks the route and `min_out`, as it picks
//! attribution amounts: a leaked dev key is stopped with the admin's `payouts_paused`.
use anchor_lang::prelude::*;
use solana_instructions_sysvar::{load_current_index_checked, load_instruction_at_checked};
use anchor_lang::Discriminator;
use anchor_spl::associated_token::{get_associated_token_address_with_program_id, AssociatedToken};
use anchor_spl::token::{spl_token::native_mint, Token};
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::errors::LaunchpadError;
use crate::state::*;
use crate::token_utils::{create_ata, token_amount, transfer_tokens, transfer_tokens_signed};

pub const CONVERT_SEED: &[u8] = b"convert";

/// A conversion in flight: what convert_end checks the dev's balances against. Lives within one transaction.
#[account]
#[derive(InitSpace)]
pub struct ConvertTicket {
    pub pair: Pubkey,
    pub amount: u64,
    /// The dev's pair-token and wrapped-SOL balances before the conversion.
    pub quote_before: u64,
    pub sol_before: u64,
    pub bump: u8,
}

/// Whether this transaction has one of our `name` instructions before (`before`) or after the current one.
fn has_partner(sysvar: &AccountInfo, discriminator: &[u8], before: bool) -> Result<bool> {
    let current = load_current_index_checked(sysvar)? as usize;
    let mut i = if before { 0 } else { current + 1 };
    loop {
        if before && i >= current {
            return Ok(false);
        }
        let ix = match load_instruction_at_checked(i, sysvar) {
            Ok(ix) => ix,
            Err(_) => return Ok(false),
        };
        if ix.program_id == crate::ID && ix.data.len() >= 8 && ix.data[..8] == *discriminator {
            return Ok(true);
        }
        i += 1;
    }
}

pub fn convert_begin(ctx: Context<ConvertBegin>, amount: u64) -> Result<()> {
    require!(
        has_partner(&ctx.accounts.instructions.to_account_info(), crate::instruction::ConvertEnd::DISCRIMINATOR, false)?,
        LaunchpadError::ConvertUnpaired
    );
    let a = &ctx.accounts;
    require!(amount > 0 && amount <= a.pair.pending, LaunchpadError::ExceedsPending);
    for (ata, mint, program) in [
        (a.dev_quote_ata.to_account_info(), a.quote_mint.to_account_info(), a.quote_token_program.to_account_info()),
        (a.dev_wsol_ata.to_account_info(), a.native_mint.to_account_info(), a.token_program.to_account_info()),
    ] {
        create_ata(
            &a.distributor.to_account_info(), &ata, &a.distributor.to_account_info(), &mint,
            &a.system_program.to_account_info(), &program, &a.associated_token_program.to_account_info(),
        )?;
    }
    let quote_before = token_amount(&a.dev_quote_ata.to_account_info())?;
    let sol_before = token_amount(&a.dev_wsol_ata.to_account_info())?;
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[a.global.authority_bump]];
    transfer_tokens(
        &a.authority_quote_ata.to_account_info(), &a.dev_quote_ata.to_account_info(), &a.quote_mint.to_account_info(),
        &a.authority.to_account_info(), &a.quote_token_program.to_account_info(), &[seeds], amount,
    )?;
    let ticket = &mut ctx.accounts.ticket;
    ticket.pair = ctx.accounts.pair.key();
    ticket.amount = amount;
    ticket.quote_before = quote_before;
    ticket.sol_before = sol_before;
    ticket.bump = ctx.bumps.ticket;
    Ok(())
}

pub fn convert_end(ctx: Context<ConvertEnd>, min_out: u64) -> Result<()> {
    require!(
        has_partner(&ctx.accounts.instructions.to_account_info(), crate::instruction::ConvertBegin::DISCRIMINATOR, true)?,
        LaunchpadError::ConvertUnpaired
    );
    let a = &ctx.accounts;
    let t = &a.ticket;
    let dev = a.distributor.to_account_info();
    // What the swap took of the pair token (the dev's own balance must not be used), and what's left goes back.
    let quote_now = token_amount(&a.dev_quote_ata.to_account_info())?;
    let (spent, refund) = flagship_leg(t.quote_before, t.amount, quote_now).ok_or(LaunchpadError::Overspent)?;
    transfer_tokens_signed(
        &a.dev_quote_ata.to_account_info(), &a.authority_quote_ata.to_account_info(), &a.quote_mint.to_account_info(),
        &dev, &a.quote_token_program.to_account_info(), refund,
    )?;
    require!(token_amount(&a.dev_quote_ata.to_account_info())? == t.quote_before, LaunchpadError::DevBalanceChanged);
    create_ata(
        &dev, &a.authority_wsol_ata.to_account_info(), &a.authority.to_account_info(), &a.native_mint.to_account_info(),
        &a.system_program.to_account_info(), &a.token_program.to_account_info(), &a.associated_token_program.to_account_info(),
    )?;
    // What it bought: all of it goes to the authority.
    let sol_now = token_amount(&a.dev_wsol_ata.to_account_info())?;
    let sol = sol_now.checked_sub(t.sol_before).ok_or(LaunchpadError::DevBalanceChanged)?;
    require!(sol > 0 && sol >= min_out, LaunchpadError::ConvertTooLittle);
    transfer_tokens_signed(
        &a.dev_wsol_ata.to_account_info(), &a.authority_wsol_ata.to_account_info(), &a.native_mint.to_account_info(),
        &dev, &a.token_program.to_account_info(), sol,
    )?;
    require!(token_amount(&a.dev_wsol_ata.to_account_info())? == t.sol_before, LaunchpadError::DevBalanceChanged);

    let quote_mint = ctx.accounts.pair.quote_mint;
    ctx.accounts.pair.pending -= spent;
    let global_split = ctx.accounts.global.split;
    let sol_pair = &mut ctx.accounts.sol_pair;
    sol_pair.collected += sol;
    if sol_pair.pending == 0 {
        sol_pair.pending_split = global_split;
    }
    let s = sol_pair.pending_split.apply(sol);
    let pending = s.creator + s.holders;
    sol_pair.pending += pending;
    sol_pair.flagship_in += s.flagship;
    sol_pair.traders_in += s.traders;
    sol_pair.treasury_in += s.treasury;
    emit!(FeesConverted { quote_mint, spent, sol, pending, flagship: s.flagship, traders: s.traders, treasury: s.treasury });
    Ok(())
}

#[derive(Accounts)]
pub struct ConvertBegin<'info> {
    #[account(mut)]
    pub distributor: Signer<'info>,
    #[account(
        seeds = [GLOBAL_SEED], bump = global.bump, has_one = distributor,
        constraint = !global.payouts_paused @ LaunchpadError::PayoutsPaused,
    )]
    pub global: Box<Account<'info, Global>>,
    /// A pair other than SOL (SOL needs no converting).
    #[account(
        seeds = [PAIR_SEED, quote_mint.key().as_ref()], bump = pair.bump,
        constraint = pair.quote_mint != native_mint::ID @ LaunchpadError::InvalidPool,
    )]
    pub pair: Box<Account<'info, Pair>>,
    #[account(init, payer = distributor, space = 8 + ConvertTicket::INIT_SPACE, seeds = [CONVERT_SEED], bump)]
    pub ticket: Box<Account<'info, ConvertTicket>>,
    #[account(seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = authority, associated_token::token_program = quote_token_program)]
    pub authority_quote_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the dev's associated account for the pair token, created here if needed
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(distributor.key, &quote_mint.key(), quote_token_program.key) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub dev_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the dev's wrapped-SOL account the swap pays into, created here if needed
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(distributor.key, &native_mint::ID, &anchor_spl::token::ID) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub dev_wsol_ata: UncheckedAccount<'info>,
    #[account(address = pair.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = native_mint::ID)]
    pub native_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = pair.quote_token_program)]
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    /// CHECK: the instructions sysvar (the matching convert_end must follow)
    #[account(address = solana_instructions_sysvar::ID)]
    pub instructions: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ConvertEnd<'info> {
    #[account(mut)]
    pub distributor: Signer<'info>,
    #[account(
        seeds = [GLOBAL_SEED], bump = global.bump, has_one = distributor,
        constraint = !global.payouts_paused @ LaunchpadError::PayoutsPaused,
    )]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [PAIR_SEED, quote_mint.key().as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, Pair>>,
    #[account(mut, seeds = [PAIR_SEED, native_mint::ID.as_ref()], bump = sol_pair.bump)]
    pub sol_pair: Box<Account<'info, Pair>>,
    #[account(mut, seeds = [CONVERT_SEED], bump = ticket.bump, has_one = pair, close = distributor)]
    pub ticket: Box<Account<'info, ConvertTicket>>,
    #[account(seeds = [AUTHORITY_SEED], bump = global.authority_bump)]
    pub authority: SystemAccount<'info>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = authority, associated_token::token_program = quote_token_program)]
    pub authority_quote_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the authority's wrapped-SOL account, created here if needed (before the first SOL claim it may not exist)
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(authority.key, &native_mint::ID, &anchor_spl::token::ID) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub authority_wsol_ata: UncheckedAccount<'info>,
    /// CHECK: the dev's associated account for the pair token
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(distributor.key, &quote_mint.key(), quote_token_program.key) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub dev_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the dev's wrapped-SOL account
    #[account(
        mut,
        address = get_associated_token_address_with_program_id(distributor.key, &native_mint::ID, &anchor_spl::token::ID) @ LaunchpadError::InvalidTokenAccount,
    )]
    pub dev_wsol_ata: UncheckedAccount<'info>,
    #[account(address = pair.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = native_mint::ID)]
    pub native_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = pair.quote_token_program)]
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    /// CHECK: the instructions sysvar (the matching convert_begin must come first)
    #[account(address = solana_instructions_sysvar::ID)]
    pub instructions: UncheckedAccount<'info>,
}
