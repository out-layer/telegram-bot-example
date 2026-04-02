# outlayer-tipbot

Telegram tip bot for USDC on NEAR, powered by Outlayer deterministic wallets.

## Architecture

```
src/
├── main.rs              Entry point, dispatcher, command routing
├── outlayer.rs           Outlayer API client (deterministic auth)
└── handlers/
    ├── mod.rs            Shared types, view builders, keyboards
    ├── start.rs          /start — main menu
    ├── tip.rs            /near <amount> — tips in groups
    ├── withdraw.rs       Multi-step withdraw flow (text input)
    └── callback.rs       Inline keyboard button handler
```

### Key design decisions

**Zero storage.** No database. Every wallet is derived deterministically from `(NEAR_ACCOUNT_ID, sha256("tg:{telegram_user_id}"))`. Restart the bot, move to another server — everything works. The NEAR private key in env is the only secret.

**Outlayer deterministic auth.** Every API request is signed with the bot's NEAR key. Auth format: `Bearer near:<base64url(JSON{account_id, seed, pubkey, timestamp, signature}))>`. Timestamp must be ±30 sec. Signature covers `"auth:{seed}:{timestamp}"`.

**Payment checks for tips.** Sender creates a payment check (locks USDC into an ephemeral intents account), receiver claims it. On failure — 3 retries with 2 sec delay, then reclaim back to sender. No funds are ever lost.

**In-memory state.** Rate limiter, conversation state (withdraw flow), and pending withdrawals are all `DashMap` — lost on restart. This is fine: conversations are short-lived, and pending withdrawals timeout naturally.

## Stack

| Concern | Crate |
|---------|-------|
| Telegram bot | teloxide 0.13 |
| HTTP client | reqwest 0.12 |
| Ed25519 signing | ed25519-dalek 2 |
| Base58/Base64 | bs58, base64 |
| SHA-256 | sha2 |
| Env vars | dotenvy |
| Logging | tracing + tracing-subscriber |
| Concurrent maps | dashmap |

## Environment variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `TELEGRAM_BOT_TOKEN` | yes | — | Telegram bot token from @BotFather |
| `NEAR_ACCOUNT_ID` | yes | — | NEAR account that owns the bot's key |
| `NEAR_PRIVATE_KEY` | yes | — | `ed25519:<base58>` private key |
| `OUTLAYER_API` | no | `https://api.outlayer.fastnear.com` | Outlayer coordinator URL |
| `TIP_TOKEN` | no | USDC contract | NEP-141 token contract ID |
| `TIP_DECIMALS` | no | `6` | Token decimal places |
| `RUST_LOG` | no | `outlayer_tipbot=info` | Log level filter |

## Bot commands

| Command | Context | Description |
|---------|---------|-------------|
| `/start` | DM | Main menu with inline buttons |
| `/balance` | DM | Check USDC balance |
| `/deposit` | DM | Show deposit address |
| `/withdraw` | DM | Multi-step withdraw flow |
| `/near <amount>` | Group (reply) | Send tip to replied user |

The bot registers different command menus for DMs vs groups via `setMyCommands` with scope.

## UX patterns

- **Inline keyboards** are the primary navigation. Commands are fallback entry points.
- **Edit-in-place** — callback buttons edit the existing message instead of sending new ones.
- **Every screen has Back/Cancel** — no dead ends.
- **Withdraw is multi-step**: address → amount (with MAX button) → confirm/cancel.
- **Group tips are minimal** — one short message + "Open wallet" deep-link button.
- Receiver gets a DM notification after a tip (if they've interacted with the bot before).

## Development

```bash
cp .env.example .env
# Fill in TELEGRAM_BOT_TOKEN, NEAR_ACCOUNT_ID, NEAR_PRIVATE_KEY

cargo run
```

For testnet: set `NEAR_ACCOUNT_ID=my-bot.testnet` and use a testnet NEAR key. The Outlayer API works with both mainnet and testnet accounts.

### Adding a new command

1. Add variant to `Command` enum in `main.rs`
2. Add match arm in `handle_command()`
3. If it needs a view (DM-only), add a `*_view()` function in `handlers/mod.rs`
4. If it needs a callback button, add the route in `handlers/callback.rs`

### Adding a new callback button

1. Add the button in the keyboard builder (e.g. `main_menu_keyboard()` in `handlers/mod.rs`)
2. Add the callback data match arm in `handlers/callback.rs`

## Outlayer API endpoints used

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/register` | POST | Create/derive deterministic wallet |
| `/wallet/v1/balance?token=...&source=intents` | GET | Get token balance |
| `/wallet/v1/address?chain=...` | GET | Get deposit address |
| `/wallet/v1/payment-check/create` | POST | Lock funds into payment check |
| `/wallet/v1/payment-check/claim` | POST | Claim check into receiver's wallet |
| `/wallet/v1/payment-check/reclaim` | POST | Return unclaimed check to sender |
| `/wallet/v1/intents/withdraw` | POST | Gasless withdraw to external address |

Auth for `/register`: signature fields in request body.
Auth for all `/wallet/v1/*`: `Authorization: Bearer near:<base64url>` header.

## Tip flow (detailed)

```
Sender: /near 1.00 (reply to Receiver)
  │
  ├─ Validate: amount, reply exists, sender ≠ receiver, not a bot
  ├─ Rate limit: 1 tip per 5 sec per sender
  ├─ Register sender wallet (idempotent)
  ├─ Register receiver wallet (idempotent)
  ├─ Check sender balance, adjust for dust (±$0.01)
  ├─ Create payment check (sender → ephemeral account)
  ├─ Claim check for receiver (3 retries, 2 sec delay)
  │   ├─ Success → send confirmation message + DM receiver
  │   └─ Failure → reclaim check back to sender
  └─ Done
```
