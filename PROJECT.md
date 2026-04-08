# outlayer-tipbot

Telegram tip bot for NEAR and USDC, powered by Outlayer deterministic wallets. Zero database — wallets are derived from a single NEAR key + Telegram user ID.

## How it works

One NEAR private key in env. For each Telegram user, the bot computes `seed = sha256("tg:{user_id}")` and uses it to derive an Outlayer custody wallet. The same seed always produces the same wallet. No per-user secrets stored anywhere — restart the bot, move to another server, everything works.

Every Outlayer API request is signed with the bot's NEAR key using deterministic auth: `Bearer near:<base64url(JSON{account_id, seed, pubkey, timestamp, signature}))>`. The coordinator verifies the ed25519 signature and maps `(account_id, seed)` to a wallet.

Tips use payment checks: sender locks tokens into an ephemeral intents account, receiver claims them. On failure — 3 retries, then reclaim back to sender. Funds are never lost.

Swaps and withdrawals go through NEAR Intents solver relay — fully gasless.

## Project structure

```
outlayer-tipbot/
├── src/
│   ├── main.rs              Entry point, AppState, dispatcher, command routing
│   ├── outlayer.rs           Outlayer API client, auth, amount helpers
│   └── handlers/
│       ├── mod.rs            TokenConfig, ConvoState, view builders, keyboards
│       ├── start.rs          /start — main menu with balances
│       ├── tip.rs            /near and /usd — tips in groups (reply)
│       ├── swap.rs           Swap flow (NEAR↔USDC via intents)
│       ├── withdraw.rs       Withdraw flow + deposit custom amount (text input)
│       └── callback.rs       All inline keyboard button handlers
├── contract/
│   ├── src/lib.rs            NEAR smart contract: deposit helper (NEAR→wNEAR→intents)
│   ├── Cargo.toml            near-sdk 5.17.2 with legacy feature
│   ├── rust-toolchain.toml   Rust 1.86.0
│   └── build.sh              Build with cargo-near
├── .env.example
├── DEPLOY.md
└── PROJECT.md
```

## Bot commands

| Command | Context | Description |
|---------|---------|-------------|
| `/start` | DM | Main menu with balances and inline buttons |
| `/balance` | DM | Both balances + wallet address + transactions link |
| `/deposit` | DM | Choose token → choose amount → web wallet link |
| `/withdraw` | DM | Choose token → address → amount (MAX) → confirm |
| `/near <amount>` | Group (reply) | Tip NEAR to replied user |
| `/usd <amount>` | Group (reply) | Tip USDC to replied user |

Different command menus are registered for DMs vs groups via `setMyCommands` with scope.

## User flows

### Main menu (/start)

Shows both balances (NEAR + USDC) and inline buttons:
```
[💰 Balance]  [📥 Deposit]
[🔄 Swap]
[📤 Withdraw NEAR]  [📤 Withdraw USDC]
```

### Balance

Shows NEAR and USDC balances, Outlayer wallet address (`<code>` — tap to copy), and buttons:
- **Refresh** — re-fetches balances, edits message in place
- **Transactions** — opens intents explorer for the wallet

### Deposit

Multi-step:
1. Choose token: NEAR / USDC
2. Choose amount: preset buttons (0.5, 1, 5, 10) or custom (free text input, validated)
3. "Open wallet" button — URL to `outlayer.fastnear.com/wallet/fund` with all parameters

NEAR deposits go through `deposit.tipbot.near` contract (wrap → intents).
USDC deposits go directly via `ft_transfer_call` to `intents.near`.

### Swap

1. Choose direction: NEAR → USDC or USDC → NEAR
2. Enter amount (shows available balance, MAX button)
3. Confirm → gasless swap via `/wallet/v1/intents/swap`
4. Result shows received amount

Tokens use `nep141:` defuse asset ID prefix for the swap API.

### Withdraw

1. `/withdraw` or button → choose token (NEAR / USDC)
2. Enter recipient address (auto-detects chain: near/ethereum/solana)
3. Enter amount (MAX button shows full balance)
4. Confirm/Cancel → gasless withdraw via intents
5. Success message links to near-intents.org

### Tip (/near, /usd)

In a group chat, reply to someone's message:
```
/near 1.5
/usd 5.00
```

Flow:
1. Validate: amount, reply exists, sender ≠ receiver, not a bot, not anonymous admin
2. Rate limit: 5 sec between tips per sender
3. Tip lock: prevents concurrent tips from same sender (double-spend protection)
4. Register both wallets (idempotent)
5. Check sender balance, adjust for dust
6. Create payment check (sender → ephemeral account)
7. Claim check for receiver (3 retries, 2 sec delay)
8. On failure → reclaim back to sender
9. On success → short message in group + DM to receiver

## Deposit helper contract

`contract/src/lib.rs` — deployed to `deposit.tipbot.near`.

Purpose: accept native NEAR and deposit as wNEAR on intents in one transaction. Users call `deposit(msg)` with attached NEAR. The contract:

1. Calls `wrap.near::near_deposit()` — wraps NEAR to wNEAR
2. On success callback, calls `wrap.near::ft_transfer_call(intents.near, amount, msg)` — deposits to intents
3. On wrap failure — refunds NEAR to sender

`msg` parameter = the hex64 intents account to credit (user's Outlayer wallet address).

Owner can pause/resume deposits: `set_deposits_enabled(true/false)`.

Prerequisites: contract account must have storage registered on `wrap.near` (one-time `storage_deposit`).

## Environment variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `TELEGRAM_BOT_TOKEN` | yes | — | From @BotFather |
| `NEAR_ACCOUNT_ID` | yes | — | NEAR account that owns the bot's key |
| `NEAR_PRIVATE_KEY` | yes | — | `ed25519:<base58>` private key |
| `OUTLAYER_API` | no | `https://api.outlayer.fastnear.com` | Outlayer coordinator |
| `WNEAR_TOKEN` | no | `wrap.near` | wNEAR contract |
| `USDC_TOKEN` | no | USDC contract hash | USDC contract |
| `DEPOSIT_CONTRACT` | no | `deposit.tipbot.near` | Deposit helper contract |
| `FUND_BASE_URL` | no | `https://outlayer.fastnear.com/wallet/fund` | Fund page URL |
| `RUST_LOG` | no | `outlayer_tipbot=info` | Log level |

For testnet: `WNEAR_TOKEN=wrap.testnet`, `USDC_TOKEN=usdc.fakes.testnet`, `DEPOSIT_CONTRACT=deposit.tipbot.testnet`.

## Outlayer API endpoints used

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/register` | POST | Create/derive deterministic wallet (signature in body) |
| `/wallet/v1/balance?token=...&source=intents` | GET | Token balance (string, u128-safe) |
| `/wallet/v1/address?chain=near` | GET | Wallet deposit address (hex64) |
| `/wallet/v1/payment-check/create` | POST | Lock funds into ephemeral check |
| `/wallet/v1/payment-check/claim` | POST | Claim check (receiver's auth) |
| `/wallet/v1/payment-check/reclaim` | POST | Return check to creator (by check_id) |
| `/wallet/v1/intents/swap` | POST | Gasless swap (nep141: prefixed tokens, amount_in) |
| `/wallet/v1/intents/withdraw` | POST | Gasless withdraw (token, amount, chain, to) |

Auth for `/register`: NEAR signature fields in request body.
Auth for `/wallet/v1/*`: `Authorization: Bearer near:<base64url>` header.

## Security model

- **One key, all wallets.** The NEAR private key controls every user's wallet. Protect it.
- **Seed is predictable** — `sha256("tg:{user_id}")` can be computed by anyone who knows the Telegram user ID. This is by design: the seed without the NEAR key signature is useless.
- **Key rotation** — add new key to NEAR account, update env, restart bot, remove old key. Wallets unaffected (identity = account_id + seed, not key).
- **Double-spend protection** — `tip_locks` DashMap prevents concurrent tip operations from the same sender.
- **Rate limiting** — 5 seconds between tips per sender.
- **Tip validations** — self-tip, bot recipient, anonymous admin, amount bounds, balance check.
- **Reclaim on failure** — if payment check claim fails after 3 retries, funds are reclaimed to sender.
- **DM-only for sensitive ops** — balance, withdraw, deposit only work in private chat.
- **Telegram guarantees user IDs** — `msg.from` and `callback.from` come from Telegram API, not user input.

## UX patterns

- **Inline keyboards** for navigation, commands as entry points.
- **Edit-in-place** — callback buttons update the existing message.
- **Every screen has Back/Cancel** — no dead ends.
- **Multi-step flows** (withdraw, deposit custom amount) use `ConvoState` in DashMap, reset on any command.
- **Group tips are minimal** — one line + "Open wallet" button. Receiver gets a DM.
- **Amounts are u128 strings** internally — no f64 precision loss for NEAR (24 decimals).

## Development

```bash
cp .env.example .env
# Fill in TELEGRAM_BOT_TOKEN, NEAR_ACCOUNT_ID, NEAR_PRIVATE_KEY
cargo run
```

### Adding a new token

1. Add `TokenConfig` in `main.rs` (contract, symbol, decimals, display_dp, dust, prefix)
2. Add to `AppState`, update `token_by_key()`
3. Add tip command variant to `Command` enum
4. Add deposit/withdraw/swap routes in `callback.rs`

### Adding a new command

1. Add variant to `Command` enum in `main.rs`
2. Add match arm in `handle_command()`
3. Add view builder in `handlers/mod.rs` if needed
4. Add callback routes in `handlers/callback.rs` if needed

### Adding a new callback button

1. Add button to keyboard builder in `handlers/mod.rs`
2. Add match arm in `handlers/callback.rs`
