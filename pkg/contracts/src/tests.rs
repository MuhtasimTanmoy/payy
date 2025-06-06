use crate::util::{convert_element_to_h256, convert_h160_to_element};
use secp256k1::rand::random;
use secp256k1::PublicKey;
use smirk::Element;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use testutil::eth::{EthNode, EthNodeOptions};
use testutil::ACCOUNT_1_SK;
use web3::contract::tokens::Tokenizable;
use web3::ethabi::Token;
use web3::signing::{keccak256, SecretKey};
use web3::types::Address;
use zk_circuits::constants::MERKLE_TREE_DEPTH;
use zk_circuits::data::{BurnTo, Mint, ParameterSet};
use zk_circuits::test::rollup::Rollup;

use super::*;

struct Env {
    _eth_node: Arc<EthNode>,
    evm_secret_key: SecretKey,
    evm_address: Address,
    rollup_contract: RollupContract,
    usdc_contract: USDCContract,
}

async fn make_env(options: EthNodeOptions) -> Env {
    let eth_node = EthNode::new(options).run_and_deploy().await;

    let evm_secret_key = SecretKey::from_str(ACCOUNT_1_SK).unwrap();
    let evm_address = to_address(&evm_secret_key);

    let rollup_contract = RollupContract::from_eth_node(&eth_node, evm_secret_key)
        .await
        .unwrap();
    let usdc_contract = USDCContract::from_eth_node(&eth_node, evm_secret_key)
        .await
        .unwrap();

    Env {
        _eth_node: eth_node,
        evm_secret_key,
        evm_address,
        rollup_contract,
        usdc_contract,
    }
}

fn to_address(secret_key: &SecretKey) -> Address {
    let secret_key_bytes = secret_key.secret_bytes();
    let secp = secp256k1::Secp256k1::new();
    let secret_key = secp256k1::SecretKey::from_slice(&secret_key_bytes).unwrap();
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    let serialized_public_key = public_key.serialize_uncompressed();

    // Ethereum address is the last 20 bytes of the Keccak hash of the public key
    let address_bytes = &keccak256(&serialized_public_key[1..])[12..];
    Address::from_slice(address_bytes)
}

async fn sign_block(new_root: &Element, height: u64, other_hash: [u8; 32]) -> Vec<u8> {
    let env = make_env(EthNodeOptions::default()).await;

    let proposal_hash = keccak256(&{
        let mut bytes = vec![];
        bytes.extend_from_slice(convert_element_to_h256(new_root).as_bytes());

        let mut height_bytes = [0u8; 32];
        U256::from(height).to_big_endian(&mut height_bytes);
        bytes.extend_from_slice(&height_bytes);

        bytes.extend_from_slice(&other_hash);
        bytes
    });

    let accept_hash = keccak256(&{
        let mut bytes = vec![];

        let mut height_bytes = [0u8; 32];
        U256::from(height + 1).to_big_endian(&mut height_bytes);
        bytes.extend_from_slice(&height_bytes);

        bytes.extend_from_slice(&proposal_hash);

        bytes
    });

    let msg = keccak256(&{
        let mut bytes = vec![];
        bytes.extend_from_slice(&("Polybase".len() as u64).to_be_bytes());
        bytes.extend_from_slice(b"Polybase");
        bytes.extend_from_slice(&accept_hash);
        bytes
    });

    let sig = secp256k1::SECP256K1.sign_ecdsa_recoverable(
        &secp256k1::Message::from_digest(msg),
        &secp256k1::SecretKey::from_slice(&env.evm_secret_key.secret_bytes()).unwrap(),
    );
    let (recovery, r_s) = sig.serialize_compact();
    let mut sig = vec![0u8; 65];
    sig[0..64].copy_from_slice(&r_s[0..64]);
    sig[64] = recovery.to_i32() as u8;
    sig
}

#[tokio::test]
async fn root_hashes() {
    let env: Env = make_env(EthNodeOptions::default()).await;

    let _root_hashes = env.rollup_contract.root_hashes().await.unwrap();
}

#[tokio::test]
async fn root_hash() {
    let env = make_env(EthNodeOptions::default()).await;

    let _root_hash = env.rollup_contract.root_hash().await.unwrap();
}

#[tokio::test]
async fn height() {
    let env = make_env(EthNodeOptions::default()).await;

    let _height = env.rollup_contract.block_height().await.unwrap();
}

#[tokio::test]

async fn verify_transfers() {
    let env = make_env(EthNodeOptions::default()).await;
    let params_21 = zk_circuits::data::ParameterSet::TwentyOne;

    let utxo_aggs = zk_circuits::test::agg_utxo::create_or_load_agg_utxo_snarks(params_21);

    let aggregate_agg =
        zk_circuits::test::agg_agg::create_or_load_agg_agg_utxo_snark(params_21, utxo_aggs);

    let aggregate_agg_agg = zk_circuits::test::agg_agg::create_or_load_agg_agg_final_evm_proof(
        params_21,
        aggregate_agg,
    )
    .try_as_v_1()
    .unwrap();

    // Public inputs
    let agg_instances = aggregate_agg_agg.agg_instances;
    let agg_instances: Vec<_> = agg_instances.into_iter().map(From::from).collect();
    let old_root = aggregate_agg_agg.old_root;
    let new_root = aggregate_agg_agg.new_root;
    let utxo_inputs = aggregate_agg_agg.utxo_inputs;
    let proof = aggregate_agg_agg.proof;

    // Sign
    let other_hash = [0u8; 32];
    let height = 1;
    let sig = sign_block(&new_root, height, other_hash).await;

    // Set the root, we add some pre-existing values to the tree before generating the UTXO,
    // so the tree is not empty
    env.rollup_contract.set_root(&old_root).await.unwrap();

    env.rollup_contract
        .verify_block(
            &proof,
            agg_instances.try_into().unwrap(),
            &old_root,
            &new_root,
            &utxo_inputs,
            other_hash,
            height,
            &[&sig],
            500_000,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn mint_with_authorization() {
    let env = make_env(EthNodeOptions::default()).await;
    let rollup = Rollup::new();
    let bob = rollup.new_wallet();

    let amount = 10 * 10u64.pow(6);
    let note = bob.new_note(amount);
    let mint = Mint::new([note.clone()]);
    let params = ParameterSet::Eight;
    let proof = mint.evm_proof(params).unwrap();

    let secret_key = secp256k1::SecretKey::from_slice(&env.evm_secret_key.secret_bytes()).unwrap();

    let nonce = random();
    let valid_after = U256::from(0);
    let valid_before = U256::from(u64::MAX);

    // Sig for the USDC function
    let sig_bytes = env.usdc_contract.signature_for_receive(
        env.evm_address,
        env.rollup_contract.address(),
        amount.into(),
        valid_after,
        valid_before,
        nonce,
        secret_key,
    );

    // Sig for our mint function
    let mint_sig_bytes = env.rollup_contract.signature_for_mint(
        note.commitment(),
        amount.into(),
        note.source(),
        env.evm_address,
        valid_after,
        valid_before,
        nonce,
        secret_key,
    );

    env.rollup_contract
        .mint_with_authorization(
            &proof,
            &note.commitment(),
            &note.value(),
            &note.source(),
            &env.evm_address,
            U256::from(0),
            U256::from(u64::MAX),
            nonce,
            &sig_bytes,
            &mint_sig_bytes,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn mint_from() {
    let env = make_env(EthNodeOptions::default()).await;
    let rollup = Rollup::new();
    let bob = rollup.new_wallet();

    // Create the proof
    let note = bob.new_note(10 * 10u64.pow(6));
    let mint = Mint::new([note.clone()]);
    let params = ParameterSet::Eight;
    let proof = mint.evm_proof(params).unwrap();

    env.usdc_contract
        .approve_max(env.rollup_contract.address())
        .await
        .unwrap();

    env.rollup_contract
        .mint(&proof, &note.commitment(), &note.value(), &note.source())
        .await
        .unwrap();
}

#[tokio::test]
async fn burn_to() {
    let env = make_env(EthNodeOptions::default()).await;

    // Create the proof
    let mut rollup = Rollup::new();
    let bob = rollup.new_wallet();

    let bob_note = rollup.unverified_add_unspent_note(&bob, 100);

    // Set the root, we add some pre-existing values to the tree before generating the UTXO,
    // so the tree is not empty
    env.rollup_contract
        .set_root(&rollup.root_hash())
        .await
        .unwrap();

    let note = bob_note.note();
    let burn = BurnTo {
        notes: [note.clone()],
        secret_key: bob.pk,
        to_address: convert_h160_to_element(&env.evm_address),
        kind: Element::ZERO,
    };

    let proof = burn.evm_proof(ParameterSet::Nine).unwrap();

    env.rollup_contract
        .burn_to_address(
            &burn.kind,
            &burn.to_address,
            &proof,
            &note.nullifier(bob.pk),
            &note.value(),
            &note.source(),
            &burn.signature(&note),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn burn_to_router() {
    let env = make_env(EthNodeOptions::default()).await;

    // Create the proof
    let mut rollup = Rollup::new();
    let bob = rollup.new_wallet();

    let bob_note = rollup.unverified_add_unspent_note(&bob, 100);

    // Set the root, we add some pre-existing values to the tree before generating the UTXO,
    // so the tree is not empty
    env.rollup_contract
        .set_root(&rollup.root_hash())
        .await
        .unwrap();

    let owner = env.evm_address;
    let router = Address::from_str("4a679253410272dd5232b3ff7cf5dbb88f295319").unwrap();
    let return_address = Address::from_str("0000000000000000000000000000000000000001").unwrap();

    let mut router_calldata = keccak256(b"burnToAddress(address,address,uint256)")[0..4].to_vec();
    router_calldata.extend_from_slice(&web3::ethabi::encode(&[
        Address::from_str("09635f643e140090a9a8dcd712ed6285858cebef")
            .unwrap()
            .into_token(),
        owner.into_token(),
        convert_element_to_h256(&bob_note.note().value).into_token(),
    ]));

    let msg = web3::ethabi::encode(&[
        Token::Address(router),
        Token::Bytes(router_calldata.clone()),
        Token::Address(return_address),
    ]);

    let mut msg_hash = keccak256(&msg);
    // Bn256 can't fit the full hash, so we remove the first 3 bits
    msg_hash[0] &= 0x1f; // 0b11111

    let note = bob_note.note();
    let burn = BurnTo {
        notes: [note.clone()],
        secret_key: bob.pk,
        to_address: Element::from_be_bytes(msg_hash),
        kind: Element::ONE,
    };

    let proof = burn.evm_proof(ParameterSet::Nine).unwrap();

    env.rollup_contract
        .burn_to_router(
            &burn.kind,
            &burn.to_address,
            &proof,
            &note.nullifier(bob.pk),
            &note.value(),
            &note.source(),
            &burn.signature(&note),
            &router,
            &router_calldata,
            &Address::from_str("0000000000000000000000000000000000000001").unwrap(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn substitute_burn() {
    let env = make_env(EthNodeOptions {
        use_noop_verifier: true,
        ..Default::default()
    })
    .await;

    // Create the proof
    let mut rollup = Rollup::new();
    let bob = rollup.new_wallet();

    let bob_note = rollup.unverified_add_unspent_note(&bob, 100);

    // Set the root, we add some pre-existing values to the tree before generating the UTXO,
    // so the tree is not empty
    env.rollup_contract
        .set_root(&rollup.root_hash())
        .await
        .unwrap();

    let owner = Address::from_str("1111111111111111111111111111111111111111").unwrap();
    let router = Address::from_str("4a679253410272dd5232b3ff7cf5dbb88f295319").unwrap();
    let return_address = Address::from_str("0000000000000000000000000000000000000001").unwrap();

    let mut router_calldata = keccak256(b"burnToAddress(address,address,uint256)")[0..4].to_vec();
    router_calldata.extend_from_slice(&web3::ethabi::encode(&[
        env.usdc_contract.address().into_token(),
        owner.into_token(),
        convert_element_to_h256(&bob_note.note().value).into_token(),
    ]));

    let msg = web3::ethabi::encode(&[
        Token::Address(router),
        Token::Bytes(router_calldata.clone()),
        Token::Address(return_address),
    ]);

    let mut msg_hash = keccak256(&msg);
    // Bn256 can't fit the full hash, so we remove the first 3 bits
    msg_hash[0] &= 0x1f; // 0b11111

    let note = bob_note.note();
    let burn = BurnTo {
        notes: [note.clone()],
        secret_key: bob.pk,
        to_address: Element::from_be_bytes(msg_hash),
        kind: Element::ONE,
    };

    let proof = burn.evm_proof(ParameterSet::Nine).unwrap();

    let nullifier = note.nullifier(bob.pk);
    env.rollup_contract
        .burn_to_router(
            &burn.kind,
            &burn.to_address,
            &proof,
            &nullifier,
            &note.value(),
            &note.source(),
            &burn.signature(&note),
            &router,
            &router_calldata,
            &Address::from_str("0000000000000000000000000000000000000001").unwrap(),
        )
        .await
        .unwrap();

    let owner_balance_pre_substitute = env.usdc_contract.balance(owner).await.unwrap();
    assert_eq!(owner_balance_pre_substitute, U256::from(0));

    let substitutor_balance_pre_substitute =
        env.usdc_contract.balance(env.evm_address).await.unwrap();

    let rollup_balance_pre_substitute = env
        .usdc_contract
        .balance(env.rollup_contract.address())
        .await
        .unwrap();

    env.usdc_contract
        .approve_max(env.rollup_contract.address())
        .await
        .unwrap();

    assert!(!env
        .rollup_contract
        .was_burn_substituted(&nullifier)
        .await
        .unwrap());

    let txn = env
        .rollup_contract
        .substitute_burn(&nullifier, &note.value())
        .await
        .unwrap();

    while env
        .rollup_contract
        .client
        .client()
        .eth()
        .transaction_receipt(txn)
        .await
        .unwrap()
        .is_none()
    {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    assert_eq!(
        env.usdc_contract.balance(owner).await.unwrap(),
        U256::from(100)
    );
    assert_eq!(
        env.usdc_contract.balance(env.evm_address).await.unwrap(),
        substitutor_balance_pre_substitute - U256::from(100)
    );

    assert_eq!(
        env.usdc_contract
            .balance(env.rollup_contract.address())
            .await
            .unwrap(),
        rollup_balance_pre_substitute
    );

    assert!(env
        .rollup_contract
        .was_burn_substituted(&nullifier)
        .await
        .unwrap());
}

#[tokio::test]
async fn set_validators() {
    let env = make_env(EthNodeOptions::default()).await;

    // let's also test the worker
    let worker_rollup_contract = env.rollup_contract.clone();
    let _worker = tokio::spawn(async move {
        worker_rollup_contract
            .worker(Duration::from_millis(100))
            .await
    });

    let validator_sets_before = env.rollup_contract.get_validator_sets(0).await.unwrap();
    assert_eq!(
        validator_sets_before,
        *env.rollup_contract.validator_sets.read()
    );

    let valid_from = validator_sets_before.last().unwrap().valid_from + 2;
    let tx = env
        .rollup_contract
        .set_validators(valid_from.as_u64(), &[env.evm_address])
        .await
        .unwrap();

    // Wait for receipt
    while env
        .rollup_contract
        .client
        .client()
        .eth()
        .transaction_receipt(tx)
        .await
        .unwrap()
        .is_none()
    {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let validator_sets_after = env
        .rollup_contract
        .get_validator_sets(validator_sets_before.len() as u64)
        .await
        .unwrap();
    assert_eq!(validator_sets_after.last().unwrap().valid_from, valid_from);
    assert_eq!(
        validator_sets_after.last().unwrap().validators,
        vec![env.evm_address]
    );

    // Wait for worker to update the validator sets
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // Make sure the worker updated the contract's state
    assert_eq!(
        validator_sets_before
            .into_iter()
            .chain(validator_sets_after)
            .collect::<Vec<_>>(),
        *env.rollup_contract.validator_sets.read()
    );
}

#[test]
fn empty_root() {
    let tree = smirk::Tree::<MERKLE_TREE_DEPTH, ()>::new();
    let hash = expect_test::expect_file!["./empty_merkle_tree_root_hash.txt"];
    hash.assert_eq(format!("{:?}", tree.root_hash().to_base()).as_str());
}
