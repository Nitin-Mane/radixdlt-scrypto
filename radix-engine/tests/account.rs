#[rustfmt::skip]
pub mod test_runner;

use crate::test_runner::TestRunner;
use radix_engine::engine::RuntimeError;
use scrypto::prelude::*;
use scrypto::to_struct;
use scrypto::values::ScryptoValue;
use transaction::builder::ManifestBuilder;
use transaction::model::*;

#[test]
fn can_withdraw_from_my_account() {
    // Arrange
    let mut test_runner = TestRunner::new(true);
    let (public_key, _, account) = test_runner.new_account();
    let (_, _, other_account) = test_runner.new_account();

    // Act
    let manifest = ManifestBuilder::new()
        .withdraw_from_account(RADIX_TOKEN, account)
        .call_method_with_all_resources(other_account, "deposit_batch")
        .build();
    let receipt = test_runner.execute_manifest(manifest, vec![public_key]);

    // Assert
    receipt.expect_success();

    let vault = test_runner.inspect_account_vault(account.into(), RADIX_TOKEN);
    assert_eq!(vault.resource_address(), RADIX_TOKEN);
    assert!(vault.is_empty());

    let other_vault = test_runner.inspect_account_vault(other_account.into(), RADIX_TOKEN);
    assert_eq!(other_vault.resource_address(), RADIX_TOKEN);
    assert!(!other_vault.is_empty());
}

#[test]
fn can_withdraw_non_fungible_from_my_account() {
    // Arrange
    let mut test_runner = TestRunner::new(true);
    let (public_key, _, account) = test_runner.new_account();
    let (_, _, other_account) = test_runner.new_account();
    let resource_address = test_runner.create_non_fungible_resource(account);

    // Act
    let manifest = ManifestBuilder::new()
        .withdraw_from_account(resource_address, account)
        .call_method_with_all_resources(other_account, "deposit_batch")
        .build();
    let receipt = test_runner.execute_manifest(manifest, vec![public_key]);

    // Assert
    receipt.expect_success();
}

#[test]
fn cannot_withdraw_from_other_account() {
    // Arrange
    let mut test_runner = TestRunner::new(true);
    let (_, _, account) = test_runner.new_account();
    let (other_public_key, _, other_account) = test_runner.new_account();
    let manifest = ManifestBuilder::new()
        .withdraw_from_account(RADIX_TOKEN, account)
        .call_method_with_all_resources(other_account, "deposit_batch")
        .build();

    // Act
    let receipt = test_runner.execute_manifest(manifest, vec![other_public_key]);

    // Assert
    let error = receipt.result.expect_err("Should be runtime error");
    assert_auth_error!(error);
}

#[test]
fn account_to_bucket_to_account() {
    // Arrange
    let mut test_runner = TestRunner::new(true);
    let (public_key, _, account) = test_runner.new_account();
    let manifest = ManifestBuilder::new()
        .withdraw_from_account(RADIX_TOKEN, account)
        .take_from_worktop(RADIX_TOKEN, |builder, bucket_id| {
            builder
                .add_instruction(Instruction::CallMethod {
                    component_address: account,
                    method_name: "deposit".to_string(),
                    arg: to_struct!(scrypto::resource::Bucket(bucket_id)),
                })
                .0
        })
        .build();

    // Act
    let receipt = test_runner.execute_manifest(manifest, vec![public_key]);

    // Assert
    receipt.expect_success();
}

#[test]
fn test_account_balance() {
    // Arrange
    let mut test_runner = TestRunner::new(true);
    let (public_key, _, account) = test_runner.new_account();
    let manifest = ManifestBuilder::new()
        .call_method(account, "balance", to_struct!(RADIX_TOKEN))
        .build();

    // Act
    let receipt = test_runner.execute_manifest(manifest, vec![public_key]);
    receipt.result.expect("Should be okay");

    // Assert
    assert_eq!(
        receipt.outputs[0],
        ScryptoValue::from_typed(&Decimal::from(1000000)).raw
    );
}
