use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a call waits for a check transfer to confirm before it reports it
/// as still on its way. Kept under Telegram's patience, not the transfer's.
const SETTLE_WAIT: Duration = Duration::from_secs(60);
const SETTLE_POLL: Duration = Duration::from_secs(2);
/// How long the background return of an unconfirmed check keeps watching it.
const RETURN_WATCH: Duration = Duration::from_secs(15 * 60);

/// How a check transfer (claim or reclaim) ended within the wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckTransfer {
    /// Executed: the funds moved.
    Done,
    /// Handed over and not confirmed yet. It settles on its own; sending it
    /// again is refused while it is in flight, and must not be tried.
    Pending,
}

#[derive(Clone)]
pub struct OutlayerClient {
    account_id: String,
    /// Optional sovereign vault. `Some` → wallets derive under the vault's MPC
    /// master and the vault id is bound into the signed auth message. `None` →
    /// base deterministic flow: wallets derive under `account_id` directly.
    vault_id: Option<String>,
    signing_key: SigningKey,
    pubkey_str: String,
    api_url: String,
    http: Client,
}

impl OutlayerClient {
    pub fn new(
        account_id: String,
        private_key_str: &str,
        vault_id: Option<String>,
        api_url: String,
    ) -> Self {
        let key_b58 = private_key_str
            .strip_prefix("ed25519:")
            .expect("NEAR_PRIVATE_KEY must start with ed25519:");
        let key_bytes = bs58::decode(key_b58)
            .into_vec()
            .expect("invalid base58 in NEAR_PRIVATE_KEY");

        let secret: [u8; 32] = match key_bytes.len() {
            64 => key_bytes[..32].try_into().unwrap(),
            32 => key_bytes.try_into().unwrap(),
            n => panic!("unexpected NEAR key length: {n} (expected 32 or 64)"),
        };

        let signing_key = SigningKey::from_bytes(&secret);
        let pubkey_str = format!(
            "ed25519:{}",
            bs58::encode(signing_key.verifying_key().as_bytes()).into_string()
        );

        Self {
            account_id,
            vault_id,
            signing_key,
            pubkey_str,
            api_url,
            http: Client::new(),
        }
    }

    fn seed_for_user(&self, tg_user_id: u64) -> String {
        format!("{:x}", Sha256::digest(format!("tg:{tg_user_id}").as_bytes()))
    }

    fn timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn sign_message(&self, message: &str) -> String {
        let sig = self.signing_key.sign(message.as_bytes());
        bs58::encode(sig.to_bytes()).into_string()
    }

    fn make_bearer(&self, seed: &str) -> String {
        let ts = Self::timestamp();

        let mut payload = serde_json::json!({
            "account_id": self.account_id,
            "seed": seed,
            "pubkey": self.pubkey_str,
            "timestamp": ts,
        });

        // The signed message and payload must match what the coordinator
        // expects for each mode. With a vault, vault_id MUST be part of what we
        // sign — not just the JSON payload (verified against prod: signing
        // "auth:{seed}:{ts}" alone returns 401 invalid_signature when vault_id
        // is in the payload). Without a vault, it's the base flow and vault_id
        // is omitted from both the message and the payload.
        let message = match &self.vault_id {
            Some(vault_id) => {
                payload["vault_id"] = serde_json::json!(vault_id);
                format!("auth:{seed}:{ts}:{vault_id}")
            }
            None => format!("auth:{seed}:{ts}"),
        };
        let signature = self.sign_message(&message);
        payload["signature"] = serde_json::json!(signature);

        let encoded = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        format!("near:{encoded}")
    }

    async fn request(
        &self,
        seed: &str,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        self.request_keyed(seed, method, path, body, None)
            .await
            .map_err(|e| match e {
                Sent::NoAnswer(e) | Sent::Refused(e) => e,
            })
    }

    /// One request. `NoAnswer`: nothing came back, so the request may or may
    /// not have run — resend it only under the same `idempotency_key`.
    async fn request_keyed(
        &self,
        seed: &str,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
        idempotency_key: Option<&str>,
    ) -> Result<serde_json::Value, Sent> {
        let url = format!("{}{}", self.api_url, path);
        let bearer = self.make_bearer(seed);

        let mut req = match method {
            "GET" => self.http.get(&url),
            "POST" => self.http.post(&url),
            _ => return Err(Sent::Refused(format!("unsupported method: {method}"))),
        };

        req = req
            .header("Authorization", format!("Bearer {bearer}"))
            .header("Content-Type", "application/json");
        if let Some(key) = idempotency_key {
            req = req.header("X-Idempotency-Key", key);
        }

        if let Some(b) = body {
            req = req.json(&b);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| Sent::NoAnswer(format!("request failed: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            // Full body to logs (helps diagnose vault.parent mismatch, key not on
            // parent, stale signature, etc.); truncated form for the Err string
            // that flows up to user-facing handlers.
            tracing::error!(%status, method, path, body = %text, "outlayer error");
            return Err(Sent::Refused(format!(
                "{method} {path} → {status} {}",
                &text[..text.len().min(300)]
            )));
        }

        if text.is_empty() {
            return Ok(serde_json::json!({}));
        }

        serde_json::from_str(&text).map_err(|e| {
            Sent::Refused(format!("parse error: {e} (body: {})", &text[..text.len().min(200)]))
        })
    }

    // ── Public API ─────────────────────────────────────────────────

    /// No-op kept for handler-call-site compatibility. Outlayer auto-creates
    /// the sub-wallet on first `Bearer near:` request — see vault flow A.
    pub async fn register_wallet(&self, _tg_user_id: u64) -> Result<(), String> {
        Ok(())
    }

    /// Get token balance as a raw string (preserves u128 precision).
    pub async fn get_balance(&self, tg_user_id: u64, token: &str) -> Result<String, String> {
        let seed = self.seed_for_user(tg_user_id);
        let resp = self
            .request(
                &seed,
                "GET",
                &format!("/wallet/v1/balance?token={token}&source=intents"),
                None,
            )
            .await?;

        Ok(resp["balance"]
            .as_str()
            .unwrap_or("0")
            .to_string())
    }

    /// Create a payment check. Returns `(check_id, check_key)` once the check
    /// is funded and can be claimed.
    ///
    /// The funding can be slow to confirm. If it is not confirmed within the
    /// wait, this answers an error and a background task returns the check to
    /// its creator once the funding lands, so nothing is left stranded in it.
    pub async fn create_payment_check(
        &self,
        tg_user_id: u64,
        token: &str,
        amount: &str,
        memo: &str,
    ) -> Result<(String, String), String> {
        let seed = self.seed_for_user(tg_user_id);
        let resp = self
            .request(
                &seed,
                "POST",
                "/wallet/v1/payment-check/create",
                Some(serde_json::json!({
                    "token": token,
                    "amount": amount,
                    "memo": memo,
                })),
            )
            .await?;

        let check_id = resp["check_id"]
            .as_str()
            .ok_or("missing check_id")?
            .to_string();
        let check_key = resp["check_key"]
            .as_str()
            .ok_or("missing check_key")?
            .to_string();

        // `creating`: the funding was handed over and is not confirmed yet.
        // A coordinator that predates the field answers funded checks only.
        if resp["status"].as_str() == Some("creating") {
            match self.wait_for_check(tg_user_id, &check_id, SETTLE_WAIT).await? {
                CheckState::Funded => {}
                CheckState::NeverFunded => {
                    return Err(format!("check {check_id} was never funded; nothing moved"));
                }
                CheckState::StillFunding => {
                    let this = self.clone();
                    let id = check_id.clone();
                    tokio::spawn(async move { this.return_when_funded(tg_user_id, &id).await });
                    return Err(format!(
                        "check {check_id} is not funded yet; it goes back to its creator once it is"
                    ));
                }
            }
        }
        Ok((check_id, check_key))
    }

    /// Claim a check into the user's wallet.
    pub async fn claim_payment_check(
        &self,
        tg_user_id: u64,
        check_key: &str,
    ) -> Result<CheckTransfer, String> {
        self.check_transfer(
            tg_user_id,
            "/wallet/v1/payment-check/claim",
            serde_json::json!({ "check_key": check_key }),
            check_key,
        )
        .await
    }

    /// Return what a check still holds to its creator.
    pub async fn reclaim_payment_check(
        &self,
        tg_user_id: u64,
        check_id: &str,
    ) -> Result<CheckTransfer, String> {
        self.check_transfer(
            tg_user_id,
            "/wallet/v1/payment-check/reclaim",
            serde_json::json!({ "check_id": check_id }),
            check_id,
        )
        .await
    }

    /// A claim or reclaim, followed to its outcome.
    ///
    /// The answer can be `processing`: the transfer was handed over and is not
    /// confirmed. That is followed by its request until it settles or the wait
    /// runs out (`Pending`) — never sent again, which the server refuses while
    /// the check has a transfer in flight. A request that got no answer at all
    /// is resent under the same idempotency key, so a transfer that did start
    /// is found rather than started twice.
    async fn check_transfer(
        &self,
        tg_user_id: u64,
        path: &str,
        body: serde_json::Value,
        subject: &str,
    ) -> Result<CheckTransfer, String> {
        let seed = self.seed_for_user(tg_user_id);
        let key = idempotency_key(subject);
        let mut last = String::new();
        for _ in 0..3 {
            match self.request_keyed(&seed, "POST", path, Some(body.clone()), Some(&key)).await {
                Ok(resp) => return self.follow_transfer(tg_user_id, &resp).await,
                Err(Sent::Refused(e)) => return Err(e),
                Err(Sent::NoAnswer(e)) => {
                    last = e;
                    tokio::time::sleep(SETTLE_POLL).await;
                }
            }
        }
        Err(last)
    }

    async fn follow_transfer(&self, tg_user_id: u64, resp: &serde_json::Value) -> Result<CheckTransfer, String> {
        let request_id = match transfer_answer(resp) {
            TransferAnswer::Done => return Ok(CheckTransfer::Done),
            TransferAnswer::Follow(Some(id)) => id,
            TransferAnswer::Follow(None) => {
                return Err("the transfer is in flight but its request id was not returned".to_string())
            }
        };

        let seed = self.seed_for_user(tg_user_id);
        let started = tokio::time::Instant::now();
        loop {
            let req = self
                .request(&seed, "GET", &format!("/wallet/v1/requests/{request_id}"), None)
                .await?;
            match req["status"].as_str() {
                Some("completed") | Some("success") => return Ok(CheckTransfer::Done),
                Some("failed") => {
                    return Err(format!(
                        "transfer {request_id} failed: {}",
                        req["result"]["reason"]
                            .as_str()
                            .or(req["result"]["error"].as_str())
                            .unwrap_or("no reason given")
                    ))
                }
                Some("needs_review") => {
                    return Err(format!("transfer {request_id} needs review by OutLayer"));
                }
                _ => {}
            }
            if started.elapsed() >= SETTLE_WAIT {
                return Ok(CheckTransfer::Pending);
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }

    /// Where a just-created check stands.
    async fn wait_for_check(
        &self,
        tg_user_id: u64,
        check_id: &str,
        wait: Duration,
    ) -> Result<CheckState, String> {
        let seed = self.seed_for_user(tg_user_id);
        let started = tokio::time::Instant::now();
        loop {
            let check = self
                .request(&seed, "GET", &format!("/wallet/v1/payment-check/status?check_id={check_id}"), None)
                .await?;
            match check["status"].as_str() {
                Some("creating") => {}
                Some("failed") => return Ok(CheckState::NeverFunded),
                _ => return Ok(CheckState::Funded),
            }
            if started.elapsed() >= wait {
                return Ok(CheckState::StillFunding);
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }

    /// Return a check its creator gave up on once its funding lands.
    async fn return_when_funded(&self, tg_user_id: u64, check_id: &str) {
        match self.wait_for_check(tg_user_id, check_id, RETURN_WATCH).await {
            Ok(CheckState::Funded) => match self.reclaim_payment_check(tg_user_id, check_id).await {
                Ok(_) => tracing::info!(check_id, "returned a check whose funding confirmed late"),
                Err(e) => tracing::error!(check_id, "could not return a late-funded check: {e}"),
            },
            Ok(CheckState::NeverFunded) => tracing::info!(check_id, "check was never funded; nothing to return"),
            Ok(CheckState::StillFunding) => {
                tracing::error!(check_id, "check still not funded; reclaim it by hand once it is")
            }
            Err(e) => tracing::error!(check_id, "could not watch an unconfirmed check: {e}"),
        }
    }

    pub async fn withdraw(
        &self,
        tg_user_id: u64,
        token: &str,
        amount: &str,
        chain: &str,
        to: &str,
    ) -> Result<serde_json::Value, String> {
        let seed = self.seed_for_user(tg_user_id);
        self.request(
            &seed,
            "POST",
            "/wallet/v1/intents/withdraw",
            Some(serde_json::json!({
                "token": format!("nep141:{token}"),
                "amount": amount,
                "chain": chain,
                "to": to,
            })),
        )
        .await
    }

    /// Gasless swap via intents solver relay.
    pub async fn swap(
        &self,
        tg_user_id: u64,
        token_in: &str,
        token_out: &str,
        amount: &str,
    ) -> Result<serde_json::Value, String> {
        let seed = self.seed_for_user(tg_user_id);
        self.request(
            &seed,
            "POST",
            "/wallet/v1/intents/swap",
            Some(serde_json::json!({
                "token_in": format!("nep141:{token_in}"),
                "token_out": format!("nep141:{token_out}"),
                "amount_in": amount,
            })),
        )
        .await
    }

    pub async fn get_address(&self, tg_user_id: u64, chain: &str) -> Result<String, String> {
        let seed = self.seed_for_user(tg_user_id);
        let resp = self
            .request(
                &seed,
                "GET",
                &format!("/wallet/v1/address?chain={chain}"),
                None,
            )
            .await?;

        resp["address"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| "missing address in response".to_string())
    }
}

// ── Amount helpers (u128-safe) ─────────────────────────────────────

/// Parse "1.5" with `decimals` → raw amount string.
/// E.g. parse_amount("1.5", 6) → Some("1500000")
///      parse_amount("1.5", 24) → Some("1500000000000000000000000")
pub fn parse_amount(input: &str, decimals: u32) -> Option<u128> {
    let input = input.trim();
    let mut parts = input.splitn(2, '.');
    let whole: u128 = parts.next()?.parse().ok()?;
    let frac = parts.next().unwrap_or("0");
    let frac_len = frac.len() as u32;
    if frac_len > decimals {
        return None;
    }
    let frac_val: u128 = if frac.is_empty() {
        0
    } else {
        frac.parse().ok()?
    };
    Some(whole * 10u128.pow(decimals) + frac_val * 10u128.pow(decimals - frac_len))
}

/// Format raw amount with `decimals` for display.
/// E.g. format_amount("1500000", 6, 2) → "1.50"
///      format_amount("1500000000000000000000000", 24, 4) → "1.5000"
pub fn format_amount(raw: &str, decimals: u32, display_decimals: u32) -> String {
    let val: u128 = raw.parse().unwrap_or(0);
    let divisor = 10u128.pow(decimals);
    let whole = val / divisor;
    let frac = val % divisor;

    // Scale fractional part to display_decimals
    let display_div = 10u128.pow(decimals.saturating_sub(display_decimals));
    let display_frac = if display_div > 0 { frac / display_div } else { frac };

    format!("{whole}.{display_frac:0>width$}", width = display_decimals as usize)
}

/// If balance is within `dust` of required amount, use the full balance.
pub fn adjust_for_dust(balance: u128, amount: u128, dust: u128) -> u128 {
    if balance < amount && balance + dust > amount {
        balance
    } else {
        amount
    }
}

/// Why a request did not answer with a result.
enum Sent {
    /// Nothing came back: it may or may not have run.
    NoAnswer(String),
    /// The server answered with a refusal, or with something unreadable.
    Refused(String),
}

enum CheckState {
    Funded,
    NeverFunded,
    StillFunding,
}

/// A fresh idempotency key for one logical transfer, reused only to resend it.
fn idempotency_key(subject: &str) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let digest = Sha256::digest(format!("{subject}:{nanos}:{seq}").as_bytes());
    format!("tipbot-{}", hex_prefix(&digest))
}

fn hex_prefix(bytes: &[u8]) -> String {
    bytes.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// What a claim or reclaim answer says to do next.
#[derive(Debug, PartialEq)]
enum TransferAnswer {
    /// The transfer executed.
    Done,
    /// Follow the request (its id, when the answer named it) to its outcome.
    Follow(Option<String>),
}

fn transfer_answer(resp: &serde_json::Value) -> TransferAnswer {
    // A resend of a request that did run is answered with the request it was:
    // `{"error": "duplicate_idempotency_key", "message": "Request already processed: <id>"}`.
    if resp["error"].as_str() == Some("duplicate_idempotency_key") {
        let id = resp["message"].as_str().and_then(|m| m.rsplit(' ').next()).map(str::to_string);
        return TransferAnswer::Follow(id);
    }
    // Handed over and not confirmed. A server that predates the field answers
    // executed transfers only.
    if resp["status"].as_str() == Some("processing") {
        return TransferAnswer::Follow(resp["request_id"].as_str().map(str::to_string));
    }
    TransferAnswer::Done
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_settled_answer_is_done_from_either_server() {
        let old = json!({"token": "t", "amount_claimed": "5", "remaining": "0", "claimed_at": "x"});
        assert_eq!(transfer_answer(&old), TransferAnswer::Done);
        let new = json!({"request_id": "r", "status": "claimed", "token": "t", "amount_claimed": "5"});
        assert_eq!(transfer_answer(&new), TransferAnswer::Done);
    }

    #[test]
    fn an_unconfirmed_or_resent_transfer_is_followed_by_its_request() {
        let pending = json!({"request_id": "r-1", "status": "processing", "poll_url": "/wallet/v1/requests/r-1"});
        assert_eq!(transfer_answer(&pending), TransferAnswer::Follow(Some("r-1".to_string())));
        let resent = json!({"error": "duplicate_idempotency_key", "message": "Request already processed: 7d45cad4-3b67"});
        assert_eq!(transfer_answer(&resent), TransferAnswer::Follow(Some("7d45cad4-3b67".to_string())));
    }

    #[test]
    fn each_transfer_gets_its_own_key() {
        assert_ne!(idempotency_key("same"), idempotency_key("same"));
        assert!(idempotency_key("k").starts_with("tipbot-"));
    }
}
