# Safe Transaction Simulator

Self-hosted simulator for Safe (Gnosis Safe) multisig transactions. Paste a
`safeTxHash`, get the **full decoded call trace** against the **latest mainnet
block**.

It's a thin wrapper: fetch the pending tx from the Safe Transaction Service →
one `cast call --trace` (overriding the Safe `threshold` to 1 so no real owner
signatures are needed) → show the trace. No EVM, no anvil, no custom tracer.

Rust backend: `src/lib.rs` (pure logic, no I/O — WASM-ready) and `src/main.rs`
(axum server + JSON API, Safe Tx Service / RPC calls, `cast`). `static/`
(`index.html` + `app.js`) is the vanilla frontend — no build, no deps.

## Requirements

- [Foundry](https://getfoundry.sh)
  `curl -L https://getfoundry.sh/install | bash && foundryup`
- Rust (stable)
- A mainnet JSON-RPC URL

## Run

```sh
MAINNET_RPC_URL="https://ethereum-rpc.publicnode.com" cargo run --release
```

Open the page, paste a pending mainnet `safeTxHash` (from the Safe web UI queue
or the Safe Transaction Service), and simulate. A revert shows its reason in
the trace — that's a useful result, not a crash.

```sh
cargo test                # self-checks, no network/cast needed
```

## Address book

Optional `addressbook.json` at the repo root: `{"0x…": "label"}` (see
`addressbook.example.json`). Labels show in the summary, the token tables and
the trace. It's re-read on every simulation, so no restart is needed.

Edit it from the page: click any address to set or clear its label, or use the
"Address book" panel at the bottom (add, rename, delete).

## My Safes

The "My Safes" panel keeps your Safe addresses in `safes.json` at the repo root
(a JSON array) and lists their pending txs: not executed, nonce ≥ the Safe's
current nonce (same as the Safe UI queue). Click **Simulate** on any row.

## Known limits

- Mainnet only; Safe 1.x storage layout (threshold at slot 4).
- Token summary (ERC20 transfers + net balance changes per holder, plus ETH
  value moves) needs an RPC with `debug_traceCall`; without it the trace still
  shows. WETH `Deposit`/`Withdrawal` and ERC721/1155 aren't counted.
- Local dev tool: binds 127.0.0.1:8000 only.
