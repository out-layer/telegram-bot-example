use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct OutlayerClient {
    account_id: String,
    signing_key: SigningKey,
    pubkey_str: String,
    api_url: String,
    http: Client,
}

impl OutlayerClient {
    pub fn new(account_id: String, private_key_str: &str, api_url: String) -> Self {
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
        let message = format!("auth:{seed}:{ts}");
        let signature = self.sign_message(&message);

        let payload = serde_json::json!({
            "account_id": self.account_id,
            "seed": seed,
            "pubkey": self.pubkey_str,
            "timestamp": ts,
            "signature": signature,
        });

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
        let url = format!("{}{}", self.api_url, path);
        let bearer = self.make_bearer(seed);

        let mut req = match method {
            "GET" => self.http.get(&url),
            "POST" => self.http.post(&url),
            _ => return Err(format!("unsupported method: {method}")),
        };

        req = req
            .header("Authorization", format!("Bearer {bearer}"))
            .header("Content-Type", "application/json");

        if let Some(b) = body {
            req = req.json(&b);
        }

        let resp = req.send().await.map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(format!(
                "{method} {path} → {status} {}",
                &text[..text.len().min(300)]
            ));
        }

        if text.is_empty() {
            return Ok(serde_json::json!({}));
        }

        serde_json::from_str(&text)
            .map_err(|e| format!("parse error: {e} (body: {})", &text[..text.len().min(200)]))
    }

    // ── Public API ─────────────────────────────────────────────────

    pub async fn register_wallet(&self, tg_user_id: u64) -> Result<serde_json::Value, String> {
        let seed = self.seed_for_user(tg_user_id);
        let ts = Self::timestamp();
        let message = format!("register:{seed}:{ts}");
        let signature = self.sign_message(&message);

        let url = format!("{}/register", self.api_url);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "account_id": self.account_id,
                "seed": seed,
                "pubkey": self.pubkey_str,
                "message": message,
                "signature": signature,
            }))
            .send()
            .await
            .map_err(|e| format!("register failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(format!("register → {status} {text}"));
        }

        serde_json::from_str(&text).map_err(|e| format!("register parse: {e}"))
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

    /// Create a payment check. Returns `(check_id, check_key)`.
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
        Ok((check_id, check_key))
    }

    pub async fn claim_payment_check(
        &self,
        tg_user_id: u64,
        check_key: &str,
    ) -> Result<serde_json::Value, String> {
        let seed = self.seed_for_user(tg_user_id);
        self.request(
            &seed,
            "POST",
            "/wallet/v1/payment-check/claim",
            Some(serde_json::json!({ "check_key": check_key })),
        )
        .await
    }

    pub async fn reclaim_payment_check(
        &self,
        tg_user_id: u64,
        check_key: &str,
    ) -> Result<serde_json::Value, String> {
        let seed = self.seed_for_user(tg_user_id);
        self.request(
            &seed,
            "POST",
            "/wallet/v1/payment-check/reclaim",
            Some(serde_json::json!({ "check_key": check_key })),
        )
        .await
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
