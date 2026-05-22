# outlayer-tipbot

A Telegram tip bot for NEAR and USDC, built as a **fully stateless backend** on top of [OutLayer](https://outlayer.fastnear.com).

This repo is published as a **reference implementation** of one pattern: running a multi-user crypto wallet service with **zero database and zero per-user secrets**. Every user's wallet is *derived*, not *stored*. The only secret the bot holds is one NEAR private key in an env var. Delete the server, redeploy on another host, lose the disk — every user still has the exact same wallet with the exact same funds. And if that key ever leaks, you rotate it: add a new access key to the NEAR account, drop the old one, and every wallet keeps working untouched — wallet identity doesn't depend on *which* key signs.

If you are evaluating OutLayer for a backend (bot, web app with OAuth login, AI agent fleet), this is the part worth reading: **[The stateless model](#the-stateless-model)**.

---

## The stateless model

Conventional custodial backends store a key (or an API token) per user in a database. That database is the crown jewel: leak it and every wallet is drained; lose it and every wallet is gone.

This bot stores **nothing per user**. There is no database, no key file, no Redis. State is a pure function:

```
wallet  =  f(NEAR private key,  [vault id],  telegram_user_id)
```

Same inputs → same wallet, every time, from any machine. The bot is a stateless HTTP client in front of OutLayer's coordinator. All custody and signing happen inside Intel TDX TEEs that OutLayer operates — the bot never sees or holds a user's wallet key.

> **What is the vault id, and is it required?**
> A **vault** is a contract you deploy on-chain that owns its own MPC master key. Deriving user wallets *under* a vault you control is what makes custody **sovereign**: you can exit OutLayer unilaterally and keep signing for your users (see [Guarantees → Sovereign exit](#guarantees)).
>
> The vault is **optional**. OutLayer's base deterministic-wallet flow derives wallets directly from `(account_id, seed)` — no vault, and `vault_id` simply drops out of the signed payload (the signed message becomes `auth:{seed}:{timestamp}`). You get the full stateless model and TEE custody without it; you just don't get the sovereign-exit guarantee.
>
> **This bot defaults to a vault** but doesn't require one: set `OUTLAYER_VAULT_ID` to use a vault, leave it unset to fall back to the base `(account_id, seed)` flow. For a fund-custodying tip bot a vault is worth the one extra deploy; for prototyping, skip it.

What this buys you in practice:

| Property | Consequence |
|----------|-------------|
| No per-user storage | Nothing to back up, nothing to leak, nothing to migrate. |
| Deterministic derivation | Restart / redeploy / move servers — wallets are identical. |
| One secret to protect | The NEAR private key in env is the *only* thing that matters. |
| Key rotation without migration | Swap the env key; wallets are unaffected (see [Guarantees](#guarantees)). |
| TEE custody | Even the bot operator cannot extract a user's wallet key. |

The entire integration is ~290 lines: [src/outlayer.rs](src/outlayer.rs). Everything else in the repo is Telegram UX.

---

## How addresses are derived

There are three identifiers in play. Keep them distinct:

- **`NEAR_ACCOUNT_ID`** — the bot's NEAR account, holding the access key the bot signs with. This is the auth anchor. With a vault, it is also the vault's **parent**.
- **`OUTLAYER_VAULT_ID`** — *(optional — see the callout above)* a sovereign vault deployed on-chain (e.g. `vault.my-tipbot.near`). When set, its on-chain parent **must equal** `NEAR_ACCOUNT_ID` and all user wallets derive under this vault's MPC master. When unset, wallets derive directly under `account_id`.
- **`telegram_user_id`** — supplied by the Telegram API (not user input), turned into a per-user seed.

### Step 1 — seed from the Telegram user ID

```rust
seed = hex(sha256("tg:{telegram_user_id}"))
```

See [`OutlayerClient::seed_for_user`](src/outlayer.rs#L53). The seed is **not** a secret — anyone who knows a Telegram user ID can compute it. That is by design: the seed alone is useless. Authority comes from the NEAR signature, not from the seed's secrecy.

### Step 2 — coordinator derives a stable `wallet_id`

The OutLayer coordinator maps `(account_id, seed)` to a deterministic wallet id — a UUID over a domain-separated hash. With a vault configured, that derivation happens under the vault's master instead, keyed on `(vault, seed)`. Either way the mapping is a pure function: the same inputs always yield the same `wallet_id`, and therefore the same on-chain account. The coordinator stores at most a row keyed by `wallet_id`; it stores **no auth secret**.

### Step 3 — keystore (TEE) derives the actual keys

The on-chain **deposit address** is a NEAR implicit account (a 64-hex string). Its private key — and every cross-chain key (Ethereum, Solana, Bitcoin, …) — is derived **inside Intel TDX** from `wallet_id` via the NEAR MPC network. Keys never leave the enclave. The bot only ever receives the public *address*, never a key.

```
telegram_user_id
      │  sha256("tg:{id}")
      ▼
    seed ───────────┐
                    ▼
  vault + seed ──► wallet_id ──► keystore (TEE) ──► addresses
  (public inputs)   (deterministic   (MPC, in-enclave)   (NEAR/ETH/SOL/…)
                     UUID hash)
```

The address is a pure function of public inputs — the **coordinator adds no secret to derivation**, it only *computes* `wallet_id` and routes the request. At runtime it's still a mandatory hop: it's the only path to the keystore and the auth gate (it verifies the NEAR signature on every request and checks the access key on-chain). It just isn't part of what *determines* the address.

The bot reads a user's NEAR deposit address with a single call — [`get_address`](src/outlayer.rs#L272) → `GET /wallet/v1/address?chain=near`. The first authenticated request for a new seed also **auto-provisions** the sub-wallet; there is no explicit registration step.

### Authentication — proving authority on every request

Every call to `/wallet/v1/*` carries:

```
Authorization: Bearer near:<base64url(JSON)>
```

where the JSON is signed with the bot's NEAR key ([`make_bearer`](src/outlayer.rs#L69)):

```jsonc
{
  "account_id": "my-tipbot.near",      // signing account (= vault PARENT, with a vault)
  "seed":       "<hex sha256 of tg id>", // selects WHICH wallet
  "pubkey":     "ed25519:<base58>",
  "timestamp":  1712000000,
  "signature":  "<base58>",            // ed25519 over the signed message (below)
  "vault_id":   "vault.my-tipbot.near" // omitted entirely when no vault is used
}
```

> **Vault binding.** With a vault, the signed message is `auth:{seed}:{timestamp}:{vault_id}` — the vault id is part of what is signed, not just the payload. (Signing `auth:{seed}:{ts}` alone returns `401 invalid_signature` once a vault is in the payload.) Without a vault, both the `vault_id` field and that suffix drop: the message is `auth:{seed}:{timestamp}`. See [`make_bearer`](src/outlayer.rs#L69).

The coordinator then:
1. verifies the ed25519 signature and the ±30s timestamp window,
2. confirms `pubkey` is a live access key on `account_id` (= vault parent) via NEAR RPC (cached 60s),
3. re-derives `wallet_id` from `(vault, seed)` and routes the request to that wallet.

No bearer token is stored anywhere. Authority is checked against the chain on every request.

---

## Capabilities

Each derived wallet is a full OutLayer wallet. The bot surfaces a subset through Telegram:

| Capability | How | Code |
|-----------|-----|------|
| **Balances** | NEAR + USDC, u128-precise strings (no float loss) | [`get_balance`](src/outlayer.rs#L147) |
| **Deposit address** | NEAR implicit account, multi-chain derivation available | [`get_address`](src/outlayer.rs#L272) |
| **Tips** (`/near`, `/usd`) | Reply-to-user in a group; lock → claim → reclaim-on-failure | [`create_payment_check`](src/outlayer.rs#L165) |
| **Gasless swap** | NEAR ↔ USDC via NEAR Intents solver relay | [`swap`](src/outlayer.rs#L251) |
| **Gasless withdraw** | To NEAR / Ethereum / Solana (chain auto-detected) | [`withdraw`](src/outlayer.rs#L227) |
| **Deposit helper** | Native NEAR → wNEAR → intents in one tx | [contract/src/lib.rs](contract/src/lib.rs) |

Swaps and withdrawals require **no gas** on the user's wallet — they go through the intents solver relay. Payment-check tips are atomic from the user's perspective: funds are locked into an ephemeral check, claimed by the receiver, and on claim failure (3 retries) **reclaimed to the sender** — funds are never stranded.

There's also an optional group dice game ([src/extensions/dice.rs](src/extensions/dice.rs)) built on the same wallet primitives. It's **off by default** and lives behind a Cargo feature (`cargo run --features dice`) because — unlike the wallet flow — it keeps a small on-disk journal of in-flight games (escrow check keys aren't derivable). Keeping it gated is deliberate: the default build stays a pure stateless backend, and the dice extension is there as a contrast — what a stateful component looks like and why it needs durable state.

Out of the box (not all wired into this bot, but available per derived wallet): multi-chain addresses, cross-chain deposits via 1Click bridge, arbitrary NEAR contract calls, a per-wallet policy engine (spending limits / allowed actions / freeze thresholds), and TEE attestation verifiable on-chain. See [OutLayer's deterministic-wallets doc](https://outlayer.fastnear.com).

---

## Guarantees

What the architecture guarantees, and what it doesn't.

**Statelessness / recoverability.** Wallet identity is `(vault_id, seed, account_id)` — never anything on the bot's disk. Restart, redeploy, or move to a new server and every user keeps the same wallet and funds. There is nothing to back up.

**No per-user secrets exist.** The coordinator stores no auth credential for these wallets; the bot stores nothing per user. The seed is public by design. The single secret is the NEAR private key in env — protect that one thing.

**TEE custody, MPC-derived keys.** Wallet keys are derived and held inside Intel TDX enclaves — the bot operator, the host, and OutLayer staff cannot extract them, and the enclave image is attested on-chain. The derivation is a deterministic chain, not a stored key:

```
NEAR MPC network ──CKD──► per-vault master ──HMAC──► wallet ed25519 key
  (threshold key,          (one per vault,            HMAC-SHA256(master,
   no node holds it)        in-enclave)                "wallet:<wallet_id>:near")
```

The per-vault master comes from **MPC CKD** (child-key derivation) over the NEAR MPC network: the master key is split across MPC nodes — no single node, and no TEE, ever holds the whole thing. CKD is **deterministic**: the same `(vault, derivation_path)` always reproduces the same master, hence the same wallet keys. That determinism is the load-bearing property — it's both why the bot can be stateless (keys are *recomputed*, never stored) and why sovereign exit works (you can reproduce the master yourself without OutLayer).

**Key rotation without migration.** Add a new access key to the vault parent, update the env, restart, then delete the old key. Wallets are unaffected because identity does not depend on *which* key signs — only on `(vault, seed, account_id)`. ([DEPLOY.md](DEPLOY.md), and the rotation flow in OutLayer's docs.)

**Revocation within ~60s.** Compromised key? `near delete-key` it. Within the coordinator's 60-second access-key cache TTL, every request signed by that key returns 401. No coordinator action, no DB update.

**Sovereign exit (vault mode only).** Because wallets live under a vault you own, the vault parent can `unilateral_initiate_recovery`: after an exit window the contract atomically swaps OutLayer's TEE key out and your key in, and OutLayer loses all access to the vault account. You then reproduce the per-vault master yourself by replaying the same MPC CKD call from chain (same `(predecessor, derivation_path)` → same master), and re-derive every wallet's ed25519 key via `HMAC(master, "wallet:<wallet_id>:near")` — fully offline, no OutLayer cooperation. The funds and key authority were always yours; exit just removes OutLayer from the signing path. Full runbook: OutLayer's `LEAVING_OUTLAYER.md`.

**Funds safety on tips.** Payment-check tips lock → claim → reclaim. On repeated claim failure the funds return to the sender. Concurrent tips from one sender are blocked by an in-process lock (double-spend guard); tips are rate-limited to one per 5 seconds per sender.

**Trust boundary — be explicit.** This is **custodial**. One NEAR key has authority over *all* user wallets in the vault. Compromise of that key compromises every wallet until the key is revoked. The model removes the *database* as an attack surface; it does **not** make the system non-custodial. Treat the NEAR key like a hot wallet's master key: env-only, never committed, rotated on suspicion.

---

## Project structure

```
outlayer-tipbot/
├── src/
│   ├── main.rs            Entry point, AppState, command routing
│   ├── outlayer.rs        OutLayer client: seed derivation, signed auth, wallet ops  ← the integration
│   ├── handlers/          Telegram UX (start, tip, swap, withdraw, callbacks)
│   └── extensions/        Optional, feature-gated add-ons
│       └── dice.rs        Dice game — stateful, off by default (`--features dice`)
├── contract/
│   └── src/lib.rs         Deposit helper contract (native NEAR → wNEAR → intents)
├── .env.example
├── DEPLOY.md
└── PROJECT.md             Full implementation notes
```

## Quick start

```bash
cp .env.example .env
# Fill in TELEGRAM_BOT_TOKEN, NEAR_ACCOUNT_ID (vault parent),
#         NEAR_PRIVATE_KEY (access key on that parent), OUTLAYER_VAULT_ID
cargo run
```

### Configuration

| Variable | Required | Description |
|----------|----------|-------------|
| `TELEGRAM_BOT_TOKEN` | yes | From @BotFather |
| `NEAR_ACCOUNT_ID` | yes | Vault **parent** — holds the bot's signing access key |
| `NEAR_PRIVATE_KEY` | yes | `ed25519:<base58>`, an access key on `NEAR_ACCOUNT_ID` |
| `OUTLAYER_VAULT_ID` | no | Full vault account id; if set, its on-chain parent must equal `NEAR_ACCOUNT_ID`. Unset → base `(account_id, seed)` derivation |
| `OUTLAYER_API` | no | Coordinator (default `https://api.outlayer.fastnear.com`) |
| `WNEAR_TOKEN` / `USDC_TOKEN` | no | Token contracts (mainnet defaults; testnet values in `.env.example`) |
| `DEPOSIT_CONTRACT` | no | Deposit helper (default `deposit.tipbot.near`) |

See [PROJECT.md](PROJECT.md) for the full variable list (including the dice game) and deployment details in [DEPLOY.md](DEPLOY.md).

## License

MIT
