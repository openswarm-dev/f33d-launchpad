use anchor_lang::prelude::*;
use anchor_spl::associated_token::{self, get_associated_token_address_with_program_id};
use anchor_spl::token_2022::spl_token_2022::{
    extension::StateWithExtensions,
    state::{Account as SplAccount, Mint as SplMint},
};
use anchor_spl::token_interface;

use crate::errors::LaunchpadError;

/// Creates `ata` for `owner` if it doesn't exist.
#[allow(clippy::too_many_arguments)]
pub fn create_ata<'info>(
    payer: &AccountInfo<'info>,
    ata: &AccountInfo<'info>,
    owner: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    ata_program: &AccountInfo<'info>,
) -> Result<()> {
    require_ata(ata, owner.key, mint.key, token_program.key)?;
    associated_token::create_idempotent(CpiContext::new(
        ata_program.key(),
        associated_token::Create {
            payer: payer.clone(),
            associated_token: ata.clone(),
            authority: owner.clone(),
            mint: mint.clone(),
            system_program: system_program.clone(),
            token_program: token_program.clone(),
        },
    ))
}

pub fn require_ata(ata: &AccountInfo, owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Result<()> {
    let expected = get_associated_token_address_with_program_id(owner, mint, token_program);
    require_keys_eq!(*ata.key, expected, LaunchpadError::InvalidTokenAccount);
    Ok(())
}

/// Balance of a token account; 0 if it doesn't exist yet.
pub fn token_amount(account: &AccountInfo) -> Result<u64> {
    if account.data_is_empty() {
        return Ok(0);
    }
    let data = account.try_borrow_data()?;
    Ok(StateWithExtensions::<SplAccount>::unpack(&data)?.base.amount)
}

pub fn mint_decimals(mint: &AccountInfo) -> Result<u8> {
    let data = mint.try_borrow_data()?;
    Ok(StateWithExtensions::<SplMint>::unpack(&data)?.base.decimals)
}

pub fn mint_supply(mint: &AccountInfo) -> Result<u64> {
    let data = mint.try_borrow_data()?;
    Ok(StateWithExtensions::<SplMint>::unpack(&data)?.base.supply)
}

/// `transfer_checked` signed by a PDA (SPL Token or Token-2022).
pub fn transfer_tokens<'info>(
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    signer_seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            token_interface::TransferChecked { from: from.clone(), mint: mint.clone(), to: to.clone(), authority: authority.clone() },
            signer_seeds,
        ),
        amount,
        mint_decimals(mint)?,
    )
}

/// Burns `amount` from a PDA-owned token account.
pub fn burn_tokens<'info>(
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    signer_seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    token_interface::burn(
        CpiContext::new_with_signer(
            token_program.key(),
            token_interface::Burn { mint: mint.clone(), from: from.clone(), authority: authority.clone() },
            signer_seeds,
        ),
        amount,
    )
}

/// `transfer_checked` from an account whose owner signed this transaction (a wallet, not a PDA).
pub fn transfer_tokens_signed<'info>(
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    owner: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    require!(owner.is_signer, ErrorCode::AccountNotSigner);
    transfer_tokens(from, to, mint, owner, token_program, &[], amount)
}

/// Burns `amount` from an account whose owner signed this transaction (a wallet, not a PDA).
pub fn burn_tokens_signed<'info>(
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    owner: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    require!(owner.is_signer, ErrorCode::AccountNotSigner);
    burn_tokens(from, mint, owner, token_program, &[], amount)
}

pub fn pubkey_at(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::new_from_array(data[offset..offset + 32].try_into().unwrap())
}
