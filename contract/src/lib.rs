use near_sdk::json_types::U128;
use near_sdk::{env, near, AccountId, Gas, NearToken, PanicOnDefault, Promise};

const GAS_WRAP: Gas = Gas::from_tgas(15);
const GAS_FT_TRANSFER: Gas = Gas::from_tgas(50);
const GAS_CALLBACK: Gas = Gas::from_tgas(15);

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct DepositHelper {
    wrap_near: AccountId,
    intents: AccountId,
}

#[near]
impl DepositHelper {
    #[init]
    pub fn new(wrap_near: AccountId, intents: AccountId) -> Self {
        Self { wrap_near, intents }
    }

    /// One-time setup: register this contract with wrap.near for storage.
    /// Attach >= 0.01 NEAR.
    #[payable]
    pub fn register_storage(&mut self) -> Promise {
        let args = format!(
            r#"{{"account_id":"{}"}}"#,
            env::current_account_id()
        );
        Promise::new(self.wrap_near.clone()).function_call(
            "storage_deposit".to_string(),
            args.into_bytes(),
            env::attached_deposit(),
            GAS_WRAP,
        )
    }

    /// Wrap attached NEAR → wNEAR → deposit to intents.
    ///
    /// `msg` is forwarded to `ft_transfer_call` — the intents account to credit
    /// (the user's Outlayer wallet NEAR implicit hex64 address).
    #[payable]
    pub fn deposit(&mut self, msg: String) -> Promise {
        let deposit = env::attached_deposit();
        assert!(
            deposit > NearToken::from_yoctonear(0),
            "Attach NEAR to deposit"
        );

        Promise::new(self.wrap_near.clone())
            .function_call("near_deposit".to_string(), b"{}".to_vec(), deposit, GAS_WRAP)
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FT_TRANSFER.saturating_add(GAS_CALLBACK))
                    .on_wrap(
                        U128(deposit.as_yoctonear()),
                        msg,
                        env::predecessor_account_id(),
                    ),
            )
    }

    #[private]
    pub fn on_wrap(&mut self, amount: U128, msg: String, sender: AccountId) -> Promise {
        match env::promise_result_checked(0, 0) {
            Ok(_) => {
                let args = format!(
                    r#"{{"receiver_id":"{}","amount":"{}","msg":"{}"}}"#,
                    self.intents, amount.0, msg
                );
                Promise::new(self.wrap_near.clone()).function_call(
                    "ft_transfer_call".to_string(),
                    args.into_bytes(),
                    NearToken::from_yoctonear(1),
                    GAS_FT_TRANSFER,
                )
            }
            _ => {
                env::log_str(&format!("wrap failed, refunding {} to {}", amount.0, sender));
                Promise::new(sender).transfer(NearToken::from_yoctonear(amount.0))
            }
        }
    }
}
