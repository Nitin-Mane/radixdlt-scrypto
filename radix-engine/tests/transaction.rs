#[rustfmt::skip]
pub mod test_runner;

use radix_engine::engine::TransactionExecutor;
use radix_engine::ledger::InMemorySubstateStore;
use radix_engine::wasm::DefaultWasmEngine;
use scrypto::prelude::*;
use test_runner::{TestEpochManager, TestIntentHashManager};
use transaction::builder::ManifestBuilder;
use transaction::builder::TransactionBuilder;
use transaction::model::Network;
use transaction::model::TransactionHeader;
use transaction::signing::EcdsaPrivateKey;
use transaction::validation::TransactionValidator;

#[test]
fn test_normal_transaction_flow() {
    let mut substate_store = InMemorySubstateStore::with_bootstrap();
    let mut wasm_engine = DefaultWasmEngine::new();
    let epoch_manager = TestEpochManager::new(0);
    let intent_hash_manager = TestIntentHashManager::new();

    let raw_transaction = create_transaction();
    let validated_transaction = TransactionValidator::validate_from_slice(
        &raw_transaction,
        &intent_hash_manager,
        &epoch_manager,
    )
    .expect("Invalid transaction");

    let mut executor = TransactionExecutor::new(&mut substate_store, &mut wasm_engine, true);
    let receipt = executor.execute(&validated_transaction);

    receipt.result.expect("Transaction failed");
}

fn create_transaction() -> Vec<u8> {
    // create key pairs
    let sk1 = EcdsaPrivateKey::from_u64(1).unwrap();
    let sk2 = EcdsaPrivateKey::from_u64(2).unwrap();
    let sk_notary = EcdsaPrivateKey::from_u64(3).unwrap();

    let transaction = TransactionBuilder::new()
        .header(TransactionHeader {
            version: 1,
            network: Network::InternalTestnet,
            start_epoch_inclusive: 0,
            end_epoch_exclusive: 100,
            nonce: 5,
            notary_public_key: sk_notary.public_key(),
        })
        .manifest(ManifestBuilder::new().clear_auth_zone().build())
        .sign(&sk1)
        .sign(&sk2)
        .notarize(&sk_notary)
        .build();

    transaction.to_bytes()
}