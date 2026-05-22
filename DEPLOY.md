# Deploying outlayer-tipbot

## Prerequisites

- Rust toolchain (`rustup`)
- `cargo-near` (`cargo install cargo-near`)
- A Telegram bot token (from [@BotFather](https://t.me/BotFather))
- A NEAR account with at least one ed25519 access key
- The NEAR account's private key in `ed25519:<base58>` format

## 1. Build the bot

```bash
cargo build --release --features dice
```

Binary: `target/release/outlayer-tipbot`.

> The `dice` feature enables the group dice game. It is **off by default** (a
> plain `cargo build --release` produces a pure stateless tipbot with no on-disk
> state). This deployment runs with dice enabled, so build with `--features dice`.

### Cross-compile for Linux (from macOS)

```bash
docker run --rm -v "$PWD":/app -w /app rust:1.86 cargo build --release --features dice
```

## 2. Build and deploy the deposit contract

```bash
cd contract
./build.sh
```

Output: `contract/res/deposit_helper.wasm`

### Deploy to mainnet

```bash
# Deploy
near contract deploy deposit.tipbot.near use-file res/deposit_helper.wasm \
  without-init-call network-config mainnet sign-with-keychain send

# Initialize
near contract call-function as-transaction deposit.tipbot.near new \
  json-args '{"wrap_near":"wrap.near","intents":"intents.near"}' \
  prepaid-gas '30.0 Tgas' attached-deposit '0 NEAR' \
  sign-as deposit.tipbot.near network-config mainnet sign-with-keychain send

# Register storage on wrap.near (one-time)
near contract call-function as-transaction wrap.near storage_deposit \
  json-args '{"account_id":"deposit.tipbot.near"}' \
  prepaid-gas '30.0 Tgas' attached-deposit '0.00125 NEAR' \
  sign-as deposit.tipbot.near network-config mainnet sign-with-keychain send
```

Optional: add a function-call-only key for the contract account:

```bash
near account add-key deposit.tipbot.near \
  grant-function-call-access --allowance unlimited \
  --contract-account-id deposit.tipbot.near \
  --function-names register autogenerate-new-keypair \
  save-to-legacy-keychain network-config mainnet sign-with-keychain send
```

### Pause/resume deposits

```bash
# Pause
near contract call-function as-transaction deposit.tipbot.near set_deposits_enabled \
  json-args '{"enabled":false}' prepaid-gas '10.0 Tgas' attached-deposit '0 NEAR' \
  sign-as deposit.tipbot.near network-config mainnet sign-with-keychain send

# Resume
near contract call-function as-transaction deposit.tipbot.near set_deposits_enabled \
  json-args '{"enabled":true}' prepaid-gas '10.0 Tgas' attached-deposit '0 NEAR' \
  sign-as deposit.tipbot.near network-config mainnet sign-with-keychain send
```

## 3. Configure the bot

Create `.env` on the target server (or set env vars directly):

```bash
TELEGRAM_BOT_TOKEN=123456:ABC-DEF...
NEAR_ACCOUNT_ID=my-tipbot.near
NEAR_PRIVATE_KEY=ed25519:5Kxz...
OUTLAYER_API=https://api.outlayer.fastnear.com

# Tokens
WNEAR_TOKEN=wrap.near
USDC_TOKEN=17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1

# Deposit helper contract
DEPOSIT_CONTRACT=deposit.tipbot.near

RUST_LOG=outlayer_tipbot=info
```

For **testnet**:

```bash
WNEAR_TOKEN=wrap.testnet
USDC_TOKEN=usdc.fakes.testnet
DEPOSIT_CONTRACT=deposit.tipbot.testnet
```

Decimals are hardcoded: 24 for wNEAR, 6 for USDC.

**Security:**
- `.env` must be readable only by the bot user: `chmod 600 .env`
- `NEAR_PRIVATE_KEY` is the only secret. It controls all user wallets. Protect it.
- Consider using a dedicated NEAR account with a single full-access key for the bot.

## 4. Run

### Direct

```bash
cd /opt/outlayer-tipbot
./outlayer-tipbot
```

### systemd (recommended)

Create `/etc/systemd/system/outlayer-tipbot.service`:

```ini
[Unit]
Description=Outlayer Telegram Tip Bot
After=network.target

[Service]
Type=simple
User=tipbot
WorkingDirectory=/opt/outlayer-tipbot
ExecStart=/opt/outlayer-tipbot/outlayer-tipbot
Restart=always
RestartSec=5
EnvironmentFile=/opt/outlayer-tipbot/.env

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd -r -s /usr/sbin/nologin tipbot
sudo mkdir -p /opt/outlayer-tipbot
sudo cp target/release/outlayer-tipbot /opt/outlayer-tipbot/
sudo cp .env /opt/outlayer-tipbot/
sudo chown -R tipbot:tipbot /opt/outlayer-tipbot
sudo chmod 600 /opt/outlayer-tipbot/.env

sudo systemctl daemon-reload
sudo systemctl enable --now outlayer-tipbot
sudo journalctl -u outlayer-tipbot -f
```

### Docker

```dockerfile
FROM rust:1.86 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --features dice

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/outlayer-tipbot /usr/local/bin/
CMD ["outlayer-tipbot"]
```

```bash
docker build -t outlayer-tipbot .
docker run -d --name tipbot --env-file .env --restart always outlayer-tipbot
```

## 5. Verify

1. Open Telegram, DM the bot, send `/start`
2. You should see the main menu with both balances (NEAR: 0.0000, USDC: $0.00)
3. Tap "Deposit" — see intents address for USDC and `near call` command for NEAR
4. Deposit some tokens
5. Tap "Balance" → "Refresh" — balances should update
6. Try "Swap" — NEAR to USDC or vice versa
7. Add the bot to a test group, reply to a message with `/near 0.01` or `/usd 0.01`
8. Check that the tip goes through and the recipient gets a DM
9. Try "Withdraw NEAR" / "Withdraw USDC" — multi-step flow with confirmation

## 6. Updating

```bash
git pull
cargo build --release --features dice
sudo cp target/release/outlayer-tipbot /opt/outlayer-tipbot/
sudo systemctl restart outlayer-tipbot
```

## 7. Monitoring

Logs go to stdout (captured by systemd journal or Docker logs).

```bash
# systemd
journalctl -u outlayer-tipbot -f

# docker
docker logs -f tipbot
```

Set `RUST_LOG=outlayer_tipbot=debug` for verbose output (all API calls logged).

## 8. NEAR key rotation

If the bot's NEAR key is compromised:

```bash
# 1. Add new key to the NEAR account
near add-key my-tipbot.near ed25519:NEW_PUBLIC_KEY

# 2. Update .env with the new private key
# 3. Restart the bot
sudo systemctl restart outlayer-tipbot

# 4. Remove the old key (within 60 sec, old key stops working)
near delete-key my-tipbot.near ed25519:OLD_PUBLIC_KEY
```

Wallets are NOT affected — wallet identity depends on (account_id, seed), not the signing key.
