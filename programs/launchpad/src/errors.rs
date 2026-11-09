use anchor_lang::prelude::*;

#[error_code]
pub enum LaunchpadError {
    #[msg("Launches are paused")]
    Paused,
    #[msg("Launch must be co-signed by the attestor")]
    MissingAttestor,
    #[msg("Only the pending admin can accept")]
    NotPendingAdmin,
    #[msg("X record account is required when an X handle hash is given")]
    MissingXRecord,
    #[msg("Nothing to withdraw")]
    NothingToWithdraw,
    #[msg("Token account is not the expected associated token account")]
    InvalidTokenAccount,
    #[msg("The fee split must add up to 100%")]
    InvalidSplit,
    #[msg("Amount exceeds the treasury balance")]
    InsufficientTreasury,
    #[msg("Launches paired with this token are switched off")]
    PairDisabled,
    #[msg("Not LaunchLab's global config for this token")]
    InvalidLaunchConfig,
    #[msg("The platform is already set up")]
    PlatformExists,
    #[msg("The platform isn't set up yet")]
    NoPlatform,
    #[msg("Not our LaunchLab platform config")]
    InvalidPlatform,
    #[msg("Not this coin's pool")]
    InvalidPool,
    #[msg("Not the locked LP of this coin's graduated pool")]
    InvalidLpLock,
    #[msg("More than the pending fees")]
    ExceedsPending,
    #[msg("More than the pool holds")]
    ExceedsPool,
    #[msg("Only the program's upgrade authority can set up Global")]
    NotUpgradeAuthority,
    #[msg("This fee level isn't open for new launches")]
    FeeTierDisabled,
    #[msg("Only the pending distributor can accept")]
    NotPendingDistributor,
    #[msg("The flagship must be launched by the distributor (the developer wallet)")]
    NotFlagshipCreator,
    #[msg("The developer wallet's own balance would change")]
    DevBalanceChanged,
    #[msg("The buy took more than it was given")]
    Overspent,
    #[msg("Payouts are paused")]
    PayoutsPaused,
    #[msg("Over the pair's outflow limit for this window")]
    OutflowLimit,
    #[msg("Only the proposed payout wallet can accept")]
    NotPendingPayout,
    #[msg("The treasury can't be the default address")]
    InvalidTreasury,
    #[msg("The outflow window must be at least a minute")]
    InvalidWindow,
    #[msg("convert_begin and convert_end must be in the same transaction")]
    ConvertUnpaired,
    #[msg("The swap returned less SOL than the minimum")]
    ConvertTooLittle,
}
