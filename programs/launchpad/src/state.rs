use anchor_lang::prelude::*;

pub const GLOBAL_SEED: &[u8] = b"global";
pub const X_SEED: &[u8] = b"x";
pub const LAUNCH_SEED: &[u8] = b"launch";
pub const CREATOR_VAULT_SEED: &[u8] = b"creator_vault";
pub const PAIR_SEED: &[u8] = b"pair";
/// System-owned PDA (no data) that is our LaunchLab platform: its admin, fee wallet and LP-NFT wallet.
/// It holds every fee we collect, one token account per pair token (wrapped SOL included), and pays
/// rent for the accounts LaunchLab creates on its behalf, so it keeps a little SOL too.
pub const AUTHORITY_SEED: &[u8] = b"authority";
/// One per trading-fee level a creator can pick: the `FeeTier` account, and the system-owned PDA that is
/// that level's LaunchLab platform admin (it signs the platform's creation and updates, nothing else).
pub const FEE_TIER_SEED: &[u8] = b"fee_tier";
pub const TIER_ADMIN_SEED: &[u8] = b"tier_admin";

pub const BPS: u64 = 10_000;

/// Coins all launch with 1B tokens (6 decimals), 71.01% sold on the curve and the rest seeding the graduated pool.
/// That share makes the graduation market cap 6x the starting one; each pair's `fund_raising` sets the level
/// (390 SOL / 34,800 USDC: about $20k at launch and $120k at graduation, with SOL at $89).
pub const COIN_DECIMALS: u8 = 6;
pub const COIN_SUPPLY: u64 = 1_000_000_000_000_000;
pub const COIN_CURVE_SELL: u64 = 710_100_000_000_000;
/// LaunchLab migrate type: 1 = into a CPMM pool (0 would be the older AMM v4).
pub const MIGRATE_TO_CPMM: u8 = 1;

#[account]
#[derive(InitSpace)]
pub struct Global {
    pub admin: Pubkey,
    /// Set by `propose_admin`, takes over on `accept_admin`.
    pub pending_admin: Pubkey,
    /// Backend key that co-signs launches after its off-chain checks.
    pub attestor: Pubkey,
    pub attestor_required: bool,
    /// Wallet (multisig) that treasury withdrawals go to.
    pub treasury: Pubkey,
    /// The payout bot, which is also the developer wallet that launched the flagship: the only signer that
    /// can attribute pooled fees to coins, pay out the holder, trader and creator pools, and buy and burn
    /// the flagship. It can't touch the treasury, and it can only pay creators to their own payout wallets.
    /// Changed in two steps: `propose_distributor` (admin), then `accept_distributor` (the new key).
    pub distributor: Pubkey,
    /// How our share of every trade is divided.
    pub split: FeeSplit,
    /// Unused since fee tiers replaced the single platform (each `FeeTier` has its own); kept so the
    /// account layout doesn't change.
    pub legacy_platform_config: Pubkey,
    /// Set by `propose_distributor`, takes over on `accept_distributor`; default when none is proposed.
    /// (This slot held the unused `legacy_cpmm_config` before, so the account layout is unchanged.)
    pub pending_distributor: Pubkey,
    /// Our flagship token, bought and burned with the flagship pool; default until it exists. Set with
    /// `set_flagship`, only to a coin launched through us by the distributor (or the proposed one).
    pub flagship_mint: Pubkey,
    /// Gates new launches only (the keeper also skips payouts while it is set).
    pub paused: bool,
    pub authority_bump: u8,
    pub bump: u8,
    /// The admin's emergency stop for the payout bot: while set, every distributor instruction (attribution,
    /// release, holder, trader and creator payouts, flagship buys) is refused, so a leaked dev key can be frozen
    /// with one admin transaction. Fee claims and creators' own withdrawals keep working.
    pub payouts_paused: bool,
}

/// Shares of our fees in basis points, adding up to 10,000 (`FeeSplit::DEFAULT` is ours). The admin can change
/// it any time with `update_global`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace, PartialEq, Eq, Debug)]
pub struct FeeSplit {
    /// Credited to the coin's creator vault, claimable any time.
    pub creator: u16,
    /// The coin's holder pool, paid out pro rata by the payout bot.
    pub holders: u16,
    /// Per pair token; buys and burns our flagship token.
    pub flagship: u16,
    /// Per pair token; airdropped to traders by volume by the payout bot.
    pub traders: u16,
    /// Per pair token; withdrawn by the admin to the treasury wallet only.
    pub treasury: u16,
}

impl FeeSplit {
    /// Float's split, which we started from.
    pub const FLOAT: FeeSplit = FeeSplit { creator: 500, holders: 1_500, flagship: 3_500, traders: 3_500, treasury: 1_000 };
    /// Ours: creators 25% (5x Float's share), holders 20%, flagship 10%, traders 35%, treasury 10%.
    pub const DEFAULT: FeeSplit = FeeSplit { creator: 2_500, holders: 2_000, flagship: 1_000, traders: 3_500, treasury: 1_000 };

    pub fn is_valid(&self) -> bool {
        [self.creator, self.holders, self.flagship, self.traders, self.treasury].iter().map(|x| *x as u64).sum::<u64>() == BPS
    }

    /// The part paid per coin (creator and holders); the rest is per pair token.
    pub fn per_coin_bps(&self) -> u64 {
        self.creator as u64 + self.holders as u64
    }

    /// Divides `amount`; the treasury takes whatever rounding leaves over.
    pub fn apply(&self, amount: u64) -> SplitAmounts {
        let part = |bps: u16| (amount as u128 * bps as u128 / BPS as u128) as u64;
        let (creator, holders, flagship, traders) = (part(self.creator), part(self.holders), part(self.flagship), part(self.traders));
        SplitAmounts { creator, holders, flagship, traders, treasury: amount - creator - holders - flagship - traders }
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SplitAmounts {
    pub creator: u64,
    pub holders: u64,
    pub flagship: u64,
    pub traders: u64,
    pub treasury: u64,
}

/// A token coins can be paired with, and its books. All of a pair's funds sit in the authority's
/// token account for that token; these totals say whose they are.
#[account]
#[derive(InitSpace)]
pub struct Pair {
    pub quote_mint: Pubkey,
    /// SPL Token or Token-2022, whichever owns `quote_mint`.
    pub quote_token_program: Pubkey,
    /// LaunchLab's global config for this quote token (curve type 0, index 0).
    pub launch_config: Pubkey,
    /// What a coin's curve must raise to graduate, in the pair token's base units.
    pub fund_raising: u64,
    /// Gates new launches only; coins already paired keep working.
    pub enabled: bool,
    /// All fees collected for this pair, from every source.
    pub collected: u64,
    /// Per-coin shares (creator + holders) of curve fees, which arrive unlabelled in LaunchLab's pooled
    /// fee vault, waiting for the payout bot to attribute them to coins.
    pub pending: u64,
    pub flagship_in: u64,
    pub flagship_out: u64,
    /// Flagship tokens bought and burned with this pair's flagship pool.
    pub flagship_burned: u64,
    pub traders_in: u64,
    pub traders_out: u64,
    pub treasury_in: u64,
    pub treasury_out: u64,
    pub bump: u8,
    /// The split `pending` was booked under. Curve-fee claims keep using it while `pending` is non-zero and
    /// take the current `global.split` once it is empty; attribution always uses it. So a split change never
    /// re-divides fees that were already claimed (the keeper empties `pending` to let a new split take effect).
    pub pending_split: FeeSplit,
    /// At most this much (pair base units) can leave the pair's pools at the payout bot's discretion per
    /// window: creator credits from attribution, holder and trader payouts, and flagship buys. Bounds what a
    /// leaked dev key can take before the admin pauses payouts or rotates it. 0 (a new pair's default)
    /// allows nothing until the admin sets it with `set_outflow_limit`.
    pub outflow_cap: u64,
    pub outflow_window_secs: u32,
    pub outflow_window_start: i64,
    /// Used so far in the current window.
    pub outflow_used: u64,
}

impl Pair {
    /// Counts `amount` against the pair's outflow limit at time `now`, starting a new window when the last
    /// one has ended.
    pub fn consume_outflow(&mut self, amount: u64, now: i64) -> Result<()> {
        let (start, used) = outflow_after(self.outflow_cap, self.outflow_window_secs, self.outflow_window_start, self.outflow_used, amount, now)
            .ok_or(crate::errors::LaunchpadError::OutflowLimit)?;
        self.outflow_window_start = start;
        self.outflow_used = used;
        Ok(())
    }
}

/// Fixed windows of `window_secs`, each starting at the first outflow after the previous one ended. Returns the
/// window's start and use after taking `amount`, or None if that would go over `cap`.
pub fn outflow_after(cap: u64, window_secs: u32, start: i64, used: u64, amount: u64, now: i64) -> Option<(i64, u64)> {
    let (start, used) = if now >= start.saturating_add(window_secs as i64) { (now, 0) } else { (start, used) };
    let used = used.checked_add(amount)?;
    (used <= cap).then_some((start, used))
}

/// A trading-fee level creators can pick at launch. Each is its own LaunchLab platform (LaunchLab charges one
/// fee rate per platform), all paying their fees and locked-LP fee NFTs to the same authority, so the books
/// and the split are the same at every level. A coin keeps its level for life: its curve trades pay its
/// platform's rate, and it graduates into a CPMM pool with the matching fee.
#[account]
#[derive(InitSpace)]
pub struct FeeTier {
    pub index: u8,
    /// Our fee on LaunchLab's scale (1,000,000 = 100%), on top of Raydium's 0.25%: 7,500 makes 1% in total.
    pub fee_rate: u64,
    /// This level's LaunchLab platform config, owned by the tier-admin PDA.
    pub platform_config: Pubkey,
    /// The CPMM config its coins graduate into, with the matching pool fee (fixes their pool address).
    pub cpmm_config: Pubkey,
    /// Gates new launches only; coins already on this level keep trading.
    pub enabled: bool,
    pub bump: u8,
    pub admin_bump: u8,
}

/// One coin launched through us.
#[account]
#[derive(InitSpace)]
pub struct LaunchRecord {
    pub mint: Pubkey,
    pub creator: Pubkey,
    /// The token this coin trades against.
    pub quote_mint: Pubkey,
    /// LaunchLab pool (the bonding curve).
    pub pool: Pubkey,
    pub launched_at: i64,
    /// All our fees credited to this coin (attributed curve fees and LP fees), in the pair token.
    pub fees: u64,
    /// The coin's holder pool.
    pub holders_in: u64,
    pub holders_out: u64,
    /// Coin tokens burned from its LP fees after graduation.
    pub burned: u64,
    pub creator_vault_bump: u8,
    pub bump: u8,
}

/// A coin's creator earnings and who they're paid to. The funds themselves sit with the authority.
#[account]
#[derive(InitSpace)]
pub struct CreatorVault {
    pub mint: Pubkey,
    pub quote_mint: Pubkey,
    /// Wallet that withdraws; starts as the launching creator.
    pub payout: Pubkey,
    pub harvested: u64,
    pub withdrawn: u64,
    pub bump: u8,
    /// A payout wallet proposed by the current one, which takes over when it signs `accept_payout_wallet`
    /// (so a mistyped address can't receive the earnings); default when none is proposed.
    pub pending_payout: Pubkey,
}

/// One coin per X account, keyed by sha256 of the lowercased handle.
#[account]
#[derive(InitSpace)]
pub struct XRecord {
    pub mint: Pubkey,
    pub bump: u8,
}

/// Where a batch of fees came from.
pub const SOURCE_FEE_VAULT: u8 = 0;
pub const SOURCE_LP: u8 = 2;

#[event]
pub struct Launched {
    pub mint: Pubkey,
    pub quote_mint: Pubkey,
    pub ticker: String,
    pub creator: Pubkey,
    pub pool: Pubkey,
}

/// Fees collected and split. `mint` is default for pooled vault fees (their per-coin part is pending).
#[event]
pub struct FeesCollected {
    pub mint: Pubkey,
    pub quote_mint: Pubkey,
    pub source: u8,
    pub amount: u64,
    pub creator: u64,
    pub holders: u64,
    pub pending: u64,
    pub flagship: u64,
    pub traders: u64,
    pub treasury: u64,
}

#[event]
pub struct FeesAttributed {
    pub mint: Pubkey,
    /// The platform fee the coin's trades paid.
    pub coin_fee: u64,
    pub creator: u64,
    pub holders: u64,
    /// The fees in the coin's pair token this covers (equal to coin_fee for SOL coins).
    pub source_amount: u64,
}

/// The payout bot swapped `spent` of a pair token's unconverted fees to `sol` lamports, booked like a SOL claim.
#[event]
pub struct FeesConverted {
    pub quote_mint: Pubkey,
    pub spent: u64,
    pub sol: u64,
    pub pending: u64,
    pub flagship: u64,
    pub traders: u64,
    pub treasury: u64,
}

#[event]
pub struct PendingReleased {
    pub quote_mint: Pubkey,
    pub amount: u64,
}

#[event]
pub struct HolderRewardPaid {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
}

#[event]
pub struct TraderRewardPaid {
    pub quote_mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
}

/// The developer wallet (`buyer`: the distributor, which launched the flagship) bought the flagship with
/// `spent` of a pair's flagship pool and burned the `burned` tokens it got, in the same instruction.
#[event]
pub struct FlagshipBought {
    pub quote_mint: Pubkey,
    pub buyer: Pubkey,
    pub spent: u64,
    pub burned: u64,
}

#[event]
pub struct LpTokensBurned {
    pub mint: Pubkey,
    pub amount: u64,
}

#[event]
pub struct CreatorFeesWithdrawn {
    pub mint: Pubkey,
    pub payout: Pubkey,
    pub amount: u64,
}

/// The distributor sent a coin's creator earnings to its payout wallet's token account (`pay_creator`).
#[event]
pub struct CreatorPaid {
    pub mint: Pubkey,
    pub payout: Pubkey,
    pub amount: u64,
    /// The distributor that signed it.
    pub pusher: Pubkey,
}

#[event]
pub struct DistributorProposed {
    pub proposed: Pubkey,
}

#[event]
pub struct DistributorChanged {
    pub previous: Pubkey,
    pub distributor: Pubkey,
}

#[event]
pub struct FlagshipSet {
    pub mint: Pubkey,
    pub creator: Pubkey,
}

/// The settings after an `update_global` (every field, changed or not).
#[event]
pub struct GlobalUpdated {
    pub attestor: Pubkey,
    pub attestor_required: bool,
    pub treasury: Pubkey,
    pub split: FeeSplit,
    pub paused: bool,
    pub payouts_paused: bool,
}

#[event]
pub struct OutflowLimitSet {
    pub quote_mint: Pubkey,
    pub cap: u64,
    pub window_secs: u32,
}

/// The admin moved `amount` of a pair's unspent flagship pool to its treasury (`release_flagship`).
#[event]
pub struct FlagshipReleased {
    pub quote_mint: Pubkey,
    pub amount: u64,
}

/// A coin's payout wallet proposed a new one, which takes over when it accepts (default: proposal cancelled).
#[event]
pub struct PayoutWalletProposed {
    pub mint: Pubkey,
    pub payout: Pubkey,
    pub proposed: Pubkey,
}

#[event]
pub struct PayoutWalletChanged {
    pub mint: Pubkey,
    pub previous: Pubkey,
    pub payout: Pubkey,
}

/// What `set_payout_wallet(new)` does to a vault's (payout, pending_payout): the treasury takes over at once (it
/// is the admin's known wallet, and $F33D's payouts go there); the default key cancels a proposal; any other
/// wallet is only proposed, and takes over when it signs `accept_payout_wallet`.
pub fn payout_change(payout: Pubkey, new: Pubkey, treasury: Pubkey) -> (Pubkey, Pubkey) {
    if new != Pubkey::default() && new == treasury {
        (new, Pubkey::default())
    } else {
        (payout, new)
    }
}

/// Whether a coin launched by `creator` may become the flagship: its creator must be the distributor, or the
/// distributor the admin has proposed (so the flagship can be set before the hand-over is accepted).
pub fn may_be_flagship(creator: &Pubkey, distributor: &Pubkey, pending_distributor: &Pubkey) -> bool {
    creator == distributor || (*pending_distributor != Pubkey::default() && creator == pending_distributor)
}

/// The developer wallet's quote account during a flagship buy: it held `before`, received `amount` from the
/// flagship pool, and held `after_buy` once the buy was done. Returns (spent, refund): what the buy took and
/// what goes back to the pool. None if the buy took more than it was given, or the account gained, either of
/// which would mean the dev's own funds were involved.
pub fn flagship_leg(before: u64, amount: u64, after_buy: u64) -> Option<(u64, u64)> {
    let spent = before.checked_add(amount)?.checked_sub(after_buy)?;
    (spent <= amount).then(|| (spent, amount - spent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_split_is_valid_and_exact() {
        let s = FeeSplit::FLOAT;
        assert!(s.is_valid());
        let a = s.apply(1_000_000);
        assert_eq!((a.creator, a.holders, a.flagship, a.traders, a.treasury), (50_000, 150_000, 350_000, 350_000, 100_000));
        assert_eq!(s.per_coin_bps(), 2_000);
    }

    #[test]
    fn our_split_is_valid_and_exact() {
        let s = FeeSplit::DEFAULT;
        assert!(s.is_valid());
        let a = s.apply(1_000_000);
        assert_eq!((a.creator, a.holders, a.flagship, a.traders, a.treasury), (250_000, 200_000, 100_000, 350_000, 100_000));
        assert_eq!(s.per_coin_bps(), 4_500);
    }

    #[test]
    fn rounding_goes_to_the_treasury_and_nothing_is_lost() {
        for s in [FeeSplit::FLOAT, FeeSplit::DEFAULT] { for amount in [0u64, 1, 7, 99, 12_345, u64::MAX / 3] {
            let a = s.apply(amount);
            assert_eq!(a.creator + a.holders + a.flagship + a.traders + a.treasury, amount);
        } }
    }

    #[test]
    fn only_the_distributor_or_the_proposed_one_can_launch_the_flagship() {
        let (dev, next, other) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        assert!(may_be_flagship(&dev, &dev, &Pubkey::default()));
        assert!(may_be_flagship(&next, &dev, &next));
        assert!(!may_be_flagship(&other, &dev, &next));
        assert!(!may_be_flagship(&other, &dev, &Pubkey::default()));
        // "No pending distributor" never matches.
        assert!(!may_be_flagship(&Pubkey::default(), &dev, &Pubkey::default()));
    }

    #[test]
    fn the_flagship_leg_refunds_the_unspent_part_and_never_touches_the_dev_funds() {
        // Fully spent, with or without quote of the dev's own in the account.
        assert_eq!(flagship_leg(0, 100, 0), Some((100, 0)));
        assert_eq!(flagship_leg(5_000, 100, 5_000), Some((100, 0)));
        // The curve completed part way: 40 unspent goes back.
        assert_eq!(flagship_leg(5_000, 100, 5_040), Some((60, 40)));
        assert_eq!(flagship_leg(5_000, 100, 5_100), Some((0, 100)));
        // The buy took some of the dev's own quote, or the account gained: refused.
        assert_eq!(flagship_leg(5_000, 100, 4_999), None);
        assert_eq!(flagship_leg(5_000, 100, 5_101), None);
        assert_eq!(flagship_leg(u64::MAX, 1, 0), None);
    }

    #[test]
    fn outflows_are_capped_per_window() {
        let (cap, win) = (1_000u64, 3_600u32);
        // A new pair (cap 0) lets nothing out.
        assert_eq!(outflow_after(0, win, 0, 0, 1, 10_000), None);
        assert_eq!(outflow_after(0, win, 0, 0, 0, 10_000), Some((10_000, 0)));
        // The first outflow opens a window; it fills up to the cap and no further.
        assert_eq!(outflow_after(cap, win, 0, 0, 600, 10_000), Some((10_000, 600)));
        assert_eq!(outflow_after(cap, win, 10_000, 600, 400, 10_100), Some((10_000, 1_000)));
        assert_eq!(outflow_after(cap, win, 10_000, 600, 401, 10_100), None);
        assert_eq!(outflow_after(cap, win, 10_000, 1_000, 1, 13_599), None);
        // Once it has ended, the next one starts from zero.
        assert_eq!(outflow_after(cap, win, 10_000, 1_000, 1_000, 13_600), Some((13_600, 1_000)));
        assert_eq!(outflow_after(cap, win, 10_000, 0, 1_001, 13_600), None);
        assert_eq!(outflow_after(u64::MAX, win, 10_000, u64::MAX, 1, 10_001), None);
    }

    #[test]
    fn a_new_payout_wallet_must_accept_unless_it_is_the_treasury() {
        let (payout, next, treasury, none) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::default());
        assert_eq!(payout_change(payout, next, treasury), (payout, next));
        assert_eq!(payout_change(payout, treasury, treasury), (treasury, none));
        // The default key cancels a proposal, even if the treasury were unset.
        assert_eq!(payout_change(payout, none, treasury), (payout, none));
        assert_eq!(payout_change(payout, none, none), (payout, none));
    }

    #[test]
    fn splits_must_add_up() {
        assert!(!FeeSplit { treasury: 999, ..FeeSplit::FLOAT }.is_valid());
        assert!(FeeSplit { creator: 10_000, holders: 0, flagship: 0, traders: 0, treasury: 0 }.is_valid());
    }
}
