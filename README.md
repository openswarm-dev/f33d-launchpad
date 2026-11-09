# F33D.pro launchpad program

The on-chain program behind [F33D.pro](https://f33d.pro), a Solana memecoin launchpad built on Raydium LaunchLab.

- **Program ID (mainnet):** [`4CQamM8uVrGmQ8non8jEgq32topQK1cMyR2VDBf6rnLB`](https://solscan.io/account/4CQamM8uVrGmQ8non8jEgq32topQK1cMyR2VDBf6rnLB)
- **Verified build:** [verify.osec.io](https://verify.osec.io/status/4CQamM8uVrGmQ8non8jEgq32topQK1cMyR2VDBf6rnLB)

This repository holds only what is needed to rebuild the program and check it against the bytecode deployed on
mainnet: the Anchor program (`programs/launchpad`), the workspace `Cargo.toml` / `Cargo.lock`, and the Raydium IDLs it
is compiled against (`idls/`). `launchpad.idl.json` is the program's own IDL.

## Verify it yourself

```bash
# The hash of the program deployed on mainnet
solana-verify get-program-hash -u mainnet-beta 4CQamM8uVrGmQ8non8jEgq32topQK1cMyR2VDBf6rnLB

# Build this repository the same reproducible way (Docker) and print its hash
solana-verify build --library-name launchpad
solana-verify get-executable-hash target/deploy/launchpad.so
```

The two hashes match when the deployed program is exactly this source.

## What it does

- **Launch:** coins launch on our LaunchLab platform (one per trading-fee level), paired with SOL or any other token
  LaunchLab supports, with an optional first buy by the creator in the same transaction.
- **Fees:** every fee goes to the program's authority account; the books (Pair, LaunchRecord, CreatorVault) say whose
  it is. Fees in a token other than SOL are swapped to SOL by the payout bot (`convert_begin` / `convert_end` around a
  Jupiter swap, with the bot's own balances checked), so every payout is in SOL.
- **Payouts:** creators, $F33D holders and top traders are paid from those books by the payout bot (the developer
  wallet), which also buys back and burns $F33D. The admin can pause payouts; the treasury only withdraws to its own
  wallet.
