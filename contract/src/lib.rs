use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::json_types::U128;
use near_sdk::{env, near_bindgen, AccountId, Gas, NearToken, PanicOnDefault, Promise};

const GAS_WRAP: Gas = Gas::from_tgas(15);
const GAS_FT_TRANSFER: Gas = Gas::from_tgas(50);
const GAS_CALLBACK: Gas = Gas::from_tgas(15);

#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize, PanicOnDefault)]
pub struct DepositHelper {
    owner: AccountId,
    wrap_near: AccountId,
    intents: AccountId,
    deposits_enabled: bool,
}

#[near_bindgen]
impl DepositHelper {
    #[init]
    pub fn new(wrap_near: AccountId, intents: AccountId) -> Self {
        Self {
            owner: env::predecessor_account_id(),
            wrap_near,
            intents,
            deposits_enabled: true,
        }
    }

    pub fn set_deposits_enabled(&mut self, enabled: bool) {
        assert_eq!(
            env::predecessor_account_id(),
            self.owner,
            "Only owner"
        );
        self.deposits_enabled = enabled;
    }

    pub fn get_deposits_enabled(&self) -> bool {
        self.deposits_enabled
    }

    /// Wrap attached NEAR → wNEAR → deposit to intents.
    /// `msg` = the intents account to credit (hex64 Outlayer wallet address).
    #[payable]
    pub fn deposit(&mut self, msg: String) -> Promise {
        assert!(self.deposits_enabled, "Deposits are paused");

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
        if env::promise_results_count() == 1
            && matches!(env::promise_result(0), near_sdk::PromiseResult::Successful(_))
        {
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
        } else {
            env::log_str(&format!("wrap failed, refunding {} to {}", amount.0, sender));
            Promise::new(sender).transfer(NearToken::from_yoctonear(amount.0))
        }
    }
}
