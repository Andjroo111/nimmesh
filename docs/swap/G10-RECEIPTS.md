# G10 receipts — a REAL NIM⇄USDC atomic swap through the APP-FACING FFI constructors (2026-07-10)

Act 2 (`docs/swap/ACT2-RECEIPTS.md`) proved the live money path through the **rig** door
(`MeshHarness::add_session_participant` + a caller-composed session). **G10c proves the same
swap driven entirely through the EXACT `#[uniffi::export]` constructors the shipping app and
the Mac node call** — nothing hand-composed:

- the **initiator** (standing in for the phone) via `MeshNode::new_live_swap_initiator` — the
  door `SwapMesh.swift`'s live path uses: the wallet's enclave key funds the real NIM HTLC,
  the claimed USDC pays a derived receive address, a derived gas key lands `withdraw(S)`;
- the **responder** (the Mac rig) via `MeshNode::new_live_swap_responder` — the door
  `mac-node --swap-responder-live` uses: it escrows REAL Amoy USDC only AFTER its real
  `NimHtlcVerifier` confirms the NIM HTLC on-chain at depth, then claims the NIM leg with `S`.

Both nodes were `adopt`ed onto the deterministic mesh ether (BLE is already proven live — this
run is about REAL MONEY through the REAL app constructors, so the radio is the harness ether)
and driven with `poll_sync`/`poll_beacon`, exactly as the shims tick them on-device.

Tool: `cargo run --example live_ffi_mesh_swap --features "polygon-gateway gateway-rpc"`
(core v0.67.0; the G8-review safety — C1 live-safety gate, M3 pre-signer reveal + burial hold,
M4 anchored terms — is baked into these constructors, so this run also validates the safe path).

## The actors

| Role | Address |
|---|---|
| Treasury (funds the initiator's NIM leg via its enclave key; refund + sweep destination) | [`NQ92 VGEX VYH9 KHP0 Y00L DAQM 32N2 8H12 H9F7`](https://nimiq-testnet.observer/#NQ92+VGEX+VYH9+KHP0+Y00L+DAQM+32N2+8H12+H9F7) |
| Responder's NIM claim address (its session identity, derived off the treasury seed) | [`NQ57 7X7Q H4NQ 0AHD YLYR 53CE 1XXK AMRC HAB2`](https://nimiq-testnet.observer/#NQ57+7X7Q+H4NQ+0AHD+YLYR+53CE+1XXK+AMRC+HAB2) |
| Funded Amoy wallet (responder's escrow funder + gas) | [`0xa7bb819ba03743643249dfcca7508280ece059b1`](https://amoy.polygonscan.com/address/0xa7bb819ba03743643249dfcca7508280ece059b1) |
| Initiator's derived Amoy GAS account (pays `withdraw(S)`; seeded 0.02 POL from the funded wallet) | [`0x2e20da6d1939c4e7e85a7e495d0f8707e6bd0636`](https://amoy.polygonscan.com/address/0x2e20da6d1939c4e7e85a7e495d0f8707e6bd0636) |
| Initiator's derived Amoy USDC CLAIM address (the swap payout) | [`0xd0239ae54438c725b090d4fa4198b4987336a143`](https://amoy.polygonscan.com/address/0xd0239ae54438c725b090d4fa4198b4987336a143) |
| `NimmeshHtlc` v2 (forwarder-bound, `docs/swap/AMOY.md`) | [`0xb3B3703E07AC897B7E3e864C113a2Fa547D76736`](https://amoy.polygonscan.com/address/0xb3B3703E07AC897B7E3e864C113a2Fa547D76736) |

Trade: **5 tNIM (500 000 luna) ⇄ 1 USDC (1 000 000 µUSDC)**. Discovery → both legs claimed in
~6 protocol ticks (chain waits dominate).

## The swap

| Step | Chain | Tx |
|---|---|---|
| 0. Seed the initiator's derived gas account (0.02 POL, from the funded wallet — the derived account starts empty) | Amoy | [`0x30ae0711…7dc4`](https://amoy.polygonscan.com/tx/0x30ae071117cf89d8cc7a9ef6e80b307947e47c4510cd64668b585a9b9d587dc4) |
| 1. **Initiator** (phone ctor) funds the real NIM HTLC `NQ87 S0XJ U7VH B9FJ 339L N0RF 241J K2TS 0G9B` (500 000 luna, recipient = the responder's claim address, timeout = the ADR-0010 ms mapping of `T_A` against the run's head anchor) | NIM testnet | [`b065fd0c…22b2`](https://nimiq-testnet.observer/transactions/b065fd0c627a5e1f38515f6adf16e2aed5663f5f21044f4309afb4787ee122b2) |
| 2. **Responder** (Mac ctor) — its real `NimHtlcVerifier` confirms the NIM HTLC on-chain at depth, THEN escrows USDC: `newSwap` — `swapId 0xea8998b8747375d9ed4f0b7ecc02f904bf14f64ca04b9aec5e533b9df1e15cae`, 1 000 000 µUSDC, receiver = the initiator's claim address, block 41 838 608 | Amoy | [`0x5c7d16bb…ce51`](https://amoy.polygonscan.com/tx/0x5c7d16bbced55751f539b71790290c8469e417195f4acdabc711c64e7f6fce51) |
| 3. **Initiator** — its `AmoyHtlcSwapVerifier` confirms the escrow at depth, THEN lands `withdraw(S)`: **the on-chain reveal**, mined in block 41 838 613 (held off the mesh until buried past the M3 reveal depth) | Amoy | [`0xbe0862ec…e588`](https://amoy.polygonscan.com/tx/0xbe0862ecc6a5943d3dc20771c23b28002316af4f0306aad0c5635b50017ae588) |
| 4. **Responder** reads `S` off the mesh `PreimageReveal` (`S ‖ raw-tx` wire) and claims the NIM HTLC (RegularTransfer, SHA-256 preimage proof) | NIM testnet | [`0e12b07e…06f3`](https://nimiq-testnet.observer/transactions/0e12b07eeeaeee830b6ad0dbfa50ca0584e8b81fda85a1dc5a113fdba3d906f3) |
| 5. Settle-up: the responder's claimed 500 000 luna swept home to the treasury | NIM testnet | [`8fa8390c…87e9`](https://nimiq-testnet.observer/transactions/8fa8390c42378dcbbb20a8524c2a430bdaabf8e15eb1c97531842d7d343187e9) |

Ground truth (checked by the example before declaring success): the responder's NIM claim
address received +500 000 luna AND the initiator's Amoy claim address received **+1 000 000
µUSDC (+1.000000 USDC)** — the two `S`-claims, one secret, both chains, no trusted party.

## What is proven vs Act 2

Act 2 already proved the money-path signers + verifiers are byte-correct on both live testnets.
G10c adds the one thing that was still rig-only: the whole swap runs through the **actual
app-facing FFI constructors** — `new_live_swap_initiator` (the phone's) and
`new_live_swap_responder` (the Mac's) — with the C1 live-safety gate asserted at construction,
the M3 reveal-deadline + burial hold, and the M4 head-anchored terms all active. The app's
`SwapMesh.swift` live path and `mac-node --swap-responder-live` call these exact constructors.

## Money movement (net)

| Account | Before | After | Net |
|---|---|---|---|
| Treasury NIM | 11 000 000 000 luna | 11 000 000 000 luna | **±0** (500 000 funded, 500 000 swept home; NIM fees are 0) |
| Responder NIM claim | 0 | 0 (swept) | ±0 |
| Funded Amoy wallet USDC | 18.000000 | 17.000000 | −1 USDC (the escrow paid out) |
| Initiator Amoy claim USDC | 0 | 1.000000 | +1 USDC (the swap payout — a distinct derived account) |
| Funded Amoy wallet POL | ~0.060 | ~0.038 | −~0.02 POL (the 0.02 gas seed + escrow gas) |

## Nothing stranded

- Both HTLCs are resolved (NIM claimed, escrow `withdraw`n). Nothing is time-locked.
- **1 USDC** rests on the initiator's derived claim address `0xd0239a…a143` — deliberate (a
  distinct payout account proves the escrow paid the RIGHT party) and fully recoverable: its
  key is `sha256(AMOY_TEST_KEY ‖ "nimmesh-g10c-init-claim-v1")` (sweep via an EIP-2612 permit +
  `transferFrom`, or fund it with dust POL and `transfer`).
- A little POL rests on the derived gas account `0x2e20da…0636` (key
  `sha256(AMOY_TEST_KEY ‖ "nimmesh-g10c-init-gas-v1")`), reused by the next run.
- The example persists the NIM lock the moment it funds and refunds it on a re-run if a run
  ever dies mid-flight (`~/.nimmesh-g10c-state.json`); the app's twin is the
  `LiveLockBook` + `NimHtlcRefunder` seam (`swapMeshRefund`).

## Re-running

```
export NIMMESH_NIM_SEED=…  AMOY_TEST_KEY=…  AMOY_HTLC2_ADDRESS=…  # + optional AMOY_RPC_URL/USDC
cargo run --example live_ffi_mesh_swap --features "polygon-gateway gateway-rpc"
```

Needs 1 USDC + ~0.04 POL on the funded wallet; the NIM side tops itself up from the faucet, and
the derived gas account is seeded from the funded wallet if empty. If POL runs low the run
stops and asks for a human (the POL faucet is not scriptable).
