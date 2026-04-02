# Deploying outlayer-tipbot

## Prerequisites

- Rust toolchain (`rustup`)
- A Telegram bot token (from [@BotFather](https://t.me/BotFather))
- A NEAR account with at least one ed25519 access key
- The NEAR account's private key in `ed25519:<base58>` format

## 1. Build

```bash
cargo build --release
```

Binary: `target/release/outlayer-tipbot` (~15 MB, statically linked except libc).

### Cross-compile for Linux (from macOS)

```bash
rustup target add x86_64-unknown-linux-gnu
cargo build --release --target x86_64-unknown-linux-gnu
```

Or use Docker:

```bash
docker run --rm -v "$PWD":/app -w /app rust:1.82 cargo build --release
```

## 2. Configure

Create `.env` on the target server (or set env vars directly):

```bash
TELEGRAM_BOT_TOKEN=123456:ABC-DEF...
NEAR_ACCOUNT_ID=my-tipbot.near
NEAR_PRIVATE_KEY=ed25519:5Kxz...
OUTLAYER_API=https://api.outlayer.fastnear.com
TIP_TOKEN=17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1
TIP_DECIMALS=6
RUST_LOG=outlayer_tipbot=info
```

**Security:**
- `.env` must be readable only by the bot user: `chmod 600 .env`
- `NEAR_PRIVATE_KEY` is the only secret. It controls all user wallets. Protect it.
- Consider using a dedicated NEAR account with a single full-access key for the bot.

## 3. Run

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
FROM rust:1.82 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/outlayer-tipbot /usr/local/bin/
CMD ["outlayer-tipbot"]
```

```bash
docker build -t outlayer-tipbot .
docker run -d --name tipbot --env-file .env --restart always outlayer-tipbot
```

## 4. Verify

1. Open Telegram, DM the bot, send `/start`
2. You should see the main menu with your balance ($0.00)
3. Tap "Deposit" — you should see a NEAR hex address
4. Send some testnet USDC to that address
5. Tap "Balance" → "Refresh" — balance should update
6. Add the bot to a test group, reply to a message with `/near 0.01`
7. Check that the tip goes through and the recipient gets a DM

## 5. Updating

```bash
git pull
cargo build --release
sudo cp target/release/outlayer-tipbot /opt/outlayer-tipbot/
sudo systemctl restart outlayer-tipbot
```

## 6. Monitoring

Logs go to stdout (captured by systemd journal or Docker logs).

```bash
# systemd
journalctl -u outlayer-tipbot -f

# docker
docker logs -f tipbot
```

Set `RUST_LOG=outlayer_tipbot=debug` for verbose output (all API calls logged).

## 7. NEAR key rotation

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
