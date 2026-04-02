#!/bin/bash
set -e

echo "Building deposit-helper..."

cargo near build non-reproducible-wasm --no-abi

mkdir -p res
cp target/near/deposit_helper.wasm res/deposit_helper.wasm

ls -lh res/deposit_helper.wasm
echo "Build complete: res/deposit_helper.wasm"

# near contract deploy deposit.tipbot.near use-file res/deposit_helper.wasm without-init-call network-config mainnet sign-with-keychain send
# near contract call-function as-transaction deposit.tipbot.near new json-args '{"wrap_near":"wrap.near","intents":"intents.near"}' prepaid-gas '30.0 Tgas' attached-deposit '0 NEAR' sign-as deposit.tipbot.near network-config mainnet sign-with-keychain send
# near contract call-function as-transaction wrap.near storage_deposit json-args '{"account_id":"deposit.tipbot.near"}' prepaid-gas '30.0 Tgas' attached-deposit '0.00125 NEAR' sign-as deposit.tipbot.near network-config mainnet sign-with-keychain send

# key
# near account add-key deposit.tipbot.near grant-function-call-access --allowance unlimited --contract-account-id deposit.tipbot.near --function-names register autogenerate-new-keypair save-to-legacy-keychain network-config mainnet sign-with-keychain send