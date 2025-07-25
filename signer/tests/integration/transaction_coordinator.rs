use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;

use assert_matches::assert_matches;
use bitcoin::Address;
use bitcoin::AddressType;
use bitcoin::Amount;
use bitcoin::BlockHash;
use bitcoin::Transaction;
use bitcoin::hashes::Hash as _;
use bitcoincore_rpc::RpcApi as _;
use bitcoincore_rpc_json::GetChainTipsResultTip;
use bitvec::array::BitArray;
use blockstack_lib::chainstate::nakamoto::NakamotoBlock;
use blockstack_lib::chainstate::nakamoto::NakamotoBlockHeader;
use blockstack_lib::chainstate::stacks::StacksTransaction;
use blockstack_lib::chainstate::stacks::TokenTransferMemo;
use blockstack_lib::chainstate::stacks::TransactionPayload;
use blockstack_lib::net::api::getcontractsrc::ContractSrcResponse;
use blockstack_lib::net::api::getpoxinfo::RPCPoxInfoData;
use blockstack_lib::net::api::getsortition::SortitionInfo;
use clarity::types::chainstate::StacksAddress;
use clarity::types::chainstate::StacksBlockId;
use clarity::vm::Value as ClarityValue;
use clarity::vm::types::PrincipalData;
use clarity::vm::types::SequenceData;
use clarity::vm::types::StacksAddressExtensions;
use clarity::vm::types::StandardPrincipalData;
use emily_client::apis::deposit_api;
use fake::Fake;
use fake::Faker;
use futures::StreamExt as _;
use lru::LruCache;
use more_asserts::assert_lt;
use rand::rngs::OsRng;

use sbtc::deposits::CreateDepositRequest;
use sbtc::deposits::DepositScriptInputs;
use sbtc::deposits::ReclaimScriptInputs;
use sbtc::testing::regtest;
use sbtc::testing::regtest::AsUtxo as _;
use sbtc::testing::regtest::Recipient;
use sbtc::testing::regtest::p2wpkh_sign_transaction;
use secp256k1::Keypair;
use secp256k1::SECP256K1;
use signer::bitcoin::BitcoinInteract as _;
use signer::bitcoin::rpc::BitcoinCoreClient;
use signer::bitcoin::utxo::BitcoinInputsOutputs;
use signer::bitcoin::utxo::DepositRequest;
use signer::bitcoin::utxo::Fees;
use signer::bitcoin::utxo::TxDeconstructor as _;
use signer::bitcoin::validation::WithdrawalValidationResult;
use signer::block_observer;
use signer::context::P2PEvent;
use signer::context::RequestDeciderEvent;
use signer::context::SignerEvent;
use signer::context::SignerSignal;
use signer::message::Payload;
use signer::network::MessageTransfer;
use signer::stacks::api::SignerSetInfo;
use signer::stacks::api::StacksClient;
use signer::stacks::api::StacksInteract;
use signer::stacks::wallet::SignerWallet;
use signer::storage::model::KeyRotationEvent;
use signer::storage::model::WithdrawalTxOutput;
use signer::testing::btc::get_canonical_chain_tip;
use signer::testing::get_rng;

use signer::testing::FutureExt as _;
use signer::testing::FuturesIterExt as _;
use signer::testing::Sleep;
use signer::transaction_coordinator::given_key_is_coordinator;
use signer::transaction_coordinator::should_coordinate_dkg;
use signer::transaction_signer::STACKS_SIGN_REQUEST_LRU_SIZE;
use signer::transaction_signer::assert_allow_dkg_begin;
use signer::wsts_state_machine::construct_signing_round_id;
use testing_emily_client::apis::chainstate_api;
use testing_emily_client::apis::testing_api;
use testing_emily_client::apis::withdrawal_api;
use testing_emily_client::models::Chainstate;
use testing_emily_client::models::WithdrawalStatus as TestingEmilyWithdrawalStatus;

use signer::WITHDRAWAL_BLOCKS_EXPIRY;
use signer::WITHDRAWAL_MIN_CONFIRMATIONS;
use signer::context::SbtcLimits;
use signer::context::TxCoordinatorEvent;
use signer::keys::PrivateKey;
use signer::network::in_memory2::SignerNetwork;
use signer::network::in_memory2::WanNetwork;
use signer::request_decider::RequestDeciderEventLoop;
use signer::stacks::api::TenureBlocks;
use signer::stacks::contracts::AcceptWithdrawalV1;
use signer::stacks::contracts::AsContractCall;
use signer::stacks::contracts::RejectWithdrawalV1;
use signer::stacks::contracts::RotateKeysV1;
use signer::storage::DbRead;
use signer::storage::DbWrite;
use signer::storage::model::BitcoinBlockHash;
use signer::storage::model::BitcoinTxSigHash;
use signer::storage::model::DkgSharesStatus;
use signer::storage::model::StacksTxId;
use signer::storage::model::WithdrawalRequest;
use signer::storage::postgres::PgStore;
use signer::testing::IterTestExt as _;
use signer::testing::stacks::DUMMY_SORTITION_INFO;
use signer::testing::stacks::DUMMY_TENURE_INFO;
use signer::testing::storage::DbReadTestExt as _;
use signer::testing::storage::DbWriteTestExt as _;
use signer::testing::transaction_coordinator::select_coordinator;
use signer::testing::wsts::SignerInfo;
use stacks_common::types::chainstate::BurnchainHeaderHash;
use stacks_common::types::chainstate::ConsensusHash;
use stacks_common::types::chainstate::SortitionId;
use test_case::test_case;
use test_log::test;
use tokio_stream::wrappers::BroadcastStream;
use url::Url;

use signer::block_observer::BlockObserver;
use signer::context::Context;
use signer::emily_client::EmilyClient;
use signer::error::Error;
use signer::keys;
use signer::keys::PublicKey;
use signer::keys::SignerScriptPubKey as _;
use signer::network;
use signer::network::in_memory::InMemoryNetwork;
use signer::stacks::api::AccountInfo;
use signer::stacks::api::MockStacksInteract;
use signer::stacks::api::SubmitTxResponse;
use signer::stacks::contracts::CompleteDepositV1;
use signer::stacks::contracts::SMART_CONTRACTS;
use signer::storage::model;
use signer::storage::model::EncryptedDkgShares;
use signer::testing;
use signer::testing::context::*;
use signer::testing::storage::model::TestData;
use signer::testing::transaction_signer::TxSignerEventLoopHarness;
use signer::testing::wsts::SignerSet;
use signer::transaction_coordinator;
use signer::transaction_coordinator::TxCoordinatorEventLoop;
use signer::transaction_signer::TxSignerEventLoop;
use tokio::sync::broadcast::Sender;

use crate::complete_deposit::make_complete_deposit;
use crate::contracts::SignerStxState;
use crate::setup::AsBlockRef as _;
use crate::setup::IntoEmilyTestingConfig as _;
use crate::setup::SweepAmounts;
use crate::setup::TestSignerSet;
use crate::setup::TestSweepSetup;
use crate::setup::TestSweepSetup2;
use crate::setup::WithdrawalTriple;
use crate::setup::backfill_bitcoin_blocks;
use crate::setup::fetch_canonical_bitcoin_blockchain;
use crate::setup::set_deposit_completed;
use crate::setup::set_deposit_incomplete;
use crate::utxo_construction::generate_withdrawal;
use crate::utxo_construction::make_deposit_request;
use crate::zmq::BITCOIN_CORE_ZMQ_ENDPOINT;

type IntegrationTestContext<Stacks> = TestContext<PgStore, BitcoinCoreClient, Stacks, EmilyClient>;

pub const GET_POX_INFO_JSON: &str =
    include_str!("../../tests/fixtures/stacksapi-get-pox-info-test-data.json");

async fn run_dkg<Rng, C>(
    ctx: &C,
    rng: &mut Rng,
    signer_set: &mut SignerSet,
) -> (keys::PublicKey, model::BitcoinBlockRef)
where
    C: Context + Send + Sync,
    Rng: rand::CryptoRng + rand::RngCore,
{
    let storage = ctx.get_storage_mut();

    let bitcoin_chain_tip = storage
        .get_bitcoin_canonical_chain_tip()
        .await
        .expect("storage error")
        .expect("no chain tip");

    let bitcoin_chain_tip_ref = storage
        .get_bitcoin_block(&bitcoin_chain_tip)
        .await
        .expect("storage failure")
        .expect("missing block")
        .into();

    let dkg_txid = testing::dummy::txid(&fake::Faker, rng);
    let (aggregate_key, all_dkg_shares) = signer_set
        .run_dkg(
            bitcoin_chain_tip,
            dkg_txid.into(),
            model::DkgSharesStatus::Verified,
        )
        .await;

    let encrypted_dkg_shares = all_dkg_shares.first().unwrap();
    signer_set
        .write_as_rotate_keys_tx(&storage, &bitcoin_chain_tip, encrypted_dkg_shares, rng)
        .await;

    let encrypted_dkg_shares = all_dkg_shares.first().unwrap();

    storage
        .write_encrypted_dkg_shares(encrypted_dkg_shares)
        .await
        .expect("failed to write encrypted shares");

    (aggregate_key, bitcoin_chain_tip_ref)
}

async fn push_utxo_donation<C>(ctx: &C, aggregate_key: &PublicKey, block_hash: &bitcoin::BlockHash)
where
    C: Context + Send + Sync,
{
    let tx = Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(1_337_000_000_000),
            script_pubkey: aggregate_key.signers_script_pubkey(),
        }],
    };

    let bitcoin_transaction = model::BitcoinTxRef {
        txid: tx.compute_txid().into(),
        block_hash: (*block_hash).into(),
    };

    ctx.get_storage_mut()
        .write_bitcoin_transaction(&bitcoin_transaction)
        .await
        .unwrap();
}

pub async fn mock_reqwests_status_code_error(status_code: usize) -> reqwest::Error {
    let mut server: mockito::ServerGuard = mockito::Server::new_async().await;
    let _mock = server.mock("GET", "/").with_status(status_code).create();
    reqwest::get(server.url())
        .await
        .unwrap()
        .error_for_status()
        .expect_err("expected error")
}

fn assert_stacks_transaction_kind<T>(tx: &StacksTransaction)
where
    T: AsContractCall,
{
    let TransactionPayload::ContractCall(contract_call) = &tx.payload else {
        panic!("expected a contract call, got something else");
    };

    assert_eq!(contract_call.contract_name.as_str(), T::CONTRACT_NAME);
    assert_eq!(contract_call.function_name.as_str(), T::FUNCTION_NAME);
}

/// Wait for all signers to finish their coordinator duties and do this
/// concurrently so that we don't miss anything (not sure if we need to do
/// it concurrently).
async fn wait_for_signers<S>(
    signers: &[(IntegrationTestContext<S>, PgStore, &Keypair, SignerNetwork)],
) where
    S: StacksInteract + Clone + Send + Sync + 'static,
{
    let wait_duration = Duration::from_secs(15);

    let expected = TxCoordinatorEvent::TenureCompleted.into();
    signers
        .iter()
        .map(|(ctx, _, _, _)| async {
            ctx.wait_for_signal(wait_duration, |signal| signal == &expected)
                .await
                .unwrap();
        })
        .join_all()
        .await;

    // It's not entirely clear why this sleep is helpful, but it appears to
    // be necessary in CI.
    Sleep::for_secs(2).await;
}

fn mock_deploy_all_contracts() -> Box<dyn FnOnce(&mut MockStacksInteract)> {
    Box::new(move |client: &mut MockStacksInteract| {
        // TODO: There are a few changes that we plan to make soon that
        // will require us to add or change the mocks here.
        // 1. DKG verification should take place immediately after DKG, not
        //    after the smart contract deployment.
        // 2. Submitting the rotate keys transaction should take place
        //    after we have deployed the smart contracts, but separate from
        //    DKG verification.
        // 3. We should probably return an error when asking for the
        //    current aggregate key and the smart contracts have not been
        //    deployed.
        client.expect_get_contract_source().returning(|_, _| {
            Box::pin(async {
                Err(Error::StacksNodeResponse(
                    mock_reqwests_status_code_error(404).await,
                ))
            })
        });

        // TODO: add another mock for get_current_signer_set_info when that
        // lands on main.
        client
            .expect_get_current_signers_aggregate_key()
            .returning(|_| Box::pin(std::future::ready(Ok(None))));
    })
}

#[test(tokio::test)]
async fn process_complete_deposit() {
    let db = testing::storage::new_test_database().await;
    let mut rng = get_rng();
    let (rpc, faucet) = regtest::initialize_blockchain();

    let setup = TestSweepSetup::new_setup(rpc, faucet, 1_000_000, &mut rng);

    backfill_bitcoin_blocks(&db, rpc, &setup.sweep_block_hash).await;
    setup.store_deposit_tx(&db).await;
    setup.store_sweep_tx(&db).await;
    setup.store_dkg_shares(&db).await;
    setup.store_deposit_request(&db).await;
    setup.store_deposit_decisions(&db).await;

    // Ensure a stacks tip exists
    let stacks_block = model::StacksBlock {
        block_hash: Faker.fake_with_rng(&mut OsRng),
        block_height: Faker.fake_with_rng(&mut OsRng),
        parent_hash: Faker.fake_with_rng(&mut OsRng),
        bitcoin_anchor: setup.sweep_block_hash.into(),
    };
    db.write_stacks_block(&stacks_block).await.unwrap();

    let mut context = TestContext::builder()
        .with_storage(db.clone())
        .with_first_bitcoin_core_client()
        .with_mocked_stacks_client()
        .with_mocked_emily_client()
        .build();

    let nonce = 12;
    // Mock required stacks client functions
    context
        .with_stacks_client(|client| {
            client.expect_get_account().once().returning(move |_| {
                Box::pin(async move {
                    Ok(AccountInfo {
                        balance: 0,
                        locked: 0,
                        unlock_height: 0u64.into(),
                        // The nonce is used to create the stacks tx
                        nonce,
                    })
                })
            });

            // Dummy value
            client
                .expect_estimate_fees()
                .once()
                .returning(move |_, _, _| Box::pin(async move { Ok(25505) }));

            client
                .expect_is_deposit_completed()
                .returning(move |_, _| Box::pin(async move { Ok(false) }));
        })
        .await;

    let num_signers = 7;
    let signing_threshold = 5;
    let context_window = 10;

    let network = network::in_memory::InMemoryNetwork::new();
    let signer_info = testing::wsts::generate_signer_info(&mut rng, num_signers);

    let mut testing_signer_set =
        testing::wsts::SignerSet::new(&signer_info, signing_threshold, || network.connect());

    let (aggregate_key, bitcoin_chain_tip) =
        run_dkg(&context, &mut rng, &mut testing_signer_set).await;

    // We do not want the coordinator to think that we need to run DKG
    // because it "detects" that the signer set has changed.
    let signer_set_public_keys: BTreeSet<PublicKey> =
        testing_signer_set.signer_keys().into_iter().collect();
    let state = context.state();
    let signer_set_info = SignerSetInfo {
        aggregate_key,
        signer_set: signer_set_public_keys.clone(),
        signatures_required: signing_threshold as u16,
    };
    state.update_registry_signer_set_info(signer_set_info.clone());
    state.update_current_signer_set(signer_set_public_keys);
    state.set_bitcoin_chain_tip(bitcoin_chain_tip);

    // Ensure we have a signers UTXO (as a donation, to not mess with the current
    // temporary `get_swept_deposit_requests` implementation)
    push_utxo_donation(&context, &aggregate_key, &setup.sweep_block_hash).await;

    assert_eq!(
        context
            .get_storage()
            .get_swept_deposit_requests(&bitcoin_chain_tip.block_hash, context_window)
            .await
            .expect("failed to get swept deposits")
            .len(),
        1
    );

    let (broadcasted_transaction_tx, _broadcasted_transaction_rx) =
        tokio::sync::broadcast::channel(1);

    // This task logs all transactions broadcasted by the coordinator.
    let mut wait_for_transaction_rx = broadcasted_transaction_tx.subscribe();
    let wait_for_transaction_task =
        tokio::spawn(async move { wait_for_transaction_rx.recv().await });

    // Setup the stacks client mock to broadcast the transaction to our channel.
    context
        .with_stacks_client(|client| {
            client.expect_submit_tx().once().returning(move |tx| {
                let tx = tx.clone();
                let txid = tx.txid();
                let broadcasted_transaction_tx = broadcasted_transaction_tx.clone();
                Box::pin(async move {
                    broadcasted_transaction_tx
                        .send(tx)
                        .expect("Failed to send result");
                    Ok(SubmitTxResponse::Acceptance(txid))
                })
            });
            client
                .expect_get_current_signer_set_info()
                .returning(move |_| {
                    Box::pin(std::future::ready(Ok(Some(signer_set_info.clone()))))
                });
        })
        .await;

    // Get the private key of the coordinator of the signer set.
    let private_key = select_coordinator(&setup.sweep_block_hash.into(), &signer_info);
    let config = context.config_mut();
    config.signer.bootstrap_signing_set = signer_info
        .first()
        .map(|signer| signer.signer_public_keys.clone())
        .unwrap();
    config.signer.bootstrap_signatures_required = signing_threshold as u16;

    prevent_dkg_on_changed_signer_set_info(&context, aggregate_key);

    // Bootstrap the tx coordinator event loop
    context.state().set_sbtc_contracts_deployed();
    let tx_coordinator = transaction_coordinator::TxCoordinatorEventLoop {
        context: context.clone(),
        network: network.connect(),
        private_key,
        context_window,
        threshold: signing_threshold as u16,
        signing_round_max_duration: Duration::from_secs(10),
        bitcoin_presign_request_max_duration: Duration::from_secs(10),
        dkg_max_duration: Duration::from_secs(10),
        is_epoch3: true,
    };
    let tx_coordinator_handle = tokio::spawn(async move { tx_coordinator.run().await });

    // TODO: here signers use all the same storage, should we use separate ones?
    let _event_loop_handles: Vec<_> = signer_info
        .clone()
        .into_iter()
        .map(|signer_info| {
            let event_loop_harness = TxSignerEventLoopHarness::create(
                context.clone(),
                network.connect(),
                context_window,
                signer_info.signer_private_key,
                signing_threshold,
                rng.clone(),
            );

            event_loop_harness.start()
        })
        .collect();

    // Yield to get signers ready
    Sleep::for_millis(100).await;

    // Wake coordinator up
    context
        .signal(RequestDeciderEvent::NewRequestsHandled.into())
        .expect("failed to signal");

    // Await the `wait_for_tx_task` to receive the first transaction broadcasted.
    let broadcasted_tx = tokio::time::timeout(Duration::from_secs(10), wait_for_transaction_task)
        .await
        .unwrap()
        .expect("failed to receive message")
        .expect("no message received");

    // Stop event loops
    tx_coordinator_handle.abort();

    broadcasted_tx.verify().unwrap();

    assert_eq!(broadcasted_tx.get_origin_nonce(), nonce);

    let (complete_deposit, _) = make_complete_deposit(&setup);
    let TransactionPayload::ContractCall(contract_call) = broadcasted_tx.payload else {
        panic!("unexpected tx payload")
    };
    assert_eq!(
        contract_call.contract_name.to_string(),
        CompleteDepositV1::CONTRACT_NAME
    );
    assert_eq!(
        contract_call.function_name.to_string(),
        CompleteDepositV1::FUNCTION_NAME
    );
    assert_eq!(
        contract_call.function_args,
        complete_deposit.as_contract_args()
    );

    testing::storage::drop_db(db).await;
}

/// Mock the stacks client to return dummy data for the given context.
async fn mock_stacks_core<D, B, E>(
    ctx: &mut TestContext<D, B, WrappedMock<MockStacksInteract>, E>,
    chain_tip_info: GetChainTipsResultTip,
    db: PgStore,
    broadcast_stacks_tx: Sender<StacksTransaction>,
) {
    ctx.with_stacks_client(|client| {
        client
            .expect_get_tenure_info()
            .returning(move || Box::pin(std::future::ready(Ok(DUMMY_TENURE_INFO.clone()))));

        client.expect_get_block().returning(|_| {
            let response = Ok(NakamotoBlock {
                header: NakamotoBlockHeader::empty(),
                txs: vec![],
            });
            Box::pin(std::future::ready(response))
        });

        let chain_tip = model::BitcoinBlockHash::from(chain_tip_info.hash);
        client.expect_get_tenure().returning(move |_| {
            let mut tenure = TenureBlocks::nearly_empty().unwrap();
            tenure.anchor_block_hash = chain_tip;
            Box::pin(std::future::ready(Ok(tenure)))
        });

        client.expect_get_pox_info().returning(|| {
            let response = serde_json::from_str::<RPCPoxInfoData>(GET_POX_INFO_JSON)
                .map_err(Error::JsonSerialize);
            Box::pin(std::future::ready(response))
        });

        client
            .expect_estimate_fees()
            .returning(|_, _, _| Box::pin(std::future::ready(Ok(25))));

        // The coordinator will try to further process the deposit to submit
        // the stacks tx, but we are not interested (for the current test iteration).
        client.expect_get_account().returning(|_| {
            let response = Ok(AccountInfo {
                balance: 0,
                locked: 0,
                unlock_height: 0u64.into(),
                // this is the only part used to create the Stacks transaction.
                nonce: 12,
            });
            Box::pin(std::future::ready(response))
        });
        client.expect_get_sortition_info().returning(move |_| {
            let response = Ok(SortitionInfo {
                burn_block_hash: BurnchainHeaderHash::from(chain_tip),
                burn_block_height: chain_tip_info.height,
                burn_header_timestamp: 0,
                sortition_id: SortitionId([0; 32]),
                parent_sortition_id: SortitionId([0; 32]),
                consensus_hash: ConsensusHash([0; 20]),
                was_sortition: true,
                miner_pk_hash160: None,
                stacks_parent_ch: None,
                last_sortition_ch: None,
                committed_block_hash: None,
            });
            Box::pin(std::future::ready(response))
        });

        // The coordinator broadcasts a rotate keys transaction if it
        // is not up-to-date with their view of the current aggregate
        // key. The response of here means that the stacks node has a
        // record of a rotate keys contract call being executed once we
        // have verified shares.
        client
            .expect_get_current_signer_set_info()
            .returning(move |_| {
                let db = db.clone();
                Box::pin(async move {
                    let shares = db.get_latest_verified_dkg_shares().await?;
                    Ok(shares.map(SignerSetInfo::from))
                })
            });

        // Only the client that corresponds to the coordinator will
        // submit a transaction, so we don't make explicit the
        // expectation here.
        client.expect_submit_tx().returning(move |tx| {
            let tx = tx.clone();
            let txid = tx.txid();
            let broadcast_stacks_tx = broadcast_stacks_tx.clone();
            Box::pin(async move {
                broadcast_stacks_tx.send(tx).unwrap();
                Ok(SubmitTxResponse::Acceptance(txid))
            })
        });
        // The coordinator will get the total supply of sBTC to
        // determine the amount of mintable sBTC.
        client
            .expect_get_sbtc_total_supply()
            .returning(move |_| Box::pin(async move { Ok(Amount::ZERO) }));

        client
            .expect_is_deposit_completed()
            .returning(move |_, _| Box::pin(async move { Ok(false) }));

        // We use this during validation to check if the withdrawal
        // request completed in the smart contract.
        client
            .expect_is_withdrawal_completed()
            .returning(|_, _| Box::pin(std::future::ready(Ok(false))));
    })
    .await;
}

/// Tests that the coordinator deploys the smart contracts in the correct
/// order if none are deployed.
#[tokio::test]
async fn deploy_smart_contracts_coordinator() {
    let (_, signer_key_pairs): (_, [Keypair; 3]) = testing::wallet::regtest_bootstrap_wallet();
    let (rpc, faucet) = regtest::initialize_blockchain();

    // We need to populate our databases, so let's fetch the data.
    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    testing_api::wipe_databases(&emily_client.config().as_testing())
        .await
        .unwrap();

    let network = WanNetwork::default();

    let chain_tip_info = get_canonical_chain_tip(rpc);

    // =========================================================================
    // Step 1 - Create a database, an associated context, and a Keypair for
    //          each of the signers in the signing set.
    // -------------------------------------------------------------------------
    // - We load the database with a bitcoin blocks going back to some
    //   genesis block.
    // =========================================================================
    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_emily_client(emily_client.clone())
            .with_mocked_stacks_client()
            .build();

        backfill_bitcoin_blocks(&db, rpc, &chain_tip_info.hash).await;

        let network = network.connect(&ctx);

        signers.push((ctx, db, kp, network));
    }

    // =========================================================================
    // Step 2 - Setup the stacks client mocks.
    // -------------------------------------------------------------------------
    // - Set up the mocks to that the block observer fetches at least one
    //   Stacks block. This is necessary because we need the stacks chain
    //   tip in the transaction coordinator.
    // - Set up the current-aggregate-key response to be `None`. This means
    //   that each coordinator will broadcast a rotate keys transaction.
    // =========================================================================
    let (broadcast_stacks_tx, rx) = tokio::sync::broadcast::channel(10);
    let stacks_tx_stream = BroadcastStream::new(rx);

    for (ctx, db, _, _) in signers.iter_mut() {
        let broadcast_stacks_tx = broadcast_stacks_tx.clone();
        let db = db.clone();

        ctx.with_stacks_client(|client| mock_deploy_all_contracts()(client))
            .await;
        mock_stacks_core(ctx, chain_tip_info.clone(), db, broadcast_stacks_tx).await;
    }

    // =========================================================================
    // Step 3 - Start the TxCoordinatorEventLoop, TxSignerEventLoop and
    //          BlockObserver processes for each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started, and
    //   we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: ctx.config().signer.bootstrap_signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            last_presign_block: None,
            rng: rand::rngs::OsRng,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // =========================================================================
    // Step 4 - Wait for DKG
    // -------------------------------------------------------------------------
    // - Once they are all running, generate a bitcoin block to kick off
    //   the database updating process.
    // - After they have the same view of the canonical bitcoin blockchain,
    //   the signers should all participate in DKG.
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    for (_, db, _, _) in signers.iter() {
        let count = db.get_encrypted_dkg_shares_count().await.unwrap();
        assert_eq!(count, 1);
    }

    let sleep_fut = tokio::time::sleep(Duration::from_secs(5));
    let broadcast_stacks_txs: Vec<StacksTransaction> = stacks_tx_stream
        .take_until(sleep_fut)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(broadcast_stacks_txs.len(), SMART_CONTRACTS.len());

    // Check that the contracts were deployed
    for (deployed, broadcasted_tx) in SMART_CONTRACTS.iter().zip(broadcast_stacks_txs) {
        // Await the `wait_for_tx_task` to receive the first transaction broadcasted.
        broadcasted_tx.verify().unwrap();

        let TransactionPayload::SmartContract(contract, _) = broadcasted_tx.payload else {
            panic!("unexpected tx payload")
        };
        assert_eq!(contract.name.as_str(), deployed.contract_name());
        assert_eq!(&contract.code_body.to_string(), deployed.contract_body());
    }

    for (_, db, _, _) in signers {
        testing::storage::drop_db(db).await;
    }
}

/// Test that we run DKG if the coordinator notices that DKG has not been
/// run yet.
///
/// This test proceeds by doing the following:
/// 1. Create a database, an associated context, and a Keypair for each of
///    the signers in the signing set.
/// 2. Populate each database with the same data, so that they have the
///    same view of the canonical bitcoin blockchain. This ensures that
///    they participate in DKG.
/// 3. Check that there are no DKG shares in the database.
/// 4. Start the [`TxCoordinatorEventLoop`] and [`TxSignerEventLoop`]
///    processes for each signer.
/// 5. Once they are all running, signal that DKG should be run. We signal
///    them all because we do not know which one is the coordinator.
/// 6. Check that we have exactly one row in the `dkg_shares` table.
/// 7. Check that they all have the same aggregate key in the `dkg_shares`
///    table.
/// 8. Check that the coordinator broadcast a rotate key tx
///
/// Some of the preconditions for this test to run successfully includes
/// having bootstrap public keys that align with the [`Keypair`] returned
/// from the [`testing::wallet::regtest_bootstrap_wallet`] function.
#[test(tokio::test)]
async fn run_dkg_from_scratch() {
    let mut rng = get_rng();
    let (signer_wallet, signer_key_pairs): (_, [Keypair; 3]) =
        testing::wallet::regtest_bootstrap_wallet();

    // We need to populate our databases, so let's generate some data.
    let test_params = testing::storage::model::Params {
        num_bitcoin_blocks: 10,
        num_stacks_blocks_per_bitcoin_block: 1,
        num_deposit_requests_per_block: 0,
        num_withdraw_requests_per_block: 0,
        num_signers_per_request: 0,
        consecutive_blocks: false,
    };
    let test_data = TestData::generate(&mut rng, &[], &test_params);

    let (broadcast_stacks_tx, _rx) = tokio::sync::broadcast::channel(1);

    let mut stacks_tx_receiver = broadcast_stacks_tx.subscribe();
    let stacks_tx_receiver_task = tokio::spawn(async move { stacks_tx_receiver.recv().await });

    let iter: Vec<(Keypair, TestData)> = signer_key_pairs
        .iter()
        .copied()
        .zip(std::iter::repeat_with(|| test_data.clone()))
        .collect();

    // 1. Create a database, an associated context, and a Keypair for each of
    //    the signers in the signing set.
    let network = WanNetwork::default();
    let mut signers: Vec<_> = Vec::new();

    for (kp, data) in iter {
        let broadcast_stacks_tx = broadcast_stacks_tx.clone();
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_mocked_clients()
            .modify_settings(|config| {
                config.signer.private_key = kp.secret_key().into();
            })
            .build();

        ctx.with_stacks_client(|client| {
            client
                .expect_estimate_fees()
                .returning(|_, _, _| Box::pin(async { Ok(123000) }));

            client.expect_get_account().returning(|_| {
                Box::pin(async {
                    Ok(AccountInfo {
                        balance: 1_000_000,
                        locked: 0,
                        unlock_height: 0u64.into(),
                        nonce: 1,
                    })
                })
            });

            client.expect_submit_tx().returning(move |tx| {
                let tx = tx.clone();
                let txid = tx.txid();
                let broadcast_stacks_tx = broadcast_stacks_tx.clone();
                Box::pin(async move {
                    broadcast_stacks_tx.send(tx).expect("Failed to send result");
                    Ok(SubmitTxResponse::Acceptance(txid))
                })
            });

            client
                .expect_get_current_signer_set_info()
                .returning(move |_| Box::pin(std::future::ready(Ok(None))));
        })
        .await;

        // 2. Populate each database with the same data, so that they
        //    have the same view of the canonical bitcoin blockchain.
        //    This ensures that they participate in DKG.
        data.write_to(&db).await;

        let network = network.connect(&ctx);

        let chain_tip_ref = db
            .get_bitcoin_canonical_chain_tip_ref()
            .await
            .unwrap()
            .unwrap();
        ctx.state().set_bitcoin_chain_tip(chain_tip_ref);

        signers.push((ctx, db, kp, network));
    }

    // 3. Check that there are no DKG shares in the database.
    for (_, db, _, _) in signers.iter() {
        let some_shares = db.get_latest_encrypted_dkg_shares().await.unwrap();
        assert!(some_shares.is_none());
    }

    // 4. Start the [`TxCoordinatorEventLoop`] and [`TxSignerEventLoop`]
    //    processes for each signer.
    let tx_coordinator_processes = signers.iter().map(|(ctx, _, kp, net)| {
        ctx.state().set_sbtc_contracts_deployed(); // Skip contract deployment
        TxCoordinatorEventLoop {
            network: net.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        }
    });

    let tx_signer_processes = signers.iter().map(|(context, _, _, net)| {
        TxSignerEventLoop::new(context.clone(), net.spawn(), OsRng)
            .expect("failed to create TxSignerEventLoop")
    });

    // We only proceed with the test after all processes have started, and
    // we use this counter to notify us when that happens.
    let start_count = Arc::new(AtomicU8::new(0));

    tx_coordinator_processes.for_each(|ev| {
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });
    });

    tx_signer_processes.for_each(|ev| {
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });
    });

    while start_count.load(Ordering::SeqCst) < 6 {
        Sleep::for_millis(10).await;
    }

    // 5. Once they are all running, signal that DKG should be run. We
    //    signal them all because we do not know which one is the
    //    coordinator.
    signers.iter().for_each(|(ctx, _, _, _)| {
        ctx.get_signal_sender()
            .send(RequestDeciderEvent::NewRequestsHandled.into())
            .unwrap();
    });

    // Await the `stacks_tx_receiver_task` to receive the first transaction broadcasted.
    let broadcast_stacks_txs =
        tokio::time::timeout(Duration::from_secs(10), stacks_tx_receiver_task)
            .await
            .unwrap()
            .expect("failed to receive message")
            .expect("no message received");

    let mut aggregate_keys = BTreeSet::new();

    for (_, db, _, _) in signers.iter() {
        let mut aggregate_key =
            sqlx::query_as::<_, (PublicKey,)>("SELECT aggregate_key FROM sbtc_signer.dkg_shares")
                .fetch_all(db.pool())
                .await
                .unwrap();

        // 6. Check that we have exactly one row in the `dkg_shares` table.
        assert_eq!(aggregate_key.len(), 1);

        // An additional sanity check that the query in
        // get_last_encrypted_dkg_shares gets the right thing (which is the
        // only thing in this case.)
        let key = aggregate_key.pop().unwrap().0;
        let shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();
        assert_eq!(shares.aggregate_key, key);
        aggregate_keys.insert(key);
    }

    // 7. Check that they all have the same aggregate key in the
    //    `dkg_shares` table.
    assert_eq!(aggregate_keys.len(), 1);

    // 8. Check that the coordinator broadcast a rotate key tx
    broadcast_stacks_txs.verify().unwrap();

    let TransactionPayload::ContractCall(contract_call) = broadcast_stacks_txs.payload else {
        panic!("unexpected tx payload")
    };
    assert_eq!(
        contract_call.contract_name.to_string(),
        RotateKeysV1::CONTRACT_NAME
    );
    assert_eq!(
        contract_call.function_name.to_string(),
        RotateKeysV1::FUNCTION_NAME
    );
    let rotate_keys = RotateKeysV1::new(
        &signer_wallet,
        signers.first().unwrap().0.config().signer.deployer,
        aggregate_keys.iter().next().unwrap(),
    );
    assert_eq!(contract_call.function_args, rotate_keys.as_contract_args());

    for (_ctx, db, _, _) in signers {
        testing::storage::drop_db(db).await;
    }
}

/// Tests that dkg will be triggered if signer set changes
#[test_case(true; "signatures_required_changed")]
#[test_case(false; "signatures_required_unchanged")]
#[tokio::test]
async fn run_dkg_if_signer_set_changes(signer_set_changed: bool) {
    let mut rng = get_rng();
    let db = testing::storage::new_test_database().await;
    let ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .modify_settings(|settings| {
            settings.signer.dkg_target_rounds = std::num::NonZero::<u32>::new(1).unwrap();
        })
        .build();

    let mut config_signer_set = ctx.config().signer.bootstrap_signing_set.clone();
    // Sanity check
    assert!(!config_signer_set.is_empty());

    // Make sure that in very beginning of the test config and context signer sets are same.
    ctx.inner
        .state()
        .update_current_signer_set(config_signer_set.iter().cloned().collect());

    // Write dkg shares so it won't be a reason to trigger dkg.
    let dkg_shares = model::EncryptedDkgShares {
        dkg_shares_status: model::DkgSharesStatus::Verified,
        ..Faker.fake_with_rng(&mut rng)
    };
    db.write_encrypted_dkg_shares(&dkg_shares)
        .await
        .expect("failed to write dkg shares");

    // Remove one signer
    if signer_set_changed {
        let _removed_signer = config_signer_set
            .pop_first()
            .expect("This signer set should not be empty");
    }
    // Create chaintip
    let chaintip: model::BitcoinBlockRef = Faker.fake_with_rng(&mut rng);

    prevent_dkg_on_changed_signer_set_info(&ctx, dkg_shares.aggregate_key);

    // Before we actually change the signer set, the DKG won't be triggered
    assert!(!should_coordinate_dkg(&ctx, &chaintip).await.unwrap());
    assert!(assert_allow_dkg_begin(&ctx, &chaintip).await.is_err());

    // Now we change context signer set.
    let signer_set_info = SignerSetInfo {
        aggregate_key: dkg_shares.aggregate_key,
        signatures_required: ctx.config().signer.bootstrap_signatures_required,
        signer_set: config_signer_set,
    };

    ctx.state().update_registry_signer_set_info(signer_set_info);

    if signer_set_changed {
        assert!(should_coordinate_dkg(&ctx, &chaintip).await.unwrap());
        assert!(assert_allow_dkg_begin(&ctx, &chaintip).await.is_ok());
    } else {
        assert!(!should_coordinate_dkg(&ctx, &chaintip).await.unwrap());
        assert!(assert_allow_dkg_begin(&ctx, &chaintip).await.is_err());
    }
    testing::storage::drop_db(db).await;
}

/// Tests that dkg will be triggered if signatures required parameter changes
#[test_case(true; "signatures_required_changed")]
#[test_case(false; "signatures_required_unchanged")]
#[tokio::test]
async fn run_dkg_if_signatures_required_changes(change_signatures_required: bool) {
    let mut rng = get_rng();
    let db = testing::storage::new_test_database().await;
    let mut ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .modify_settings(|settings| {
            settings.signer.dkg_target_rounds = std::num::NonZero::<u32>::new(1).unwrap();
            settings.signer.bootstrap_signatures_required = 1;
        })
        .build();
    let config_signer_set = ctx.config().signer.bootstrap_signing_set.clone();

    // Sanity check, since we want change bootstrap_signatures_required during this test
    // we need at least two valid values.
    assert!(config_signer_set.len() > 1);

    // Write dkg shares so it won't be a reason to trigger dkg.
    let dkg_shares = model::EncryptedDkgShares {
        dkg_shares_status: model::DkgSharesStatus::Verified,
        ..Faker.fake_with_rng(&mut rng)
    };
    db.write_encrypted_dkg_shares(&dkg_shares)
        .await
        .expect("failed to write dkg shares");

    let signer_set_info = SignerSetInfo {
        aggregate_key: dkg_shares.aggregate_key,
        // This matches the value we set for the bootstrap_signatures_required
        signatures_required: ctx.config().signer.bootstrap_signatures_required,
        signer_set: config_signer_set,
    };

    ctx.state().update_registry_signer_set_info(signer_set_info);

    // Create chaintip
    let chaintip: model::BitcoinBlockRef = Faker.fake_with_rng(&mut rng);

    // Before we actually change the signatures_required, the DKG won't be triggered
    assert!(!should_coordinate_dkg(&ctx, &chaintip).await.unwrap());
    assert!(assert_allow_dkg_begin(&ctx, &chaintip).await.is_err());

    // Change bootstrap_signatures_required to trigger dkg
    if change_signatures_required {
        ctx.config_mut().signer.bootstrap_signatures_required = 2;

        assert!(should_coordinate_dkg(&ctx, &chaintip).await.unwrap());
        assert!(assert_allow_dkg_begin(&ctx, &chaintip).await.is_ok());
    } else {
        assert!(!should_coordinate_dkg(&ctx, &chaintip).await.unwrap());
        assert!(assert_allow_dkg_begin(&ctx, &chaintip).await.is_err());
    }
    testing::storage::drop_db(db).await;
}

/// Tests that DKG will not run if latest shares are unverified
#[test_case(DkgSharesStatus::Unverified, false; "unverified")]
#[test_case(DkgSharesStatus::Verified, true; "verified")]
#[test_case(DkgSharesStatus::Failed, true; "failed")]
#[tokio::test]
async fn skip_dkg_if_latest_shares_unverified(
    latest_shares_status: DkgSharesStatus,
    should_run_dkg: bool,
) {
    let mut rng = get_rng();
    let db = testing::storage::new_test_database().await;
    let ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .modify_settings(|settings| {
            // We want to run DKG twice
            settings.signer.dkg_target_rounds = std::num::NonZero::<u32>::new(2).unwrap();
            settings.signer.dkg_min_bitcoin_block_height = Some(0u64.into());
        })
        .build();

    // First DKG run result
    let dkg_shares = model::EncryptedDkgShares {
        dkg_shares_status: latest_shares_status,
        ..Faker.fake_with_rng(&mut rng)
    };
    db.write_encrypted_dkg_shares(&dkg_shares)
        .await
        .expect("failed to write dkg shares");

    let chaintip: model::BitcoinBlockRef = Faker.fake_with_rng(&mut rng);

    if should_run_dkg {
        assert!(should_coordinate_dkg(&ctx, &chaintip).await.unwrap());
        assert!(assert_allow_dkg_begin(&ctx, &chaintip).await.is_ok());
    } else {
        assert!(!should_coordinate_dkg(&ctx, &chaintip).await.unwrap());
        assert!(assert_allow_dkg_begin(&ctx, &chaintip).await.is_err());
    }

    testing::storage::drop_db(db).await;
}

/// Test that we can run multiple DKG rounds.
/// This test is very similar to the `run_dkg_from_scratch` test, but it
/// simulates that DKG has been run once before and uses a signer configuration
/// that allows for multiple DKG rounds.
#[test(tokio::test)]
async fn run_subsequent_dkg() {
    let mut rng = get_rng();
    let (signer_wallet, signer_key_pairs): (_, [Keypair; 3]) =
        testing::wallet::regtest_bootstrap_wallet();

    // We need to populate our databases, so let's generate some data.
    let test_params = testing::storage::model::Params {
        num_bitcoin_blocks: 10,
        num_stacks_blocks_per_bitcoin_block: 1,
        num_deposit_requests_per_block: 0,
        num_withdraw_requests_per_block: 0,
        num_signers_per_request: 0,
        consecutive_blocks: false,
    };
    let test_data = TestData::generate(&mut rng, &[], &test_params);

    let (broadcast_stacks_tx, _rx) = tokio::sync::broadcast::channel(1);

    let mut stacks_tx_receiver = broadcast_stacks_tx.subscribe();
    let stacks_tx_receiver_task = tokio::spawn(async move { stacks_tx_receiver.recv().await });

    let iter: Vec<(Keypair, TestData)> = signer_key_pairs
        .iter()
        .copied()
        .zip(std::iter::repeat_with(|| test_data.clone()))
        .collect();

    // 1. Create a database, an associated context, and a Keypair for each of
    //    the signers in the signing set.
    let network = WanNetwork::default();
    let mut signers: Vec<_> = Vec::new();

    // The aggregate key we will use for the first DKG shares entry.
    let aggregate_key_1: PublicKey = Faker.fake_with_rng(&mut rng);
    let signer_set_public_keys: BTreeSet<PublicKey> = signer_key_pairs
        .iter()
        .map(|kp| kp.public_key().into())
        .collect();

    for (kp, data) in iter {
        let broadcast_stacks_tx = broadcast_stacks_tx.clone();
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_mocked_clients()
            .modify_settings(|settings| {
                settings.signer.dkg_target_rounds = NonZeroU32::new(2).unwrap();
                settings.signer.dkg_min_bitcoin_block_height = Some(10u64.into());
            })
            .build();

        // 2. Populate each database with the same data, so that they
        //    have the same view of the canonical bitcoin blockchain.
        //    This ensures that they participate in DKG.
        data.write_to(&db).await;

        // Write one DKG shares entry to the signer's database simulating that
        // DKG has been successfully run once.
        db.write_encrypted_dkg_shares(&EncryptedDkgShares {
            aggregate_key: aggregate_key_1,
            signer_set_public_keys: signer_set_public_keys.iter().copied().collect(),
            dkg_shares_status: DkgSharesStatus::Verified,
            ..Faker.fake()
        })
        .await
        .expect("failed to write dkg shares");

        ctx.with_stacks_client(|client| {
            client
                .expect_estimate_fees()
                .returning(|_, _, _| Box::pin(async { Ok(123000) }));

            client.expect_get_account().returning(|_| {
                Box::pin(async {
                    Ok(AccountInfo {
                        balance: 1_000_000,
                        locked: 0,
                        unlock_height: 0u64.into(),
                        nonce: 1,
                    })
                })
            });

            client.expect_submit_tx().returning(move |tx| {
                let tx = tx.clone();
                let txid = tx.txid();
                let broadcast_stacks_tx = broadcast_stacks_tx.clone();
                Box::pin(async move {
                    broadcast_stacks_tx.send(tx).expect("Failed to send result");
                    Ok(SubmitTxResponse::Acceptance(txid))
                })
            });

            client
                .expect_get_current_signer_set_info()
                .returning(move |_| Box::pin(std::future::ready(Ok(None))));
        })
        .await;

        let network = network.connect(&ctx);

        let chain_tip_ref = db
            .get_bitcoin_canonical_chain_tip_ref()
            .await
            .unwrap()
            .unwrap();
        ctx.state().set_bitcoin_chain_tip(chain_tip_ref);

        signers.push((ctx, db, kp, network));
    }

    // 4. Start the [`TxCoordinatorEventLoop`] and [`TxSignerEventLoop`]
    //    processes for each signer.
    let tx_coordinator_processes = signers.iter().map(|(ctx, _, kp, net)| {
        ctx.state().set_sbtc_contracts_deployed(); // Skip contract deployment
        TxCoordinatorEventLoop {
            network: net.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        }
    });

    let tx_signer_processes = signers
        .iter()
        .map(|(context, _, kp, net)| TxSignerEventLoop {
            network: net.spawn(),
            threshold: context.config().signer.bootstrap_signatures_required as u32,
            context: context.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            rng: rand::rngs::OsRng,
            last_presign_block: None,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        });

    // We only proceed with the test after all processes have started, and
    // we use this counter to notify us when that happens.
    let start_count = Arc::new(AtomicU8::new(0));

    tx_coordinator_processes.for_each(|ev| {
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });
    });

    tx_signer_processes.for_each(|ev| {
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });
    });

    while start_count.load(Ordering::SeqCst) < 6 {
        Sleep::for_millis(10).await;
    }

    // 5. Once they are all running, signal that DKG should be run. We
    //    signal them all because we do not know which one is the
    //    coordinator.
    signers.iter().for_each(|(ctx, _, _, _)| {
        ctx.get_signal_sender()
            .send(RequestDeciderEvent::NewRequestsHandled.into())
            .unwrap();
    });

    // Await the `stacks_tx_receiver_task` to receive the first transaction broadcasted.
    let broadcast_stacks_txs =
        tokio::time::timeout(Duration::from_secs(10), stacks_tx_receiver_task)
            .await
            .unwrap()
            .expect("failed to receive message")
            .expect("no message received");

    // A BTreeSet to uniquely hold all the aggregate keys we find in the database.
    let mut all_aggregate_keys = BTreeSet::new();

    for (_, db, _, _) in signers.iter() {
        // Get the aggregate keys from this signer's database.
        let mut aggregate_keys =
            sqlx::query_as::<_, (PublicKey,)>("SELECT aggregate_key FROM sbtc_signer.dkg_shares")
                .fetch_all(db.pool())
                .await
                .unwrap();

        // 6. Check that we have exactly one row in the `dkg_shares` table.
        assert_eq!(aggregate_keys.len(), 2);
        for key in aggregate_keys.iter() {
            all_aggregate_keys.insert(key.0);
        }

        // An additional sanity check that the query in
        // get_last_encrypted_dkg_shares gets the right thing (which is the
        // only thing in this case).
        let key = aggregate_keys.pop().unwrap().0;
        let shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();
        assert_eq!(shares.aggregate_key, key);
    }

    // 7. Check that they all have the same aggregate keys in the
    //    `dkg_shares` table.
    assert_eq!(all_aggregate_keys.len(), 2);
    let new_aggregate_key = *all_aggregate_keys
        .iter()
        .find(|k| *k != &aggregate_key_1)
        .unwrap();
    assert_ne!(aggregate_key_1, new_aggregate_key);

    // 8. Check that the coordinator broadcast a rotate key tx
    broadcast_stacks_txs.verify().unwrap();

    let TransactionPayload::ContractCall(contract_call) = broadcast_stacks_txs.payload else {
        panic!("unexpected tx payload")
    };
    assert_eq!(
        contract_call.contract_name.to_string(),
        RotateKeysV1::CONTRACT_NAME
    );
    assert_eq!(
        contract_call.function_name.to_string(),
        RotateKeysV1::FUNCTION_NAME
    );
    let rotate_keys = RotateKeysV1::new(
        &signer_wallet,
        signers.first().unwrap().0.config().signer.deployer,
        &new_aggregate_key,
    );

    assert_eq!(contract_call.function_args, rotate_keys.as_contract_args());

    for (_ctx, db, _, _) in signers {
        testing::storage::drop_db(db).await;
    }
}

/// Test that three signers can generate the same DKG shares if DKG is run
/// with the same signer set during the same bitcoin block.
///
/// The test setup is as follows:
/// 1. There are three "signers" contexts. Each context points to its own
///    real postgres database, and they have their own private key. Each
///    database is populated with the same data.
/// 2. Each context is given to a block observer, a tx signer, and a tx
///    coordinator, where these event loops are spawned as separate tasks.
/// 3. The signers communicate with our in-memory network struct.
/// 4. A real Emily server is running in the background.
/// 5. A real bitcoin-core node is running in the background.
/// 6. Stacks-core is mocked.
///
/// After the setup, the signers observe a bitcoin block and update their
/// databases. The coordinator then runs DKG. We then modify the aggregate
/// key for the DKG shares and trigger the cooridnators so that they run
/// DKG again and observe that the same secret shares are generated. Then
/// we observe a bitcoin block so that DKG runs a third time and note that
/// completely new shares are generated.
///
/// To start the test environment do:
/// ```bash
/// make integration-env-up
/// ```
///
/// then, once everything is up and running, run the test.
#[tokio::test]
async fn pseudo_random_dkg() {
    let (_, signer_key_pairs): (_, [Keypair; 3]) = testing::wallet::regtest_bootstrap_wallet();
    let (rpc, faucet) = regtest::initialize_blockchain();

    // We need to populate our databases, so let's fetch the data.
    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    testing_api::wipe_databases(&emily_client.config().as_testing())
        .await
        .unwrap();

    let network = WanNetwork::default();

    let chain_tip_info = get_canonical_chain_tip(rpc);

    // =========================================================================
    // Step 1 - Create a database, an associated context, and a Keypair for
    //          each of the signers in the signing set.
    // -------------------------------------------------------------------------
    // - We load the database with a bitcoin blocks going back to some
    //   genesis block.
    // =========================================================================
    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_emily_client(emily_client.clone())
            .with_mocked_stacks_client()
            .modify_settings(|settings| {
                settings.signer.dkg_target_rounds = NonZeroU32::new(20).unwrap();
                settings.signer.dkg_min_bitcoin_block_height = Some(0u64.into());
            })
            .build();

        backfill_bitcoin_blocks(&db, rpc, &chain_tip_info.hash).await;

        let network = network.connect(&ctx);

        signers.push((ctx, db, kp, network));
    }

    // =========================================================================
    // Step 2 - Setup the stacks client mocks.
    // -------------------------------------------------------------------------
    // - Set up the mocks to that the block observer fetches at least one
    //   Stacks block. This is necessary because we need the stacks chain
    //   tip in the transaction coordinator.
    // - Set up the current-aggregate-key response to be `None`. This means
    //   that each coordinator will broadcast a rotate keys transaction.
    // =========================================================================
    let (broadcast_stacks_tx, rx) = tokio::sync::broadcast::channel(10);
    let _stacks_tx_stream = BroadcastStream::new(rx);

    for (ctx, db, _, _) in signers.iter_mut() {
        let broadcast_stacks_tx = broadcast_stacks_tx.clone();
        let db = db.clone();

        mock_stacks_core(ctx, chain_tip_info.clone(), db, broadcast_stacks_tx).await;
    }

    // =========================================================================
    // Step 3 - Start the TxCoordinatorEventLoop, TxSignerEventLoop, and
    //          RequestDeciderEventLoop, and BlockObserver processes for
    //          each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started,
    //   and we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        ctx.state().set_sbtc_contracts_deployed();
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: ctx.config().signer.bootstrap_signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            rng: rand::rngs::OsRng,
            dkg_begin_pause: None,
            last_presign_block: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // =========================================================================
    // Step 4 - Wait for DKG
    // -------------------------------------------------------------------------
    // - Once they are all running, generate a bitcoin block to kick off
    //   the database updating process.
    // - After they have the same view of the canonical bitcoin blockchain,
    //   the signers should all participate in DKG.
    // =========================================================================
    faucet.generate_block();

    // Now we wait for DKG to successfully complete by waiting for all
    // coordinator event loops to finish.
    wait_for_signers(&signers).await;
    let (_, db, _, _) = signers.first().unwrap();
    let original_shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();

    // =========================================================================
    // Step 5 - Prepare to re-run DKG with the same bitcoin block
    // -------------------------------------------------------------------------
    // - The signers will attempt to run DKG a second time and when they do
    //   they should generate the same aggregate key and secret shares.
    //   Unless we do something, this will leave the dkg_shares table
    //   unchanged, since the aggregate key is the primary key. So we
    //   modify the aggregate key for the current row so that we get a
    //   second row when DKG is re-run.
    // =========================================================================

    // We create a tweak public key which is the generator of the secp256k1
    // elliptic curve. There is nothing special about the chosen tweak
    // (TWEAKED_PK), we just need to make sure that it is not the adjusted
    // aggregate key is neither equal to the original aggregate key nor its
    // negation.
    let tweak_secret_key = secp256k1::SecretKey::from_slice(&secp256k1::constants::ONE).unwrap();
    let tweak_public_key = tweak_secret_key.public_key(SECP256K1);
    let adjusted_aggregate_key: PublicKey = original_shares
        .aggregate_key
        .combine(&tweak_public_key)
        .unwrap()
        .into();

    // Here we adjust the aggregate key of the first DKG run. Note that
    // changing the aggregate key in this way leads to an error if we need
    // to load these shares in the FROST or FIRE coordinators. But we do
    // not have any signing rounds or DKG verification rounds with these
    // shares so we are fine.
    for (_, db, _, _) in signers.iter() {
        let count = db.get_encrypted_dkg_shares_count().await.unwrap();
        assert_eq!(count, 1);

        let pg_result = sqlx::query(
            r#"
            UPDATE sbtc_signer.dkg_shares
            SET aggregate_key = $1
            WHERE aggregate_key = $2
            "#,
        )
        .bind(adjusted_aggregate_key)
        .bind(original_shares.aggregate_key)
        .execute(db.pool())
        .await
        .unwrap();

        assert_eq!(pg_result.rows_affected(), 1);

        let new_shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();
        assert_ne!(new_shares, original_shares);
    }

    // =========================================================================
    // Step 6 - Re-run DKG with the same bitcoin block
    // -------------------------------------------------------------------------
    // - The signers will run DKG a second time without changing the
    //   bitcoin block hash and block height. When they do they should
    //   generate the same aggregate key and secret shares. Because of Step
    //   5 the new shares will be saved.
    // =========================================================================

    // Okay, now let's see what happens if we run DKG a second time
    // assuming the same bitcoin block hash and height are used as part of
    // the process. To kick this off, we just trigger each of the
    // cooridnators.
    signers
        .iter()
        .try_for_each(|(ctx, _, _, _)| ctx.signal(RequestDeciderEvent::NewRequestsHandled.into()))
        .unwrap();

    wait_for_signers(&signers).await;

    // Okay, DKG should have run for a second time. The generated keys
    // should be identical to the keys generated the first time.
    for (_, db, keypair, _) in signers.iter() {
        let count = db.get_encrypted_dkg_shares_count().await.unwrap();
        assert_eq!(count, 2);

        let new_shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();
        let data = &new_shares.encrypted_private_shares;
        let new_decrypted_secrets = wsts::util::decrypt(&keypair.secret_bytes(), data).unwrap();

        // We have adjusted the aggregate key of our first DKG run, so we
        // load them up using the adjusted aggregate key.
        let first_shares = db
            .get_encrypted_dkg_shares(&adjusted_aggregate_key)
            .await
            .unwrap()
            .unwrap();
        // The first aggregate key is the adjusted aggregate key minus the
        // TWEAK_PK. Basically, AK1 = AK2 - TWEAK_PK = AK2 + (-TWEAK_PK).
        let unadjusted_aggregate_key: PublicKey = first_shares
            .aggregate_key
            .combine(&tweak_public_key.negate(SECP256K1))
            .unwrap()
            .into();

        // So we should have the same aggregate key, scriptPubKey,
        // threshold, signing set and so on.
        assert_eq!(new_shares.aggregate_key, unadjusted_aggregate_key);
        assert_eq!(new_shares.script_pubkey, first_shares.script_pubkey);
        assert_eq!(
            new_shares.signature_share_threshold,
            first_shares.signature_share_threshold
        );
        assert_eq!(
            new_shares.signer_set_public_keys,
            first_shares.signer_set_public_keys
        );
        assert_eq!(
            new_shares.started_at_bitcoin_block_hash,
            first_shares.started_at_bitcoin_block_hash
        );
        assert_eq!(
            new_shares.started_at_bitcoin_block_height,
            first_shares.started_at_bitcoin_block_height
        );

        // Let's check to see if the private shares are the same. We cannot
        // just do a direct comparison because the encryption algorithm
        // takes some randomness and will generate different encrypted
        // bytes each time, even when given the same plaintext data.
        let data = &first_shares.encrypted_private_shares;
        let original_decrypted_secrets =
            wsts::util::decrypt(&keypair.secret_bytes(), data).unwrap();

        assert_eq!(new_decrypted_secrets, original_decrypted_secrets);
    }

    // =========================================================================
    // Step 7 - Re-run DKG with a new bitcoin block
    // -------------------------------------------------------------------------
    // - The signers will run DKG a third time. This will generate a new
    //   aggregate key and new secret shares.
    // =========================================================================

    // Let's run DKG a third time, where this time we expect new secret
    // shares to be generated.
    faucet.generate_block();
    // Now we wait for all signers to say that their tenure has completed,
    // which means that no more actions are going to take place for any of
    // the signers.
    wait_for_signers(&signers).await;

    for (_, db, keypair, _) in signers.iter() {
        let count = db.get_encrypted_dkg_shares_count().await.unwrap();
        assert_eq!(count, 3);

        let new_shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();
        let new_decrypted_secrets = wsts::util::decrypt(
            &keypair.secret_bytes(),
            &new_shares.encrypted_private_shares,
        )
        .unwrap();

        let first_shares = db
            .get_encrypted_dkg_shares(&adjusted_aggregate_key)
            .await
            .unwrap()
            .unwrap();
        let unadjusted_aggregate_key: PublicKey = first_shares
            .aggregate_key
            .combine(&tweak_public_key.negate(SECP256K1))
            .unwrap()
            .into();
        let second_shares = db
            .get_encrypted_dkg_shares(unadjusted_aggregate_key)
            .await
            .unwrap()
            .unwrap();

        // We should have a new aggregate key, so new scriptPubKey.
        assert_ne!(new_shares.aggregate_key, adjusted_aggregate_key);
        assert_ne!(new_shares.aggregate_key, unadjusted_aggregate_key);
        assert_ne!(new_shares.script_pubkey, first_shares.script_pubkey);
        assert_ne!(new_shares.script_pubkey, second_shares.script_pubkey);
        // Yes, these are different now too.
        assert_ne!(
            new_shares.started_at_bitcoin_block_hash,
            first_shares.started_at_bitcoin_block_hash
        );
        assert_ne!(
            new_shares.started_at_bitcoin_block_height,
            first_shares.started_at_bitcoin_block_height
        );
        // We didn't change the threshold or the signer set so these should
        // remain the same.
        assert_eq!(
            new_shares.signature_share_threshold,
            first_shares.signature_share_threshold
        );
        assert_eq!(
            new_shares.signer_set_public_keys,
            first_shares.signer_set_public_keys
        );

        // We should have different private shares from the previous two
        // times.
        let data = &first_shares.encrypted_private_shares;
        let first_decrypted_secrets = wsts::util::decrypt(&keypair.secret_bytes(), data).unwrap();

        assert_ne!(new_decrypted_secrets, first_decrypted_secrets);
    }

    for (_, db, _, _) in signers {
        testing::storage::drop_db(db).await;
    }
}

/// Test that three signers can successfully sign and broadcast a bitcoin
/// transaction.
///
/// The test setup is as follows:
/// 1. There are three "signers" contexts. Each context points to its own
///    real postgres database, and they have their own private key. Each
///    database is populated with the same data.
/// 2. Each context is given to a block observer, a tx signer, and a tx
///    coordinator, where these event loops are spawned as separate tasks.
/// 3. The signers communicate with our in-memory network struct.
/// 4. A real Emily server is running in the background.
/// 5. A real bitcoin-core node is running in the background.
/// 6. Stacks-core is mocked.
///
/// After the setup, the signers observe a bitcoin block and update their
/// databases. The coordinator then constructs a bitcoin transaction and
/// gets it signed. After it is signed the coordinator broadcasts it to
/// bitcoin-core.
///
/// To start the test environment do:
/// ```bash
/// make integration-env-up-ci
/// ```
///
/// then, once everything is up and running, run the test.
#[tokio::test]
async fn sign_bitcoin_transaction() {
    let (_, signer_key_pairs): (_, [Keypair; 3]) = testing::wallet::regtest_bootstrap_wallet();
    let (rpc, faucet) = regtest::initialize_blockchain();

    // We need to populate our databases, so let's fetch the data.
    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    testing_api::wipe_databases(&emily_client.config().as_testing())
        .await
        .unwrap();

    let network = WanNetwork::default();

    let chain_tip_info = get_canonical_chain_tip(rpc);

    // =========================================================================
    // Step 1 - Create a database, an associated context, and a Keypair for
    //          each of the signers in the signing set.
    // -------------------------------------------------------------------------
    // - We load the database with a bitcoin blocks going back to some
    //   genesis block.
    // =========================================================================
    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_emily_client(emily_client.clone())
            .with_mocked_stacks_client()
            .build();

        backfill_bitcoin_blocks(&db, rpc, &chain_tip_info.hash).await;

        let network = network.connect(&ctx);

        signers.push((ctx, db, kp, network));
    }

    // =========================================================================
    // Step 2 - Setup the stacks client mocks.
    // -------------------------------------------------------------------------
    // - Set up the mocks to that the block observer fetches at least one
    //   Stacks block. This is necessary because we need the stacks chain
    //   tip in the transaction coordinator.
    // - Set up the current-aggregate-key response to be `None`. This means
    //   that each coordinator will broadcast a rotate keys transaction.
    // =========================================================================
    let (broadcast_stacks_tx, rx) = tokio::sync::broadcast::channel(10);
    let stacks_tx_stream = BroadcastStream::new(rx);

    for (ctx, db, _, _) in signers.iter_mut() {
        let broadcast_stacks_tx = broadcast_stacks_tx.clone();
        let db = db.clone();

        mock_stacks_core(ctx, chain_tip_info.clone(), db, broadcast_stacks_tx).await;
    }

    // =========================================================================
    // Step 3 - Start the TxCoordinatorEventLoop, TxSignerEventLoop and
    //          BlockObserver processes for each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started, and
    //   we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        ctx.state().set_sbtc_contracts_deployed();
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: ctx.config().signer.bootstrap_signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            rng: rand::rngs::OsRng,
            last_presign_block: None,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        Sleep::for_millis(10).await;
    }

    // =========================================================================
    // Step 4 - Wait for DKG
    // -------------------------------------------------------------------------
    // - Once they are all running, generate a bitcoin block to kick off
    //   the database updating process.
    // - After they have the same view of the canonical bitcoin blockchain,
    //   the signers should all participate in DKG.
    // =========================================================================
    faucet.generate_block();

    // We first need to wait for bitcoin-core to send us all the
    // notifications so that we are up-to-date with the chain tip and DKG.
    wait_for_signers(&signers).await;

    let (_, db, _, _) = signers.first().unwrap();
    let shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();

    // =========================================================================
    // Step 5 - Prepare for deposits
    // -------------------------------------------------------------------------
    // - Before the signers can process anything, they need a UTXO to call
    //   their own. For that we make a donation, and confirm it. The
    //   signers should pick it up.
    // - Give a "depositor" some UTXOs so that they can make a deposit for
    //   sBTC.
    // =========================================================================
    let script_pub_key = shares.aggregate_key.signers_script_pubkey();
    let network = bitcoin::Network::Regtest;
    let address = Address::from_script(&script_pub_key, network).unwrap();

    faucet.send_to(100_000, &address);

    let depositor = Recipient::new(AddressType::P2tr);

    // Start off with some initial UTXOs to work with.

    faucet.send_to(50_000_000, &depositor.address);
    faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Step 6 - Make a proper deposit
    // -------------------------------------------------------------------------
    // - Use the UTXOs confirmed in step (5) to construct a proper deposit
    //   request transaction. Submit it and inform Emily about it.
    // =========================================================================
    // Now lets make a deposit transaction and submit it
    let utxo = depositor.get_utxos(rpc, None).pop().unwrap();

    let amount = 2_500_000;
    let signers_public_key = shares.aggregate_key.into();
    let max_fee = amount / 2;
    let (deposit_tx, deposit_request, _) =
        make_deposit_request(&depositor, amount, utxo, max_fee, signers_public_key);
    rpc.send_raw_transaction(&deposit_tx).unwrap();

    assert_eq!(deposit_tx.compute_txid(), deposit_request.outpoint.txid);

    let body = deposit_request.as_emily_request(&deposit_tx);
    let _ = deposit_api::create_deposit(emily_client.config(), body)
        .await
        .unwrap();

    // =========================================================================
    // Step 7 - Confirm the deposit and wait for the signers to do their
    //          job.
    // -------------------------------------------------------------------------
    // - Confirm the deposit request. This will trigger the block observer
    //   to reach out to Emily about deposits. It will have one so the
    //   signers should do basic validations and store the deposit request.
    // - Each TxSigner process should vote on the deposit request and
    //   submit the votes to each other.
    // - The coordinator should submit a sweep transaction. We check the
    //   mempool for its existence.
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    let (ctx, _, _, _) = signers.first().unwrap();
    let mut txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();
    assert_eq!(txids.len(), 1);

    let block_hash = faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Step 8 - Assertions
    // -------------------------------------------------------------------------
    // - The first transaction should be a rotate keys contract call. And
    //   because of how we set up our mocked stacks client, each
    //   coordinator submits a rotate keys transaction before they do
    //   anything else.
    // - The last transaction should be to mint sBTC using the
    //   complete-deposit contract call.
    // - Is the sweep transaction in our database.
    // - Does the sweep transaction spend to the signers' scriptPubKey.
    // =========================================================================
    let sleep_fut = Sleep::for_secs(5);
    let broadcast_stacks_txs: Vec<StacksTransaction> = stacks_tx_stream
        .take_until(sleep_fut)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    more_asserts::assert_ge!(broadcast_stacks_txs.len(), 2);
    // Check that the first N - 1 are all rotate keys contract calls.
    let rotate_keys_count = broadcast_stacks_txs.len() - 1;
    for tx in broadcast_stacks_txs.iter().take(rotate_keys_count) {
        assert_stacks_transaction_kind::<RotateKeysV1>(tx);
    }
    // Check that the Nth transaction is the complete-deposit contract
    // call.
    let tx = broadcast_stacks_txs.last().unwrap();
    assert_stacks_transaction_kind::<CompleteDepositV1>(tx);

    // Now lets check the bitcoin transaction, first we get it.
    let txid = txids.pop().unwrap();
    let tx_info = ctx
        .bitcoin_client
        .get_tx_info(&txid, &block_hash)
        .unwrap()
        .unwrap();
    // We check that the scriptPubKey of the first input is the signers'
    let actual_script_pub_key = tx_info.prevout(0).unwrap().script_pubkey.as_bytes();

    assert_eq!(actual_script_pub_key, script_pub_key.as_bytes());
    assert_eq!(&tx_info.tx.output[0].script_pubkey, &script_pub_key);

    // Lastly we check that out database has the sweep transaction
    let script_pubkey = sqlx::query_scalar::<_, model::ScriptPubKey>(
        r#"
        SELECT script_pubkey
        FROM sbtc_signer.bitcoin_tx_outputs
        WHERE txid = $1
          AND output_type = 'signers_output'
        "#,
    )
    .bind(txid.to_byte_array())
    .fetch_one(ctx.storage.pool())
    .await
    .unwrap();

    for (_, db, _, _) in signers {
        assert!(db.is_signer_script_pub_key(&script_pubkey).await.unwrap());
        testing::storage::drop_db(db).await;
    }
}

/// Test that three signers can successfully sign and broadcast a bitcoin
/// transaction where the inputs are locked by different aggregate keys.
///
/// The test setup is as follows:
/// 1. There are three "signers" contexts. Each context points to its own
///    real postgres database, and they have their own private key. Each
///    database is populated with the same data.
/// 2. Each context is given to a block observer, a tx signer, and a tx
///    coordinator, where these event loops are spawned as separate tasks.
/// 3. The signers communicate with our in-memory network struct.
/// 4. A real Emily server is running in the background.
/// 5. A real bitcoin-core node is running in the background.
/// 6. Stacks-core is mocked.
///
/// In this test the signers do quite a few things:
/// 1. Run DKG
/// 2. Sign and broadcast a rotate keys transaction.
/// 3. Sweep some deposited funds into their UTXO.
/// 4. Mint sBTC to the recipient.
/// 5. Run DKG again.
/// 6. Sweep two more deposits in one bitcoin transaction. These deposits
///    are locked with different aggregate keys.
/// 7. Have the signers UTXO locked by the aggregate key from the second
///    DKG run.
///
/// For step 5, we "hide" the DKG shares to get the signers to run DKG
/// again. We reveal them so that they can use them for a signing round.
///
/// To start the test environment do:
/// ```bash
/// make integration-env-up-ci
/// ```
///
/// then, once everything is up and running, run the test.
#[test(tokio::test)]
async fn sign_bitcoin_transaction_multiple_locking_keys() {
    let (_, signer_key_pairs): (_, [Keypair; 3]) = testing::wallet::regtest_bootstrap_wallet();
    let (rpc, faucet) = regtest::initialize_blockchain();

    // We need to populate our databases, so let's fetch the data.
    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    testing_api::wipe_databases(&emily_client.config().as_testing())
        .await
        .unwrap();

    let network = WanNetwork::default();

    let chain_tip_info = get_canonical_chain_tip(rpc);
    // This is the height where the signers will run DKG afterward. We
    // create 4 bitcoin blocks between now and when we want DKG to run a
    // second time:
    // 1. run DKG
    // 2. confirm a donation and a deposit request,
    // 3. confirm the sweep, mint sbtc
    // 4. run DKG again.
    let dkg_run_two_height = chain_tip_info.height + 4;

    // =========================================================================
    // Step 1 - Create a database, an associated context, and a Keypair for
    //          each of the signers in the signing set.
    // -------------------------------------------------------------------------
    // - We load the database with a bitcoin blocks going back to some
    //   genesis block.
    // =========================================================================
    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_emily_client(emily_client.clone())
            .with_mocked_stacks_client()
            .modify_settings(|settings| {
                settings.signer.dkg_target_rounds = NonZeroU32::new(2).unwrap();
                settings.signer.dkg_min_bitcoin_block_height = Some(dkg_run_two_height.into());
                settings.signer.bitcoin_processing_delay = Duration::from_millis(200);
            })
            .build();

        backfill_bitcoin_blocks(&db, rpc, &chain_tip_info.hash).await;

        let network = network.connect(&ctx);

        signers.push((ctx, db, kp, network));
    }

    // =========================================================================
    // Step 2 - Setup the stacks client mocks.
    // -------------------------------------------------------------------------
    // - Set up the mocks to that the block observer fetches at least one
    //   Stacks block. This is necessary because we need the stacks chain
    //   tip in the transaction coordinator.
    // - Set up the current-aggregate-key response to be `None`. This means
    //   that each coordinator will broadcast a rotate keys transaction.
    // =========================================================================
    let (broadcast_stacks_tx, rx) = tokio::sync::broadcast::channel(10);
    let stacks_tx_stream = BroadcastStream::new(rx);

    for (ctx, db, _, _) in signers.iter_mut() {
        let broadcast_stacks_tx = broadcast_stacks_tx.clone();
        let db = db.clone();

        mock_stacks_core(ctx, chain_tip_info.clone(), db, broadcast_stacks_tx).await;
    }

    // =========================================================================
    // Step 3 - Start the TxCoordinatorEventLoop, TxSignerEventLoop and
    //          BlockObserver processes for each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started, and
    //   we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        ctx.state().set_sbtc_contracts_deployed();
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: ctx.config().signer.bootstrap_signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            rng: rand::rngs::OsRng,
            last_presign_block: None,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        Sleep::for_millis(10).await;
    }

    // =========================================================================
    // Step 4 - Give deposits seed funds.
    // -------------------------------------------------------------------------
    // - Give "depositors" some UTXOs so that they can make deposits for
    //   sBTC.
    // =========================================================================
    let depositor1 = Recipient::new(AddressType::P2tr);
    let depositor2 = Recipient::new(AddressType::P2tr);

    // Start off with some initial UTXOs to work with.
    faucet.send_to(50_000_000, &depositor1.address);
    faucet.send_to(50_000_000, &depositor2.address);

    // =========================================================================
    // Step 5 - Wait for DKG
    // -------------------------------------------------------------------------
    // - Once they are all running, generate a bitcoin block to kick off
    //   the database updating process.
    // - After they have the same view of the canonical bitcoin blockchain,
    //   the signers should all participate in DKG.
    // =========================================================================

    // This should kick off DKG. We first need to wait for bitcoin-core to
    // send us all the notifications so that we are up-to-date with the
    // chain tip.
    faucet.generate_block();
    wait_for_signers(&signers).await;

    let (_, db, _, _) = signers.first().unwrap();
    let shares1 = db.get_latest_verified_dkg_shares().await.unwrap().unwrap();

    // =========================================================================
    // Step 6 - Prepare for deposits
    // -------------------------------------------------------------------------
    // - Before the signers can process anything, they need a UTXO to call
    //   their own. For that we make a donation, and confirm it. The
    //   signers should pick it up.
    // =========================================================================
    let script_pub_key1 = shares1.aggregate_key.signers_script_pubkey();
    let network = bitcoin::Network::Regtest;
    let address = Address::from_script(&script_pub_key1, network).unwrap();

    faucet.send_to(100_000, &address);

    // =========================================================================
    // Step 7 - Make a proper deposit
    // -------------------------------------------------------------------------
    // - Use the UTXOs confirmed in step (5) to construct a proper deposit
    //   request transaction. Submit it and inform Emily about it.
    // =========================================================================
    // Now lets make a deposit transaction and submit it
    let utxo = depositor1.get_utxos(rpc, None).pop().unwrap();

    let amount = 2_500_000;
    let signers_public_key = shares1.aggregate_key.into();
    let max_fee = amount / 2;
    let (deposit_tx, deposit_request, _) =
        make_deposit_request(&depositor1, amount, utxo, max_fee, signers_public_key);
    rpc.send_raw_transaction(&deposit_tx).unwrap();

    assert_eq!(deposit_tx.compute_txid(), deposit_request.outpoint.txid);

    let body = deposit_request.as_emily_request(&deposit_tx);
    let _ = deposit_api::create_deposit(emily_client.config(), body)
        .await
        .unwrap();

    // =========================================================================
    // Step 8 - Confirm the deposit and wait for the signers to do their
    //          job.
    // -------------------------------------------------------------------------
    // - Confirm the deposit request. The arrival of a new bitcoin block
    //   will trigger the block observer to reach out to Emily about
    //   deposits. Emily will have one so the signers should do basic
    //   validations and store the deposit request.
    // - Each TxSigner process should vote on the deposit request and
    //   submit the votes to each other.
    // - The coordinator should submit a sweep transaction. We check the
    //   mempool for its existence.
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    let (ctx, _, _, _) = signers.first().unwrap();
    let mut txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();
    assert_eq!(txids.len(), 1);

    let block_hash = faucet.generate_block();
    wait_for_signers(&signers).await;

    // Now lets check the bitcoin transaction, first we get it.
    let txid = txids.pop().unwrap();
    let tx_info = ctx
        .bitcoin_client
        .get_tx_info(&txid, &block_hash)
        .unwrap()
        .unwrap();
    // We check that the scriptPubKey of the first input is the signers'
    let actual_script_pub_key = tx_info.prevout(0).unwrap().script_pubkey.as_bytes();

    assert_eq!(actual_script_pub_key, script_pub_key1.as_bytes());
    assert_eq!(&tx_info.tx.output[0].script_pubkey, &script_pub_key1);

    // Now we check that each database has the sweep transaction and is
    // recognized as a signer script_pubkey.
    for (_, db, _, _) in signers.iter() {
        let script_pubkey = sqlx::query_scalar::<_, model::ScriptPubKey>(
            r#"
            SELECT script_pubkey
            FROM sbtc_signer.bitcoin_tx_outputs
            WHERE txid = $1
              AND output_type = 'signers_output'
            "#,
        )
        .bind(txid.to_byte_array())
        .fetch_one(db.pool())
        .await
        .unwrap();

        assert!(db.is_signer_script_pub_key(&script_pubkey).await.unwrap());
    }

    // =========================================================================
    // Step 9 - Run DKG Again
    // -------------------------------------------------------------------------
    // - The signers should run DKG again after they see the next bitcoin
    //   block, this was configured above.
    // =========================================================================
    for (_, db, _, _) in signers.iter() {
        let dkg_share_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dkg_shares;")
            .fetch_one(db.pool())
            .await
            .unwrap();

        assert_eq!(dkg_share_count, 1);
    }

    // After the next bitcoin block, each of the signers will think that
    // DKG needs to be run. So we need to wait for it.
    faucet.generate_block();

    // We first need to wait for bitcoin-core to send us all the
    // notifications so that we are up-to-date with the chain tip.
    wait_for_signers(&signers).await;

    let (_, db, _, _) = signers.first().unwrap();
    let shares2 = db.get_latest_verified_dkg_shares().await.unwrap().unwrap();

    // Check that we have new DKG shares for each of the signers.
    for (_, db, _, _) in signers.iter() {
        let dkg_share_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dkg_shares;")
            .fetch_one(db.pool())
            .await
            .unwrap();

        assert_eq!(dkg_share_count, 2);
    }

    // =========================================================================
    // Step 10 - Make two proper deposits
    // -------------------------------------------------------------------------
    // - Use the UTXOs generated in step (4) to construct two proper
    //   deposit request transactions. Submit them to the bitcoin network
    //   and then inform Emily.
    // - The two deposits are locked using two different aggregate keys,
    //   the old one and the new one.
    // =========================================================================
    // Now lets make a deposit transaction and submit it
    let utxo = depositor2.get_utxos(rpc, None).pop().unwrap();

    let amount = 3_500_000;
    let signers_public_key2 = shares2.aggregate_key.into();
    let max_fee = amount / 2;
    let (deposit_tx, deposit_request, _) =
        make_deposit_request(&depositor2, amount, utxo, max_fee, signers_public_key2);
    rpc.send_raw_transaction(&deposit_tx).unwrap();

    let body = deposit_request.as_emily_request(&deposit_tx);
    deposit_api::create_deposit(emily_client.config(), body)
        .await
        .unwrap();

    let utxo = depositor1.get_utxos(rpc, None).pop().unwrap();
    let amount = 4_500_000;
    let signers_public_key1 = shares1.aggregate_key.into();
    let max_fee = amount / 2;
    let (deposit_tx, deposit_request, _) =
        make_deposit_request(&depositor1, amount, utxo, max_fee, signers_public_key1);
    rpc.send_raw_transaction(&deposit_tx).unwrap();

    let body = deposit_request.as_emily_request(&deposit_tx);
    deposit_api::create_deposit(emily_client.config(), body)
        .await
        .unwrap();

    // =========================================================================
    // Step 11 - Confirm the deposit and wait for the signers to do their
    //           job.
    // -------------------------------------------------------------------------
    // - Confirm the deposit request. This will trigger the block observer
    //   to reach out to Emily about deposits. It will have two so the
    //   signers should do basic validations and store the deposit request.
    // - Each TxSigner process should vote on the deposit request and
    //   submit the votes to each other.
    // - The coordinator should submit a sweep transaction. We check the
    //   mempool for its existence.
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    let (ctx, _, _, _) = signers.first().unwrap();
    let mut txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();

    assert_eq!(txids.len(), 1);

    let block_hash = faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Step 12 - Assertions
    // -------------------------------------------------------------------------
    // - During each bitcoin block, the signers should sign and broadcast a
    //   rotate keys contract call. This is because they haven't received a
    //   rotate-keys event, so they think that they haven't confirmed a
    //   rotate-keys contract call.
    // - After each sweep transaction is confirmed, the coordinator should
    //   also broadcast a complete-deposit contract call. There should be
    //   duplicates here as well since the signers do not receive events
    //   about the success of the contract call.
    // - They should have sweep transactions in their database.
    // - Check that the sweep transaction spend to the signers'
    //   scriptPubKey.
    // =========================================================================
    let sleep_fut = Sleep::for_secs(5);
    let broadcast_stacks_txs: Vec<StacksTransaction> = stacks_tx_stream
        .take_until(sleep_fut)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let mut complete_deposit_txs: Vec<StacksTransaction> = broadcast_stacks_txs
        .iter()
        .filter(|tx| match &tx.payload {
            TransactionPayload::ContractCall(cc) => {
                cc.function_name.as_str() == CompleteDepositV1::FUNCTION_NAME
            }
            _ => false,
        })
        .cloned()
        .collect();

    // We should try to mint for each of the three deposits. But since the
    // signers continually submit Stacks transaction for each swept
    // deposit, we need to deduplicate the contract calls before checking.
    complete_deposit_txs.sort_by_key(|tx| match &tx.payload {
        // The first argument in the contract call is the transaction ID
        // of the deposit
        TransactionPayload::ContractCall(cc) => match cc.function_args.first() {
            Some(ClarityValue::Sequence(SequenceData::Buffer(buff))) => buff.data.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    });
    complete_deposit_txs.dedup_by_key(|tx| match &tx.payload {
        TransactionPayload::ContractCall(cc) => cc.function_args.first().cloned(),
        _ => None,
    });

    // We ran DKG twice, so we should observe two distinct rotate-keys
    // contract calls. Since we call rotate keys with each bitcoin block we
    // need to filter out the duplicates.
    let mut rotate_keys_txs: Vec<StacksTransaction> = broadcast_stacks_txs
        .iter()
        .filter(|tx| match &tx.payload {
            TransactionPayload::ContractCall(cc) => {
                cc.function_name.as_str() == RotateKeysV1::FUNCTION_NAME
            }
            _ => false,
        })
        .cloned()
        .collect();
    rotate_keys_txs.dedup_by_key(|tx| match &tx.payload {
        // The second argument in the contract call is the aggregate key
        TransactionPayload::ContractCall(cc) => cc.function_args.get(1).cloned(),
        _ => None,
    });

    // These should all be rotate-keys contract calls.
    for tx in rotate_keys_txs.iter() {
        assert_stacks_transaction_kind::<RotateKeysV1>(tx);
    }
    // We ran DKG twice, so two rotate-keys contract calls.
    assert_eq!(rotate_keys_txs.len(), 2);

    // Check that these are all complete-deposit contract calls.
    for tx in complete_deposit_txs.iter() {
        assert_stacks_transaction_kind::<CompleteDepositV1>(tx);
    }
    // There were three deposits, so three distinct complete-deposit
    // contract calls.
    assert_eq!(complete_deposit_txs.len(), 3);

    // Now lets check the bitcoin transaction, first we get it.
    let txid = txids.pop().unwrap();
    let tx_info = ctx
        .bitcoin_client
        .get_tx_info(&txid, &block_hash)
        .expect("Error getting transaction info")
        .expect("Expected to be able to get the transaction info from bitcoin-core");
    // We check that the scriptPubKey of the first input is the signers'
    // old ScriptPubkey
    let actual_script_pub_key = tx_info.prevout(0).unwrap().script_pubkey.as_bytes();
    assert_eq!(actual_script_pub_key, script_pub_key1.as_bytes());

    // The scriptPubkey of the new signer UTXO should be from the new
    // aggregate key.
    let script_pub_key2 = shares2.aggregate_key.signers_script_pubkey();
    assert_eq!(&tx_info.tx.output[0].script_pubkey, &script_pub_key2);
    // The transaction should sweep two deposits, so 3 inputs total because
    // of the signers' UTXO.
    assert_eq!(tx_info.inputs().len(), 3);
    // No withdrawals, so 2 outputs
    assert_eq!(tx_info.outputs().len(), 2);

    for (_, db, _, _) in signers {
        // Lastly we check that our database has the sweep transaction
        let script_pubkey = sqlx::query_scalar::<_, model::ScriptPubKey>(
            r#"
            SELECT script_pubkey
            FROM sbtc_signer.bitcoin_tx_outputs
            WHERE txid = $1
              AND output_type = 'signers_output'
            "#,
        )
        .bind(txid.to_byte_array())
        .fetch_one(db.pool())
        .await
        .unwrap();

        assert!(db.is_signer_script_pub_key(&script_pubkey).await.unwrap());
        testing::storage::drop_db(db).await;
    }
}

/// Test that three dkg_id and sign_id are set correctly during DKG and
/// signing rounds.
///
/// The test setup is as follows:
/// 1. There are three "signers" contexts. Each context points to its own
///    real postgres database, and they have their own private key. Each
///    database is populated with the same data.
/// 2. Each context is given to a block observer, a tx signer, and a tx
///    coordinator, where these event loops are spawned as separate tasks.
/// 3. The signers communicate with our in-memory network struct.
/// 4. A real Emily server is running in the background.
/// 5. A real bitcoin-core node is running in the background.
/// 6. Stacks-core is mocked.
///
/// After the setup, the signers observe a bitcoin block and update their
/// databases. The coordinator then constructs a bitcoin transaction and
/// gets it signed. After it is signed the coordinator broadcasts it to
/// bitcoin-core.
///
/// To start the test environment do:
/// ```bash
/// make integration-env-up-ci
/// ```
///
/// then, once everything is up and running, run the test.
#[tokio::test]
async fn wsts_ids_set_during_dkg_and_signing_rounds() {
    let (_, signer_key_pairs): (_, [Keypair; 3]) = testing::wallet::regtest_bootstrap_wallet();
    let (rpc, faucet) = regtest::initialize_blockchain();

    // We need to populate our databases, so let's fetch the data.
    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    testing_api::wipe_databases(&emily_client.config().as_testing())
        .await
        .unwrap();

    let network = WanNetwork::default();

    let chain_tip_info = get_canonical_chain_tip(rpc);

    // =========================================================================
    // Step 1 - Create a database, an associated context, and a Keypair for
    //          each of the signers in the signing set.
    // -------------------------------------------------------------------------
    // - We load the database with a bitcoin blocks going back to some
    //   genesis block.
    // =========================================================================
    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_emily_client(emily_client.clone())
            .with_mocked_stacks_client()
            .build();

        backfill_bitcoin_blocks(&db, rpc, &chain_tip_info.hash).await;

        let network = network.connect(&ctx);

        signers.push((ctx, db, kp, network));
    }

    // =========================================================================
    // Step 2 - Setup the stacks client mocks.
    // -------------------------------------------------------------------------
    // - Set up the mocks to that the block observer fetches at least one
    //   Stacks block. This is necessary because we need the stacks chain
    //   tip in the transaction coordinator.
    // - Set up the current-aggregate-key response to be `None`. This means
    //   that each coordinator will broadcast a rotate keys transaction.
    // =========================================================================
    let (broadcast_stacks_tx, _rx) = tokio::sync::broadcast::channel(10);

    for (ctx, db, _, _) in signers.iter_mut() {
        let broadcast_stacks_tx = broadcast_stacks_tx.clone();
        let db = db.clone();

        mock_stacks_core(ctx, chain_tip_info.clone(), db, broadcast_stacks_tx).await;
    }

    // =========================================================================
    // Step 3 - Start the TxCoordinatorEventLoop, TxSignerEventLoop and
    //          BlockObserver processes for each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started, and
    //   we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        ctx.state().set_sbtc_contracts_deployed();
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: ctx.config().signer.bootstrap_signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            rng: rand::rngs::OsRng,
            last_presign_block: None,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        Sleep::for_millis(10).await;
    }

    // =========================================================================
    // Step 4 - Wait for DKG
    // -------------------------------------------------------------------------
    // - Once they are all running, generate a bitcoin block to kick off
    //   the database updating process.
    // - After they have the same view of the canonical bitcoin blockchain,
    //   the signers should all participate in DKG.
    // =========================================================================
    let (ctx, db, _, _) = signers.first().unwrap();
    let stream = BroadcastStream::new(ctx.get_signal_receiver());

    let block_hash = faucet.generate_block();
    wait_for_signers(&signers).await;

    let wsts_message_filter = |msg| {
        let Ok(SignerSignal::Event(SignerEvent::P2P(P2PEvent::MessageReceived(msg)))) = msg else {
            return std::future::ready(None);
        };

        let Payload::WstsMessage(wsts_msg) = msg.inner.payload else {
            return std::future::ready(None);
        };

        std::future::ready(Some(wsts_msg))
    };

    let wsts_messages = stream
        .take_until(tokio::time::sleep(Duration::from_secs(1)))
        .filter_map(wsts_message_filter)
        .collect::<Vec<_>>()
        .await;

    assert!(!wsts_messages.is_empty());

    let header = rpc.get_block_header_info(&block_hash).unwrap();

    // Let's make sure the DKG ID matched what we expected it to be, which
    // is the block height of the most recent bitcoin block. We do not
    // check the sign ID here because these messages should pertain to DKG.
    // Right now we use the FROST coordinator for the signing round of DKG
    // and we do not set the sign ID for the FROST coordinator.
    wsts_messages.iter().for_each(|msg| {
        let dkg_id = match &msg.inner {
            wsts::net::Message::DkgBegin(msg) => msg.dkg_id,
            wsts::net::Message::DkgEndBegin(msg) => msg.dkg_id,
            wsts::net::Message::DkgEnd(msg) => msg.dkg_id,
            wsts::net::Message::DkgPrivateBegin(msg) => msg.dkg_id,
            wsts::net::Message::DkgPrivateShares(msg) => msg.dkg_id,
            wsts::net::Message::DkgPublicShares(msg) => msg.dkg_id,
            wsts::net::Message::NonceRequest(msg) => msg.dkg_id,
            wsts::net::Message::NonceResponse(msg) => msg.dkg_id,
            wsts::net::Message::SignatureShareRequest(msg) => msg.dkg_id,
            wsts::net::Message::SignatureShareResponse(msg) => msg.dkg_id,
        };

        assert_eq!(dkg_id, header.height as u64);
    });

    let shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();

    // =========================================================================
    // Step 5 - Prepare for deposits
    // -------------------------------------------------------------------------
    // - Before the signers can process anything, they need a UTXO to call
    //   their own. For that we make a donation, and confirm it. The
    //   signers should pick it up.
    // - Give a "depositor" some UTXOs so that they can make a deposit for
    //   sBTC.
    // =========================================================================
    let script_pub_key = shares.aggregate_key.signers_script_pubkey();
    let network = bitcoin::Network::Regtest;
    let address = Address::from_script(&script_pub_key, network).unwrap();

    faucet.send_to(100_000, &address);

    let depositor = Recipient::new(AddressType::P2tr);

    // Start off with some initial UTXOs to work with.

    faucet.send_to(50_000_000, &depositor.address);
    faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Step 6 - Make a proper deposit
    // -------------------------------------------------------------------------
    // - Use the UTXOs confirmed in step (5) to construct a proper deposit
    //   request transaction. Submit it and inform Emily about it.
    // =========================================================================
    // Now lets make a deposit transaction and submit it
    let utxo = depositor.get_utxos(rpc, None).pop().unwrap();

    let amount = 2_500_000;
    let signers_public_key = shares.aggregate_key.into();
    let max_fee = amount / 2;
    let (deposit_tx, deposit_request, _) =
        make_deposit_request(&depositor, amount, utxo, max_fee, signers_public_key);
    rpc.send_raw_transaction(&deposit_tx).unwrap();

    assert_eq!(deposit_tx.compute_txid(), deposit_request.outpoint.txid);

    let body = deposit_request.as_emily_request(&deposit_tx);
    let _ = deposit_api::create_deposit(emily_client.config(), body)
        .await
        .unwrap();

    // =========================================================================
    // Step 7 - Confirm the deposit and wait for the signers to do their
    //          job.
    // -------------------------------------------------------------------------
    // - Confirm the deposit request. This will trigger the block observer
    //   to reach out to Emily about deposits. It will have one so the
    //   signers should do basic validations and store the deposit request.
    // - Each TxSigner process should vote on the deposit request and
    //   submit the votes to each other.
    // - The coordinator should submit a sweep transaction. We check the
    //   mempool for its existence.
    // =========================================================================
    let stream = BroadcastStream::new(ctx.get_signal_receiver());
    let block_hash: BitcoinBlockHash = faucet.generate_block().into();
    wait_for_signers(&signers).await;

    let txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();
    assert_eq!(txids.len(), 1);

    // =========================================================================
    // Step 8 - Check that the WSTS messages have the expected dkg_id and
    //          sign_id
    // =========================================================================
    let wsts_messages = stream
        .take_until(tokio::time::sleep(Duration::from_secs(1)))
        .filter_map(wsts_message_filter)
        .collect::<Vec<_>>()
        .await;

    assert!(!wsts_messages.is_empty());

    let chain_tip_header = rpc.get_block_header_info(&block_hash).unwrap();

    // Let's check that all of the sign IDs for these signing rounds are the
    // expected value.
    for msg in wsts_messages.iter() {
        let dkg_id = match &msg.inner {
            wsts::net::Message::DkgBegin(msg) => msg.dkg_id,
            wsts::net::Message::DkgEndBegin(msg) => msg.dkg_id,
            wsts::net::Message::DkgEnd(msg) => msg.dkg_id,
            wsts::net::Message::DkgPrivateBegin(msg) => msg.dkg_id,
            wsts::net::Message::DkgPrivateShares(msg) => msg.dkg_id,
            wsts::net::Message::DkgPublicShares(msg) => msg.dkg_id,
            wsts::net::Message::NonceRequest(msg) => msg.dkg_id,
            wsts::net::Message::NonceResponse(msg) => msg.dkg_id,
            wsts::net::Message::SignatureShareRequest(msg) => msg.dkg_id,
            wsts::net::Message::SignatureShareResponse(msg) => msg.dkg_id,
        };

        // The DKG ID set here should be the height associated with the
        // original block when DKG was run.
        assert_eq!(dkg_id, header.height as u64);
        more_asserts::assert_lt!(dkg_id, chain_tip_header.height as u64);

        // The signature share response does not contain the message that
        // was signed, so we skip it in the match below.
        let (sign_id, message) = match &msg.inner {
            wsts::net::Message::NonceRequest(msg) => (msg.sign_id, msg.message.clone()),
            wsts::net::Message::NonceResponse(msg) => (msg.sign_id, msg.message.clone()),
            wsts::net::Message::SignatureShareRequest(msg) => (msg.sign_id, msg.message.clone()),
            _ => continue,
        };

        let expected_sign_id = construct_signing_round_id(&message, &block_hash);
        // When the WSTS coordinator state machine starts a new signing
        // round, it automatically increments the sign ID by 1. So we
        // adjust our expectations here.
        assert_eq!(sign_id - 1, expected_sign_id);
    }

    for (_, db, _, _) in signers {
        testing::storage::drop_db(db).await;
    }
}

/// Test that coordinator stops their duties after submitting a rotate-keys
/// contract call.
///
/// The test setup is as follows:
/// 1. There are three "signers" contexts. Each context points to its own
///    real postgres database, and they have their own private key. Each
///    database is populated with the same data.
/// 2. Each context is given to a block observer, a tx signer, and a tx
///    coordinator, where these event loops are spawned as separate tasks.
/// 3. The signers communicate with our in-memory network struct.
/// 4. A real Emily server is running in the background.
/// 5. A real bitcoin-core node is running in the background.
/// 6. Stacks-core is mocked.
///
/// In this test the signers do quite a few things:
/// 1. Run DKG
/// 2. Sign and broadcast a rotate keys transaction.
/// 3. Sweep some deposited funds into their UTXO.
/// 4. Mint sBTC to the recipient.
/// 5. Submit another deposit request, but check that it does not get swept
///    during the same tenure as the second DKG run.
/// 6. Run DKG again.
/// 7. Have the signers UTXO locked by the aggregate key from the second
///    DKG run.
///
/// To start the test environment do:
/// ```bash
/// make integration-env-up-ci
/// ```
///
/// then, once everything is up and running, run the test.
#[test(tokio::test)]
async fn skip_signer_activites_after_key_rotation() {
    let (_, signer_key_pairs): (_, [Keypair; 3]) = testing::wallet::regtest_bootstrap_wallet();
    let (rpc, faucet) = regtest::initialize_blockchain();

    // We need to populate our databases, so let's fetch the data.
    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    testing_api::wipe_databases(&emily_client.config().as_testing())
        .await
        .unwrap();

    let network = WanNetwork::default();

    let chain_tip_info = get_canonical_chain_tip(rpc);
    // This is the height where the signers will run DKG afterward. We
    // create 4 bitcoin blocks between now and when we want DKG to run a
    // second time:
    // 1. run DKG
    // 2. confirm a donation and a deposit request,
    // 3. confirm the sweep
    // 4. run DKG again.
    let dkg_run_two_height = chain_tip_info.height + 4;

    // =========================================================================
    // Step 1 - Create a database, an associated context, and a Keypair for
    //          each of the signers in the signing set.
    // -------------------------------------------------------------------------
    // - We load the database with a bitcoin blocks going back to some
    //   genesis block.
    // =========================================================================
    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_emily_client(emily_client.clone())
            .with_mocked_stacks_client()
            .modify_settings(|settings| {
                settings.signer.dkg_target_rounds = NonZeroU32::new(2).unwrap();
                settings.signer.dkg_min_bitcoin_block_height = Some(dkg_run_two_height.into());
                settings.signer.bitcoin_processing_delay = Duration::from_millis(200);
            })
            .build();

        backfill_bitcoin_blocks(&db, rpc, &chain_tip_info.hash).await;

        let network = network.connect(&ctx);

        signers.push((ctx, db, kp, network));
    }

    // =========================================================================
    // Step 2 - Setup the stacks client mocks.
    // -------------------------------------------------------------------------
    // - Set up the mocks to that the block observer fetches at least one
    //   Stacks block. This is necessary because we need the stacks chain
    //   tip in the transaction coordinator.
    // - Set up the current-aggregate-key response to be `None`. This means
    //   that each coordinator will broadcast a rotate keys transaction.
    // =========================================================================
    let (broadcast_stacks_tx, rx) = tokio::sync::broadcast::channel(10);
    let stacks_tx_stream = BroadcastStream::new(rx);

    for (ctx, db, _, _) in signers.iter_mut() {
        let broadcast_stacks_tx = broadcast_stacks_tx.clone();
        let db = db.clone();

        mock_stacks_core(ctx, chain_tip_info.clone(), db, broadcast_stacks_tx).await;
    }

    // =========================================================================
    // Step 3 - Start the TxCoordinatorEventLoop, TxSignerEventLoop and
    //          BlockObserver processes for each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started, and
    //   we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        ctx.state().set_sbtc_contracts_deployed();
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: ctx.config().signer.bootstrap_signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            rng: rand::rngs::OsRng,
            last_presign_block: None,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // =========================================================================
    // Step 4 - Give deposits seed funds.
    // -------------------------------------------------------------------------
    // - Give "depositors" some UTXOs so that they can make deposits for
    //   sBTC.
    // =========================================================================
    let depositor1 = Recipient::new(AddressType::P2tr);
    let depositor2 = Recipient::new(AddressType::P2tr);

    // Start off with some initial UTXOs to work with.
    faucet.send_to(50_000_000, &depositor1.address);
    faucet.send_to(50_000_000, &depositor2.address);

    // =========================================================================
    // Step 5 - Wait for DKG
    // -------------------------------------------------------------------------
    // - Once they are all running, generate a bitcoin block to kick off
    //   the database updating process.
    // - After they have the same view of the canonical bitcoin blockchain,
    //   the signers should all participate in DKG.
    // =========================================================================

    // This should kick off DKG.
    faucet.generate_block();

    // We first need to wait for bitcoin-core to send us all the
    // notifications so that we are up-to-date with the chain tip.
    wait_for_signers(&signers).await;

    let (_, db, _, _) = signers.first().unwrap();
    let shares1 = db.get_latest_verified_dkg_shares().await.unwrap().unwrap();

    // =========================================================================
    // Step 6 - Prepare for deposits
    // -------------------------------------------------------------------------
    // - Before the signers can process anything, they need a UTXO to call
    //   their own. For that we make a donation, and confirm it. The
    //   signers should pick it up.
    // =========================================================================
    let script_pub_key1 = shares1.aggregate_key.signers_script_pubkey();
    let network = bitcoin::Network::Regtest;
    let address = Address::from_script(&script_pub_key1, network).unwrap();

    faucet.send_to(100_000, &address);

    // =========================================================================
    // Step 7 - Make a proper deposit
    // -------------------------------------------------------------------------
    // - Use the UTXOs confirmed in step (5) to construct a proper deposit
    //   request transaction. Submit it and inform Emily about it.
    // =========================================================================
    // Now lets make a deposit transaction and submit it
    let utxo = depositor1.get_utxos(rpc, None).pop().unwrap();

    let amount = 2_500_000;
    let signers_public_key = shares1.aggregate_key.into();
    let max_fee = amount / 2;
    let (deposit_tx, deposit_request, _) =
        make_deposit_request(&depositor1, amount, utxo, max_fee, signers_public_key);
    rpc.send_raw_transaction(&deposit_tx).unwrap();

    assert_eq!(deposit_tx.compute_txid(), deposit_request.outpoint.txid);

    let body = deposit_request.as_emily_request(&deposit_tx);
    let _ = deposit_api::create_deposit(emily_client.config(), body)
        .await
        .unwrap();

    // =========================================================================
    // Step 8 - Confirm the deposit and wait for the signers to do their
    //          job.
    // -------------------------------------------------------------------------
    // - Confirm the deposit request. The arrival of a new bitcoin block
    //   will trigger the block observer to reach out to Emily about
    //   deposits. Emily will have one so the signers should do basic
    //   validations and store the deposit request.
    // - Each TxSigner process should vote on the deposit request and
    //   submit the votes to each other.
    // - The coordinator should submit a sweep transaction. We check the
    //   mempool for its existence.
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    let (ctx, _, _, _) = signers.first().unwrap();
    let mut txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();
    assert_eq!(txids.len(), 1);

    let block_hash = faucet.generate_block();
    wait_for_signers(&signers).await;

    // Now lets check the bitcoin transaction, first we get it.
    let txid = txids.pop().unwrap();
    let tx_info = ctx
        .bitcoin_client
        .get_tx_info(&txid, &block_hash)
        .unwrap()
        .unwrap();
    // We check that the scriptPubKey of the first input is the signers'
    let actual_script_pub_key = tx_info.prevout(0).unwrap().script_pubkey.as_bytes();

    assert_eq!(actual_script_pub_key, script_pub_key1.as_bytes());
    assert_eq!(&tx_info.tx.output[0].script_pubkey, &script_pub_key1);

    // Now we check that each database has the sweep transaction and is
    // recognized as a signer script_pubkey.
    for (_, db, _, _) in signers.iter() {
        let script_pubkey = sqlx::query_scalar::<_, model::ScriptPubKey>(
            r#"
            SELECT script_pubkey
            FROM sbtc_signer.bitcoin_tx_outputs
            WHERE txid = $1
              AND output_type = 'signers_output'
            "#,
        )
        .bind(txid.to_byte_array())
        .fetch_one(db.pool())
        .await
        .unwrap();

        assert!(db.is_signer_script_pub_key(&script_pubkey).await.unwrap());
    }

    // =========================================================================
    // Step 9 - Run DKG Again
    // -------------------------------------------------------------------------
    // - Submit another deposit request. This should not be swept because
    //   it should happen after the signers have successfully run DKG and
    //   submitted a rotate-keys contract call.
    // - The signers should run DKG again after they see the next bitcoin
    //   block, this was configured above.
    // - Create and confirm the deposit request. This will trigger the
    //   block observer to reach out to Emily about deposits. It will have
    //   two so the signers should do basic validations and store the
    //   deposit request.
    // - Each TxSigner process should vote on the deposit request and
    //   submit the votes to each other.
    // - The coordinator should not submit a sweep transaction because DKG
    //   has been run again.
    // =========================================================================
    for (_, db, _, _) in signers.iter() {
        let dkg_share_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dkg_shares;")
            .fetch_one(db.pool())
            .await
            .unwrap();

        assert_eq!(dkg_share_count, 1);
    }

    let utxo = depositor2.get_utxos(rpc, None).pop().unwrap();

    let amount = 3_500_000;
    let max_fee = amount / 2;
    let (deposit_tx, deposit_request, _) =
        make_deposit_request(&depositor2, amount, utxo, max_fee, signers_public_key);
    rpc.send_raw_transaction(&deposit_tx).unwrap();

    let body = deposit_request.as_emily_request(&deposit_tx);
    deposit_api::create_deposit(emily_client.config(), body)
        .await
        .unwrap();

    // After the next bitcoin block, each of the signers will think that
    // DKG needs to be run. They also have a deposit request to process,
    // but they should skip it because they would have successfully run
    // DKG and submitted a rotate-keys contract call.
    faucet.generate_block();
    wait_for_signers(&signers).await;

    // We should not have any transactions in the mempool, because the
    // coordinator should bail after submitting a rotate-keys contract
    // call.
    let (ctx, _, _, _) = signers.first().unwrap();
    let txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();
    assert_eq!(txids.len(), 0);

    // Check that we have new DKG shares for each of the signers.
    for (_, db, _, _) in signers.iter() {
        let dkg_share_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dkg_shares;")
            .fetch_one(db.pool())
            .await
            .unwrap();

        assert_eq!(dkg_share_count, 2);
    }

    // =========================================================================
    // Step 10 - Confirm the deposit and wait for the signers to do their
    //           job.
    // -------------------------------------------------------------------------
    // - The coordinator should submit a sweep transaction. We check the
    //   mempool for its existence.
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    let (ctx, _, _, _) = signers.first().unwrap();
    let mut txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();

    assert_eq!(txids.len(), 1);

    let block_hash = faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Step 11 - Assertions
    // -------------------------------------------------------------------------
    // - After each sweep transaction is confirmed, the coordinator should
    //   broadcast a complete-deposit contract call. There should be
    //   duplicates here as well since the signers do not receive events
    //   about the success of the contract call.
    // - They should have sweep transactions in their database.
    // - Check that the sweep transaction spend to the signers'
    //   scriptPubKey.
    // =========================================================================
    let sleep_fut = tokio::time::sleep(Duration::from_secs(5));
    let broadcast_stacks_txs: Vec<StacksTransaction> = stacks_tx_stream
        .take_until(sleep_fut)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let mut complete_deposit_txs: Vec<StacksTransaction> = broadcast_stacks_txs
        .iter()
        .filter(|tx| match &tx.payload {
            TransactionPayload::ContractCall(cc) => {
                cc.function_name.as_str() == CompleteDepositV1::FUNCTION_NAME
            }
            _ => false,
        })
        .cloned()
        .collect();

    // We should try to mint for each of the two deposits. But since the
    // signers continually submit Stacks transaction for each swept
    // deposit, we need to deduplicate the contract calls before checking.
    complete_deposit_txs.sort_by_key(|tx| match &tx.payload {
        // The first argument in the contract call is the transaction ID
        // of the deposit
        TransactionPayload::ContractCall(cc) => match cc.function_args.first() {
            Some(ClarityValue::Sequence(SequenceData::Buffer(buff))) => buff.data.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    });
    complete_deposit_txs.dedup_by_key(|tx| match &tx.payload {
        TransactionPayload::ContractCall(cc) => cc.function_args.first().cloned(),
        _ => None,
    });

    let rotate_keys_txs: Vec<StacksTransaction> = broadcast_stacks_txs
        .iter()
        .filter(|tx| match &tx.payload {
            TransactionPayload::ContractCall(cc) => {
                cc.function_name.as_str() == RotateKeysV1::FUNCTION_NAME
            }
            _ => false,
        })
        .cloned()
        .collect();

    // These should all be rotate-keys contract calls.
    for tx in rotate_keys_txs.iter() {
        assert_stacks_transaction_kind::<RotateKeysV1>(tx);
    }
    // We ran DKG twice, so two rotate-keys contract calls.
    assert_eq!(rotate_keys_txs.len(), 2);

    // Check that these are all complete-deposit contract calls.
    for tx in complete_deposit_txs.iter() {
        assert_stacks_transaction_kind::<CompleteDepositV1>(tx);
    }
    // There were two deposits, so two distinct complete-deposit contract
    // calls.
    assert_eq!(complete_deposit_txs.len(), 2);

    // Now lets check the bitcoin transaction, first we get it.
    let txid = txids.pop().unwrap();
    let tx_info = ctx
        .bitcoin_client
        .get_tx_info(&txid, &block_hash)
        .unwrap()
        .unwrap();
    // We check that the scriptPubKey of the first input is the signers'
    // old ScriptPubkey
    let actual_script_pub_key = tx_info.prevout(0).unwrap().script_pubkey.as_bytes();
    assert_eq!(actual_script_pub_key, script_pub_key1.as_bytes());

    let shares2 = {
        let (_, db, _, _) = signers.first().unwrap();
        db.get_latest_verified_dkg_shares().await.unwrap().unwrap()
    };

    // The scriptPubkey of the new signer UTXO should be from the new
    // aggregate key.
    let script_pub_key2 = shares2.aggregate_key.signers_script_pubkey();
    assert_eq!(&tx_info.tx.output[0].script_pubkey, &script_pub_key2);
    // The transaction should sweep two deposits, so 2 inputs total because
    // of the signers' UTXO.
    assert_eq!(tx_info.inputs().len(), 2);
    // No withdrawals, so 2 outputs
    assert_eq!(tx_info.outputs().len(), 2);

    for (_, db, _, _) in signers {
        // Lastly we check that our database has the sweep transaction
        let script_pubkey = sqlx::query_scalar::<_, model::ScriptPubKey>(
            r#"
            SELECT script_pubkey
            FROM sbtc_signer.bitcoin_tx_outputs
            WHERE txid = $1
              AND output_type = 'signers_output'
            "#,
        )
        .bind(txid.to_byte_array())
        .fetch_one(db.pool())
        .await
        .unwrap();

        assert!(db.is_signer_script_pub_key(&script_pubkey).await.unwrap());
        testing::storage::drop_db(db).await;
    }
}

/// Check that we do not try to deploy the smart contracts or rotate keys
/// if we think things are up-to-date.
#[tokio::test]
async fn skip_smart_contract_deployment_and_key_rotation_if_up_to_date() {
    let (_, signer_key_pairs): (_, [Keypair; 3]) = testing::wallet::regtest_bootstrap_wallet();
    let (rpc, faucet) = regtest::initialize_blockchain();

    let mut rng = get_rng();
    // We need to populate our databases, so let's fetch the data.
    let emily_client: EmilyClient = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    testing_api::wipe_databases(&emily_client.config().as_testing())
        .await
        .unwrap();

    let network = WanNetwork::default();

    let chain_tip_info = get_canonical_chain_tip(rpc);

    // =========================================================================
    // Step 1 - Create a database, an associated context, and a Keypair for
    //          each of the signers in the signing set.
    // -------------------------------------------------------------------------
    // - We load the database with a bitcoin blocks going back to some
    //   genesis block.
    // =========================================================================
    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_emily_client(emily_client.clone())
            .with_mocked_stacks_client()
            .build();

        backfill_bitcoin_blocks(&db, rpc, &chain_tip_info.hash).await;

        let network = network.connect(&ctx);

        signers.push((ctx, db, kp, network));
    }

    // =========================================================================
    // Step 2 - Setup the stacks client mocks.
    // -------------------------------------------------------------------------
    // - Set up the mocks to that the block observer fetches at least one
    //   Stacks block. This is necessary because we need the stacks chain
    //   tip in the transaction coordinator.
    // - Set up the signer set info to be `Some(_)` where the details match
    //   the bootstrap signer set info.
    // - Write DKG shares to the database. This, with the above point,
    //   means that each coordinator will not run DKG or broadcast a rotate
    //   keys transaction.
    // =========================================================================
    let (ctx, _, _, _) = signers.first().unwrap();
    let bootstrap_signing_set = ctx.config().signer.bootstrap_signing_set.clone();
    let shares: EncryptedDkgShares = EncryptedDkgShares {
        signer_set_public_keys: bootstrap_signing_set.into_iter().collect(),
        signature_share_threshold: ctx.config().signer.bootstrap_signatures_required,
        dkg_shares_status: DkgSharesStatus::Verified,
        ..Faker.fake_with_rng(&mut rng)
    };
    for (ctx, db, _, _) in signers.iter_mut() {
        let signer_set_info: SignerSetInfo = shares.clone().into();

        db.write_encrypted_dkg_shares(&shares).await.unwrap();
        ctx.with_stacks_client(|client| {
            client
                .expect_get_tenure_info()
                .returning(move || Box::pin(std::future::ready(Ok(DUMMY_TENURE_INFO.clone()))));

            client.expect_get_block().returning(|_| {
                let response = Ok(NakamotoBlock {
                    header: NakamotoBlockHeader::empty(),
                    txs: vec![],
                });
                Box::pin(std::future::ready(response))
            });

            let chain_tip = model::BitcoinBlockHash::from(chain_tip_info.hash);
            client.expect_get_tenure().returning(move |_| {
                let mut tenure = TenureBlocks::nearly_empty().unwrap();
                tenure.anchor_block_hash = chain_tip;
                Box::pin(std::future::ready(Ok(tenure)))
            });

            client.expect_get_pox_info().returning(|| {
                let response = serde_json::from_str::<RPCPoxInfoData>(GET_POX_INFO_JSON)
                    .map_err(Error::JsonSerialize);
                Box::pin(std::future::ready(response))
            });

            client
                .expect_estimate_fees()
                .returning(|_, _, _| Box::pin(std::future::ready(Ok(25))));

            // The coordinator will try to further process the deposit to submit
            // the stacks tx, but we are not interested (for the current test iteration).
            client.expect_get_account().returning(|_| {
                let response = Ok(AccountInfo {
                    balance: 0,
                    locked: 0,
                    unlock_height: 0u64.into(),
                    // this is the only part used to create the stacks transaction.
                    nonce: 12,
                });
                Box::pin(std::future::ready(response))
            });
            client.expect_get_sortition_info().returning(move |_| {
                let response = Ok(SortitionInfo {
                    burn_block_hash: BurnchainHeaderHash::from(chain_tip),
                    burn_block_height: chain_tip_info.height,
                    burn_header_timestamp: 0,
                    sortition_id: SortitionId([0; 32]),
                    parent_sortition_id: SortitionId([0; 32]),
                    consensus_hash: ConsensusHash([0; 20]),
                    was_sortition: true,
                    miner_pk_hash160: None,
                    stacks_parent_ch: None,
                    last_sortition_ch: None,
                    committed_block_hash: None,
                });
                Box::pin(std::future::ready(response))
            });

            // The coordinator broadcasts a rotate keys transaction if it
            // is not up-to-date with their view of the current aggregate
            // key. The response of here means that the stacks node has a
            // record of a rotate keys contract call being executed, so the
            // coordinator should not broadcast one.
            client
                .expect_get_current_signer_set_info()
                .returning(move |_| {
                    Box::pin(std::future::ready(Ok(Some(signer_set_info.clone()))))
                });

            // No transactions should be submitted.
            client.expect_submit_tx().never();
        })
        .await;
    }

    // =========================================================================
    // Step 3 - Start the TxCoordinatorEventLoop, TxSignerEventLoop and
    //          BlockObserver processes for each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started, and
    //   we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        ctx.state().set_sbtc_contracts_deployed();
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: ctx.config().signer.bootstrap_signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            rng: rand::rngs::OsRng,
            last_presign_block: None,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        Sleep::for_millis(10).await;
    }

    // =========================================================================
    // Step 4 - Wait for DKG
    // -------------------------------------------------------------------------
    // - Once they are all running, generate a bitcoin block to kick off
    //   the database updating process.
    // - After they have the same view of the canonical bitcoin blockchain,
    //   the signers should all participate in DKG.
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Step 5 - Wait some more, maybe the signers will do something
    // -------------------------------------------------------------------------
    // - DKG has run, and they think the smart contracts are up-to-date, so
    //   they shouldn't do anything
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Step 6 - Assertions
    // -------------------------------------------------------------------------
    // - Make sure that no stacks contract transactions have been
    //   submitted.
    // - Couldn't hurt to check one more time that DKG has been run.
    // =========================================================================
    for (ctx, db, _, _) in signers {
        let dkg_shares = db.get_latest_encrypted_dkg_shares().await.unwrap();
        assert!(dkg_shares.is_some());

        ctx.with_stacks_client(|client| client.checkpoint()).await;
        testing::storage::drop_db(db).await;
    }
}

/// This test asserts that the `get_btc_state` function returns the correct
/// `SignerBtcState` when there are no sweep transactions available, i.e.
/// the `last_fees` field should be `None`.
#[test(tokio::test)]
async fn test_get_btc_state_with_no_available_sweep_transactions() {
    let mut rng = get_rng();

    let db = testing::storage::new_test_database().await;

    let context = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .build();
    let network = SignerNetwork::single(&context);

    context
        .with_bitcoin_client(|client| {
            client
                .expect_estimate_fee_rate()
                .times(1)
                .returning(|| Box::pin(async { Ok(1.3) }));
        })
        .await;

    let coord = TxCoordinatorEventLoop {
        context,
        private_key: PrivateKey::new(&mut rng),
        network: network.spawn(),
        threshold: 5,
        context_window: 5,
        signing_round_max_duration: std::time::Duration::from_secs(5),
        bitcoin_presign_request_max_duration: Duration::from_secs(5),
        dkg_max_duration: std::time::Duration::from_secs(5),
        is_epoch3: true,
    };

    let aggregate_key = &PublicKey::from_private_key(&PrivateKey::new(&mut rng));

    let dkg_shares = model::EncryptedDkgShares {
        aggregate_key: *aggregate_key,
        script_pubkey: aggregate_key.signers_script_pubkey().into(),
        dkg_shares_status: DkgSharesStatus::Unverified,
        ..Faker.fake_with_rng(&mut rng)
    };
    db.write_encrypted_dkg_shares(&dkg_shares).await.unwrap();

    // We create a single Bitcoin block which will be the chain tip and hold
    // our signer UTXO.
    let bitcoin_block = model::BitcoinBlock {
        block_height: 1u64.into(),
        block_hash: Faker.fake_with_rng(&mut rng),
        parent_hash: Faker.fake_with_rng(&mut rng),
    };

    // Create a Bitcoin transaction simulating holding a simulated signer
    // UTXO.
    let mut signer_utxo_tx = testing::dummy::tx(&Faker, &mut rng);
    signer_utxo_tx.output.insert(
        0,
        bitcoin::TxOut {
            value: bitcoin::Amount::from_btc(5.0).unwrap(),
            script_pubkey: aggregate_key.signers_script_pubkey(),
        },
    );
    let signer_utxo_txid = signer_utxo_tx.compute_txid();

    let utxo_input = model::TxPrevout {
        txid: signer_utxo_txid.into(),
        prevout_type: model::TxPrevoutType::SignersInput,
        ..Faker.fake_with_rng(&mut rng)
    };

    let utxo_output = model::TxOutput {
        txid: signer_utxo_txid.into(),
        output_type: model::TxOutputType::Donation,
        script_pubkey: aggregate_key.signers_script_pubkey().into(),
        ..Faker.fake_with_rng(&mut rng)
    };

    // Write the Bitcoin block and transaction to the database.
    db.write_bitcoin_block(&bitcoin_block).await.unwrap();
    db.write_bitcoin_transaction(&model::BitcoinTxRef {
        block_hash: bitcoin_block.block_hash,
        txid: signer_utxo_txid.into(),
    })
    .await
    .unwrap();
    db.write_tx_prevout(&utxo_input).await.unwrap();
    db.write_tx_output(&utxo_output).await.unwrap();

    // Get the chain tip and assert that it is the block we just wrote.
    let chain_tip = db
        .get_bitcoin_canonical_chain_tip()
        .await
        .unwrap()
        .expect("no chain tip");
    assert_eq!(chain_tip, bitcoin_block.block_hash);

    // Get the signer UTXO and assert that it is the one we just wrote.
    let utxo = db
        .get_signer_utxo(&chain_tip)
        .await
        .unwrap()
        .expect("no signer utxo");
    assert_eq!(utxo.outpoint.txid, signer_utxo_txid);

    // Grab the BTC state.
    let btc_state = coord
        .get_btc_state(&chain_tip, aggregate_key)
        .await
        .unwrap();

    // Assert that the BTC state is correct.
    assert_eq!(btc_state.utxo.outpoint.txid, signer_utxo_txid);
    assert_eq!(btc_state.utxo.public_key, aggregate_key.into());
    assert_eq!(btc_state.public_key, aggregate_key.into());
    assert_eq!(btc_state.fee_rate, 1.3);
    assert_eq!(btc_state.last_fees, None);
    assert_eq!(btc_state.magic_bytes, [b'T', b'3']);

    testing::storage::drop_db(db).await;
}

/// This test asserts that the `get_btc_state` function returns the correct
/// `SignerBtcState` when there are multiple outstanding sweep transaction
/// packages available, simulating the case where there has been an RBF.
#[test(tokio::test)]
async fn test_get_btc_state_with_available_sweep_transactions_and_rbf() {
    let mut rng = get_rng();

    let db = testing::storage::new_test_database().await;

    let client = BitcoinCoreClient::new(
        "http://localhost:18443",
        regtest::BITCOIN_CORE_RPC_USERNAME.to_string(),
        regtest::BITCOIN_CORE_RPC_PASSWORD.to_string(),
    )
    .unwrap();

    let context = TestContext::builder()
        .with_storage(db.clone())
        .with_bitcoin_client(client.clone())
        .with_mocked_emily_client()
        .with_mocked_stacks_client()
        .build();
    let network = SignerNetwork::single(&context);

    let coord = TxCoordinatorEventLoop {
        context,
        private_key: PrivateKey::new(&mut rng),
        network: network.spawn(),
        threshold: 5,
        context_window: 5,
        signing_round_max_duration: std::time::Duration::from_secs(5),
        bitcoin_presign_request_max_duration: Duration::from_secs(5),
        dkg_max_duration: std::time::Duration::from_secs(5),
        is_epoch3: true,
    };

    let aggregate_key = &PublicKey::from_private_key(&PrivateKey::new(&mut rng));

    let dkg_shares = model::EncryptedDkgShares {
        aggregate_key: *aggregate_key,
        script_pubkey: aggregate_key.signers_script_pubkey().into(),
        dkg_shares_status: DkgSharesStatus::Unverified,
        ..Faker.fake_with_rng(&mut rng)
    };
    db.write_encrypted_dkg_shares(&dkg_shares).await.unwrap();

    let (rpc, faucet) = regtest::initialize_blockchain();
    let addr = Recipient::new(AddressType::P2wpkh);

    // Get some coins to spend (and our "utxo" outpoint).
    let outpoint = faucet.send_to(10_000, &addr.address);
    let signer_utxo_block_hash = faucet.generate_block();

    let signer_utxo_tx = client.get_tx(&outpoint.txid).unwrap().unwrap();
    let signer_utxo_txid = signer_utxo_tx.tx.compute_txid();

    let utxo_input = model::TxPrevout {
        txid: signer_utxo_txid.into(),
        prevout_type: model::TxPrevoutType::SignersInput,
        ..Faker.fake_with_rng(&mut rng)
    };

    let utxo_output = model::TxOutput {
        txid: signer_utxo_txid.into(),
        output_index: 0,
        output_type: model::TxOutputType::Donation,
        script_pubkey: aggregate_key.signers_script_pubkey().into(),
        ..Faker.fake_with_rng(&mut rng)
    };

    db.write_bitcoin_block(&model::BitcoinBlock {
        block_height: 1u64.into(),
        block_hash: signer_utxo_block_hash.into(),
        parent_hash: BlockHash::all_zeros().into(),
    })
    .await
    .unwrap();

    db.write_tx_prevout(&utxo_input).await.unwrap();
    db.write_tx_output(&utxo_output).await.unwrap();

    db.write_bitcoin_transaction(&model::BitcoinTxRef {
        block_hash: signer_utxo_block_hash.into(),
        txid: signer_utxo_txid.into(),
    })
    .await
    .unwrap();

    let chain_tip = db.get_bitcoin_canonical_chain_tip().await.unwrap().unwrap();

    // Get the signer UTXO and assert that it is the one we just wrote.
    let utxo = db
        .get_signer_utxo(&chain_tip)
        .await
        .unwrap()
        .expect("no signer utxo");
    assert_eq!(utxo.outpoint.txid, signer_utxo_txid);

    // Get a utxo to spend.
    let utxo = addr.get_utxos(rpc, Some(10_000)).pop().unwrap();
    assert_eq!(utxo.txid, outpoint.txid);

    // Create a transaction that spends the utxo.
    let mut tx1 = bitcoin::Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: utxo.outpoint(),
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: bitcoin::Sequence::ZERO,
            witness: bitcoin::Witness::new(),
        }],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(9_000),
            script_pubkey: addr.address.script_pubkey(),
        }],
    };

    // Sign and broadcast the transaction
    p2wpkh_sign_transaction(&mut tx1, 0, &utxo, &addr.keypair);
    client.broadcast_transaction(&tx1).await.unwrap();

    // Grab the BTC state.
    let btc_state = coord
        .get_btc_state(&chain_tip, aggregate_key)
        .await
        .unwrap();

    let expected_fees = Fees {
        total: 1_000,
        rate: 1_000_f64 / tx1.vsize() as f64,
    };

    // Assert that everything's as expected.
    assert_eq!(btc_state.utxo.outpoint.txid, signer_utxo_txid);
    assert_eq!(btc_state.utxo.public_key, aggregate_key.into());
    assert_eq!(btc_state.public_key, aggregate_key.into());
    assert_eq!(btc_state.last_fees, Some(expected_fees));
    assert_eq!(btc_state.magic_bytes, [b'T', b'3']);

    // Create a 2nd transaction that spends the utxo (simulate RBF).
    let mut tx2 = bitcoin::Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: utxo.outpoint(),
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: bitcoin::Sequence::ZERO,
            witness: bitcoin::Witness::new(),
        }],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(8_000),
            script_pubkey: addr.address.script_pubkey(),
        }],
    };

    // Sign and broadcast the transaction
    p2wpkh_sign_transaction(&mut tx2, 0, &utxo, &addr.keypair);
    client.broadcast_transaction(&tx2).await.unwrap();

    // Grab the BTC state.
    let btc_state = coord
        .get_btc_state(&chain_tip, aggregate_key)
        .await
        .unwrap();

    let expected_fees = Fees {
        total: 2_000,
        rate: 2_000f64 / tx2.vsize() as f64,
    };

    // Assert that everything's as expected.
    assert_eq!(btc_state.utxo.outpoint.txid, signer_utxo_txid);
    assert_eq!(btc_state.last_fees, Some(expected_fees));

    testing::storage::drop_db(db).await;
}

fn create_signer_set(signers: &[Keypair], threshold: u32) -> (SignerSet, InMemoryNetwork) {
    let network = network::InMemoryNetwork::new();

    let signer_public_keys: BTreeSet<_> = signers.iter().map(|kp| kp.public_key().into()).collect();
    let signer_info: Vec<_> = signers
        .iter()
        .map(|kp| SignerInfo {
            signer_private_key: kp.secret_key().into(),
            signer_public_keys: signer_public_keys.clone(),
        })
        .collect();
    (
        SignerSet::new(&signer_info, threshold, || network.connect()),
        network,
    )
}

fn create_test_setup(
    dkg_shares: &EncryptedDkgShares,
    signatures_required: u16,
    faucet: &regtest::Faucet,
    rpc: &bitcoincore_rpc::Client,
    bitcoin_client: &BitcoinCoreClient,
) -> TestSweepSetup2 {
    let depositor = Recipient::new(AddressType::P2tr);
    faucet.send_to(50_000_000, &depositor.address);
    faucet.generate_block();

    let signer_address = Address::from_script(
        &dkg_shares.script_pubkey,
        bitcoin::Network::Regtest.params(),
    )
    .unwrap();
    let donation = faucet.send_to(100_000, &signer_address);
    let donation_block_hash = faucet.generate_block();

    let utxo = depositor.get_utxos(rpc, None).pop().unwrap();
    let (deposit_tx, deposit_request, deposit_info) = make_deposit_request(
        &depositor,
        5_000_000,
        utxo,
        100_000,
        dkg_shares.aggregate_key.x_only_public_key().0,
    );
    rpc.send_raw_transaction(&deposit_tx).unwrap();

    let deposit_block_hash = faucet.generate_block();
    let block_header = rpc.get_block_header_info(&deposit_block_hash).unwrap();
    let tx_info = bitcoin_client
        .get_tx_info(&deposit_tx.compute_txid(), &deposit_block_hash)
        .unwrap()
        .unwrap();
    let test_signers = TestSignerSet {
        keys: dkg_shares.signer_set_public_keys.clone(),
        // We don't use `signer`
        signer: Recipient::new(AddressType::P2tr),
    };
    let (request, recipient) = generate_withdrawal();
    let stacks_block = model::StacksBlock {
        block_hash: Faker.fake_with_rng(&mut OsRng),
        block_height: 0u64.into(),
        parent_hash: StacksBlockId::first_mined().into(),
        bitcoin_anchor: deposit_block_hash.into(),
    };
    TestSweepSetup2 {
        deposit_block_hash,
        deposits: vec![(deposit_info, deposit_request, tx_info)],
        sweep_tx_info: None,
        broadcast_info: None,
        donation,
        donation_block_hash,
        stacks_blocks: vec![stacks_block],
        signers: test_signers,
        withdrawals: vec![WithdrawalTriple {
            request,
            recipient,
            block_ref: block_header.as_block_ref(),
        }],
        withdrawal_sender: PrincipalData::from(StacksAddress::burn_address(false)),
        signatures_required,
    }
}

/// Test that we use conservative initial limits so that we don't process
/// requests until we can fetch limits from Emily.
///
/// Since the block observer doesn't signal the request decider (and tx coordinator)
/// if it fails to get the limits, the scenario required to check this issue is:
///  - The signers fetch a deposit from Emily, and it is validated and inserted
///    into the db
///  - Emily goes offline for some of the signers (at least one can still reach it)
///  - Those same signers are restarted, thus reloading the default sBTC limits
///  - The signer that didn't restart act as coordinator (since it can reach Emily
///    the block observer does signal the tx coordinator); as coordinator, it tries
///    to sweep the deposit above.
///  - The other signers cannot act as coordinator, but they can respond as tx
///    signers, and they do so using the default limits.
/// If the default limits are permissive (ie, default all `None`), they will
/// happily mint anything. If the limits are conservative, they will refuse to
/// mint (eg, `would exceed sBTC supply cap`).
#[tokio::test]
async fn test_conservative_initial_sbtc_limits() {
    let (rpc, faucet) = regtest::initialize_blockchain();
    let mut rng = get_rng();

    let (_, signer_key_pairs): (_, [Keypair; 3]) = testing::wallet::regtest_bootstrap_wallet();
    let signatures_required: u16 = 2;

    let network = WanNetwork::default();

    let chain_tip_info = get_canonical_chain_tip(rpc);

    // =========================================================================
    // Create a database, an associated context, and a Keypair for each of the
    // signers in the signing set.
    // -------------------------------------------------------------------------
    // - We load the database with a bitcoin blocks going back to some
    //   genesis block.
    // =========================================================================
    let signer_set_public_keys: BTreeSet<PublicKey> = signer_key_pairs
        .iter()
        .map(|kp| kp.public_key().into())
        .collect();
    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        backfill_bitcoin_blocks(&db, rpc, &chain_tip_info.hash).await;

        // Ensure a stacks tip exists before DKG
        let mut stacks_block: model::StacksBlock = Faker.fake_with_rng(&mut rng);
        stacks_block.bitcoin_anchor = chain_tip_info.hash.into();
        db.write_stacks_block(&stacks_block).await.unwrap();

        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_mocked_stacks_client()
            .with_mocked_emily_client()
            .modify_settings(|settings| {
                settings.signer.bootstrap_signing_set = signer_set_public_keys.clone();
                settings.signer.bootstrap_signatures_required = signatures_required;
            })
            .build();

        // We do not want to run DKG because we think that the signer set
        // has changed.
        let aggregate_key = Faker.fake_with_rng(&mut rng);
        prevent_dkg_on_changed_signer_set_info(&ctx, aggregate_key);

        let network = network.connect(&ctx);
        signers.push((ctx, db, kp, network));
    }

    // =========================================================================
    // Compute DKG and store it into db
    // =========================================================================
    let mut signer_set = create_signer_set(&signer_key_pairs, signatures_required as u32).0;
    let dkg_txid = testing::dummy::txid(&fake::Faker, &mut rng).into();
    let chain_tip = chain_tip_info.hash.into();

    let (aggregate_key, mut encrypted_shares) = signer_set
        .run_dkg(chain_tip, dkg_txid, DkgSharesStatus::Verified)
        .await;

    for ((_, db, _, _), dkg_shares) in signers.iter_mut().zip(encrypted_shares.iter_mut()) {
        dkg_shares.dkg_shares_status = DkgSharesStatus::Verified;
        dkg_shares.signature_share_threshold = signatures_required;
        signer_set
            .write_as_rotate_keys_tx(db, &chain_tip, dkg_shares, &mut rng)
            .await;

        db.write_encrypted_dkg_shares(dkg_shares)
            .await
            .expect("failed to write encrypted shares");
    }

    // =========================================================================
    // Setup the emily client mocks.
    // =========================================================================
    let enable_emily_limits = Arc::new(AtomicBool::new(false));
    for (i, (ctx, _, _, _)) in signers.iter_mut().enumerate() {
        ctx.with_emily_client(|client| {
            // We already stored the deposit, we don't need it from Emily
            client
                .expect_get_deposits()
                .returning(|| Box::pin(std::future::ready(Ok(vec![]))));

            // We don't care about this
            client.expect_accept_deposits().returning(|_| {
                Box::pin(std::future::ready(Err(Error::InvalidStacksResponse(
                    "dummy",
                ))))
            });

            // We don't care about this
            client.expect_accept_withdrawals().returning(|_| {
                Box::pin(std::future::ready(Err(Error::InvalidStacksResponse(
                    "dummy",
                ))))
            });

            let enable_emily_limits = enable_emily_limits.clone();
            client.expect_get_limits().times(1..).returning(move || {
                // Since we don't signal the coordinator if we fail to fetch the limits
                // we need the coordinator to be able to fetch them.
                // But we want the other signers to fail fetching limits.
                let limits = if i == 0 || enable_emily_limits.load(Ordering::SeqCst) {
                    Ok(SbtcLimits::unlimited())
                } else {
                    // Just a random error, we don't care about it
                    Err(Error::InvalidStacksResponse("dummy"))
                };
                Box::pin(std::future::ready(limits))
            });
        })
        .await;
    }

    // =========================================================================
    // Setup the stacks client mocks.
    // -------------------------------------------------------------------------
    // - Set up the mocks to that the block observer fetches at least one
    //   Stacks block. This is necessary because we need the stacks chain
    //   tip in the transaction coordinator.
    // =========================================================================
    for (ctx, _, _, _) in signers.iter_mut() {
        let signer_set = signer_set_public_keys.clone();
        ctx.with_stacks_client(|client| {
            client
                .expect_get_tenure_info()
                .returning(move || Box::pin(std::future::ready(Ok(DUMMY_TENURE_INFO.clone()))));

            client.expect_get_block().returning(|_| {
                let response = Ok(NakamotoBlock {
                    header: NakamotoBlockHeader::empty(),
                    txs: vec![],
                });
                Box::pin(std::future::ready(response))
            });

            let chain_tip = model::BitcoinBlockHash::from(chain_tip_info.hash);
            client.expect_get_tenure().returning(move |_| {
                let mut tenure = TenureBlocks::nearly_empty().unwrap();
                tenure.anchor_block_hash = chain_tip;
                Box::pin(std::future::ready(Ok(tenure)))
            });

            client.expect_get_pox_info().returning(|| {
                let response = serde_json::from_str::<RPCPoxInfoData>(GET_POX_INFO_JSON)
                    .map_err(Error::JsonSerialize);
                Box::pin(std::future::ready(response))
            });

            client
                .expect_estimate_fees()
                .returning(|_, _, _| Box::pin(std::future::ready(Ok(25))));

            // The coordinator will try to further process the deposit to submit
            // the stacks tx, but we are not interested (for the current test iteration).
            client.expect_get_account().returning(|_| {
                let response = Ok(AccountInfo {
                    balance: 0,
                    locked: 0,
                    unlock_height: 0u64.into(),
                    // this is the only part used to create the stacks transaction.
                    nonce: 12,
                });
                Box::pin(std::future::ready(response))
            });

            client
                .expect_get_sortition_info()
                .returning(move |_| Box::pin(std::future::ready(Ok(DUMMY_SORTITION_INFO))));

            // The coordinator broadcasts a rotate keys transaction if it
            // is not up-to-date with their view of the current aggregate
            // key. The response of None means that the stacks node does
            // not have a record of a rotate keys contract call being
            // executed, so the coordinator will construct and broadcast
            // one.
            client
                .expect_get_current_signer_set_info()
                .returning(move |_| {
                    Box::pin(std::future::ready(Ok(Some(SignerSetInfo {
                        aggregate_key,
                        signer_set: signer_set.clone(),
                        signatures_required,
                    }))))
                });

            // The coordinator will get the total supply of sBTC to
            // determine the amount of mintable sBTC.
            client
                .expect_get_sbtc_total_supply()
                .returning(move |_| Box::pin(async move { Ok(Amount::ZERO) }));
        })
        .await;
    }

    // =========================================================================
    // Setup a deposit
    // -------------------------------------------------------------------------
    // - Write the deposit (and anything required for it to be swept)
    // =========================================================================
    let dkg_shares = encrypted_shares.first().cloned().unwrap();
    let bitcoin_client = signers[0].0.clone().bitcoin_client;
    let setup = create_test_setup(
        &dkg_shares,
        signatures_required,
        faucet,
        rpc,
        &bitcoin_client,
    );
    for (_, db, _, _) in signers.iter_mut() {
        backfill_bitcoin_blocks(db, rpc, &setup.deposit_block_hash).await;
        setup.store_stacks_genesis_block(db).await;
        setup.store_donation(db).await;
        setup.store_deposit_txs(db).await;
        setup.store_deposit_request(db).await;
        setup.store_deposit_decisions(db).await;
    }

    // =========================================================================
    // Start the TxCoordinatorEventLoop, TxSignerEventLoop and BlockObserver
    // processes for each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started, and
    //   we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        ctx.state().set_sbtc_contracts_deployed();
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(2),
            bitcoin_presign_request_max_duration: Duration::from_secs(2),
            threshold: signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            rng: rand::rngs::OsRng,
            last_presign_block: None,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        Sleep::for_millis(10).await;
    }

    // =========================================================================
    // Wait for the first signer to be the coordinator
    // -------------------------------------------------------------------------
    // - Two of three signers will not be able to coordinate, because of failing
    //   in getting deposits (so no signal is sent to request decider and tx
    //   coordinator)
    // =========================================================================
    let signers_key = setup.signers.signer_keys().iter().cloned().collect();
    loop {
        let chain_tip: BitcoinBlockHash = faucet.generate_blocks(1).pop().unwrap().into();
        if given_key_is_coordinator(signers[0].2.public_key().into(), &chain_tip, &signers_key) {
            break;
        }
    }
    // Giving enough time to process the transaction
    Sleep::for_secs(3).await;

    // =========================================================================
    // Check we did NOT process the deposit
    // =========================================================================
    let (ctx, _, _, _) = signers.first().unwrap();
    let txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();

    assert!(txids.is_empty());

    // =========================================================================
    // Re-enable limits fetching
    // =========================================================================
    enable_emily_limits.store(true, Ordering::SeqCst);

    faucet.generate_block();
    Sleep::for_secs(3).await;
    // =========================================================================
    // Check we did process the deposit now
    // =========================================================================
    let (ctx, _, _, _) = signers.first().unwrap();
    let txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();

    assert_eq!(txids.len(), 1);
    let tx_info = bitcoin_client.get_tx(&txids[0]).unwrap().unwrap();

    assert_eq!(
        tx_info.tx.input[1].previous_output,
        setup.deposit_outpoints()[0]
    );

    for (_, db, _, _) in signers {
        testing::storage::drop_db(db).await;
    }
}

/// Test that three signers can successfully sign and broadcast a bitcoin
/// transaction sweeping out funds given a withdrawal request.
///
/// The test setup is as follows:
/// 1. There are three "signers" contexts. Each context points to its own
///    real postgres database, and they have their own private key. Each
///    database is populated with the same blockchain data.
/// 2. Each context is given to a block observer, a tx signer, and a tx
///    coordinator, where these event loops are spawned as separate tasks.
/// 3. The signers communicate with our in-memory network struct.
/// 4. A real Emily server is running in the background.
/// 5. A real bitcoin-core node is running in the background.
/// 6. Stacks-core is mocked.
///
/// After the setup, the signers observe a bitcoin block and update their
/// databases. The coordinator then constructs a bitcoin transaction and
/// gets it signed. After it is signed the coordinator broadcasts it to
/// bitcoin-core. We also check for the accept-withdrawal-request contract
/// call.
///
/// To start the test environment do:
/// ```bash
/// make integration-env-up-ci
/// ```
///
/// then, once everything is up and running, run the test.
#[tokio::test]
async fn sign_bitcoin_transaction_withdrawals() {
    let (_, signer_key_pairs): (_, [Keypair; 3]) = testing::wallet::regtest_bootstrap_wallet();
    let (rpc, faucet) = regtest::initialize_blockchain();

    let mut rng = get_rng();
    // We need to populate our databases, so let's fetch the data.
    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    let emily_config = emily_client.config().as_testing();

    testing_api::wipe_databases(&emily_config).await.unwrap();

    let network = WanNetwork::default();

    let chain_tip_info = get_canonical_chain_tip(rpc);

    // =========================================================================
    // Step 1 - Create a database, an associated context, and a Keypair for
    //          each of the signers in the signing set.
    // -------------------------------------------------------------------------
    // - We load the database with a bitcoin blocks going back to some
    //   genesis block.
    // =========================================================================
    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_emily_client(emily_client.clone())
            .with_mocked_stacks_client()
            .build();

        backfill_bitcoin_blocks(&db, rpc, &chain_tip_info.hash).await;

        let network = network.connect(&ctx);

        signers.push((ctx, db, kp, network));
    }

    // =========================================================================
    // Step 2 - Setup the stacks client mocks.
    // -------------------------------------------------------------------------
    // - Set up the mocks to that the block observer fetches at least one
    //   Stacks block. This is necessary because we need the stacks chain
    //   tip in the transaction coordinator.
    // - Set up the current-aggregate-key response to be `None`. This means
    //   that each coordinator will broadcast a rotate keys transaction.
    // =========================================================================
    let (broadcast_stacks_tx, rx) = tokio::sync::broadcast::channel(10);
    let stacks_tx_stream = BroadcastStream::new(rx);

    for (ctx, db, _, _) in signers.iter_mut() {
        let broadcast_stacks_tx = broadcast_stacks_tx.clone();
        let db = db.clone();

        mock_stacks_core(ctx, chain_tip_info.clone(), db, broadcast_stacks_tx).await;
    }

    // =========================================================================
    // Step 3 - Start the TxCoordinatorEventLoop, TxSignerEventLoop and
    //          BlockObserver processes for each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started, and
    //   we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        ctx.state().set_sbtc_contracts_deployed();
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: ctx.config().signer.bootstrap_signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            last_presign_block: None,
            rng: rand::rngs::OsRng,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        Sleep::for_millis(10).await;
    }

    // =========================================================================
    // Step 4 - Wait for DKG
    // -------------------------------------------------------------------------
    // - Once they are all running, generate a bitcoin block to kick off
    //   the database updating process.
    // - After they have the same view of the canonical bitcoin blockchain,
    //   the signers should all participate in DKG.
    // =========================================================================
    let chain_tip = faucet.generate_block().into();

    // We first need to wait for bitcoin-core to send us all the
    // notifications so that we are up-to-date with the chain tip.
    wait_for_signers(&signers).await;

    // DKG and DKG verification should have finished successfully. We
    // assume, for now, that the key rotation contract call was submitted.
    // This assumption gets validated later, but we make the assumption now
    // and populate the database with a key rotation event.
    for (ctx, db, _, _) in signers.iter() {
        let shares = db.get_latest_verified_dkg_shares().await.unwrap().unwrap();

        let stacks_chain_tip = db.get_stacks_chain_tip(&chain_tip).await.unwrap().unwrap();
        let event = KeyRotationEvent {
            txid: fake::Faker.fake_with_rng(&mut rng),
            block_hash: stacks_chain_tip.block_hash,
            aggregate_key: shares.aggregate_key,
            signer_set: shares.signer_set_public_keys.clone(),
            signatures_required: shares.signature_share_threshold,
            address: PrincipalData::from(ctx.config().signer.deployer).into(),
        };
        db.write_rotate_keys_transaction(&event).await.unwrap();
    }

    let (_, db, _, _) = signers.first().unwrap();
    let shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();

    // =========================================================================
    // Step 5 - Prepare for the withdrawal
    // -------------------------------------------------------------------------
    // - Before the signers can process anything, they need a UTXO to call
    //   their own. For that we make a donation, and confirm it. The
    //   signers should pick it up.
    // =========================================================================
    let script_pub_key = shares.aggregate_key.signers_script_pubkey();
    let network = bitcoin::Network::Regtest;
    let address = Address::from_script(&script_pub_key, network).unwrap();

    faucet.send_to(100_000_000, &address);

    // =========================================================================
    // Step 6 - Receive the withdrawal request
    // -------------------------------------------------------------------------
    // - Withdrawal requests are received from our stacks node, where we
    //   are an event observer of our node. So creating a withdrawal
    //   request is basically writing a row to our database.
    // =========================================================================
    let withdrawal_recipient = Recipient::new(AddressType::P2tr);

    // Let's initiate a withdrawal. This is easy, we just create a row in
    // the databases.
    let (bitcoin_chain_tip, stacks_chain_tip) = db.get_chain_tips().await;

    let withdrawal_request = WithdrawalRequest {
        request_id: 23,
        bitcoin_block_height: bitcoin_chain_tip.block_height,
        amount: 10_000_000,
        block_hash: stacks_chain_tip,
        recipient: withdrawal_recipient.script_pubkey.clone().into(),
        max_fee: 100_000,
        txid: StacksTxId::from([123; 32]),
        sender_address: PrincipalData::from(StandardPrincipalData::transient()).into(),
    };
    // Now we should manually put withdrawal request to Emily, pretending that
    // sidecar did it.
    let stacks_tip_height = db
        .get_stacks_block(&stacks_chain_tip)
        .await
        .unwrap()
        .unwrap()
        .block_height;

    // Set the chainstate to Emily before we create the withdrawal request
    chainstate_api::set_chainstate(
        &emily_config,
        Chainstate {
            stacks_block_hash: stacks_chain_tip.to_string(),
            stacks_block_height: *stacks_tip_height,
            bitcoin_block_height: Some(Some(0)), // TODO: maybe we will want to have here some sensible data.
        },
    )
    .await
    .expect("Failed to set chainstate");

    let request_body = testing_emily_client::models::CreateWithdrawalRequestBody {
        amount: withdrawal_request.amount,
        parameters: Box::new(testing_emily_client::models::WithdrawalParameters {
            max_fee: withdrawal_request.max_fee,
        }),
        recipient: withdrawal_request.recipient.to_string(),
        request_id: withdrawal_request.request_id,
        sender: withdrawal_request.sender_address.to_string(),
        stacks_block_hash: withdrawal_request.block_hash.to_string(),
        stacks_block_height: *stacks_tip_height,
        txid: withdrawal_request.txid.to_string(),
    };
    let response = withdrawal_api::create_withdrawal(&emily_config, request_body).await;
    assert!(response.is_ok());
    // Check that there is no Accepted requests on emily before we broadcast them
    let withdrawals_on_emily = withdrawal_api::get_withdrawals(
        &emily_config,
        TestingEmilyWithdrawalStatus::Accepted,
        None,
        None,
    )
    .await
    .unwrap()
    .withdrawals;
    assert!(withdrawals_on_emily.is_empty());

    // Check that there is no Accepted requests on emily before we broadcast them
    let withdrawals_on_emily = withdrawal_api::get_withdrawals(
        &emily_config,
        TestingEmilyWithdrawalStatus::Pending,
        None,
        None,
    )
    .await
    .unwrap()
    .withdrawals;
    assert_eq!(withdrawals_on_emily.len(), 1);

    for (_, db, _, _) in signers.iter() {
        db.write_withdrawal_request(&withdrawal_request)
            .await
            .unwrap();
    }

    // =========================================================================
    // Step 7 - Confirm WITHDRAWAL_MIN_CONFIRMATIONS more blocks so that
    //          the signers process the withdrawal request.
    // -------------------------------------------------------------------------
    // - Generate WITHDRAWAL_MIN_CONFIRMATIONS blocks the deposit request.
    //   This will trigger the block observer to update the database.
    // - Each RequestDecider process should vote on the withdrawal request
    //   and submit the votes to each other.
    // - The coordinator should submit a sweep transaction with an output
    //   fulfilling the withdrawal request.
    // =========================================================================
    let (ctx, _, _, _) = signers.first().unwrap();
    for _ in 0..WITHDRAWAL_MIN_CONFIRMATIONS - 1 {
        faucet.generate_block();
        wait_for_signers(&signers).await;
        // The mempool should be empty, since the signers do not act on the
        // withdrawal unless they've observed WITHDRAWAL_MIN_CONFIRMATIONS
        // from the chain tip of when the withdrawal request was created.
        let txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();
        assert!(txids.is_empty());
    }

    faucet.generate_block();
    wait_for_signers(&signers).await;

    let mut txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();
    assert_eq!(txids.len(), 1);

    let block_hash = faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Step 8 - Assertions
    // -------------------------------------------------------------------------
    // - The first transactions should be a rotate keys contract call. And
    //   because of how we set up our mocked stacks client, each
    //   coordinator submits a rotate keys transaction before they do
    //   anything else.
    // - The last transaction should be us accepting the withdrawal request
    //   and burning sBTC.
    // - Is the sweep transaction in our database.
    // - Does the sweep outputs to the right scriptPubKey with the right
    //   amount.
    // =========================================================================

    let withdrawals_on_emily = withdrawal_api::get_withdrawals(
        &emily_config,
        TestingEmilyWithdrawalStatus::Accepted,
        None,
        None,
    )
    .await
    .unwrap()
    .withdrawals;

    assert_eq!(withdrawals_on_emily.len(), 1);

    let withdrawal_on_emily = withdrawals_on_emily[0].clone();
    assert_eq!(
        withdrawal_on_emily.request_id,
        withdrawal_request.request_id
    );
    assert_eq!(
        withdrawal_on_emily.stacks_block_hash,
        withdrawal_request.block_hash.to_string()
    );
    assert_eq!(
        withdrawal_on_emily.recipient,
        withdrawal_request.recipient.to_string()
    );
    assert_eq!(
        withdrawal_on_emily.sender,
        withdrawal_request.sender_address.to_string()
    );
    assert_eq!(withdrawal_on_emily.amount, withdrawal_request.amount);

    let sleep_fut = Sleep::for_secs(5);
    let broadcast_stacks_txs: Vec<StacksTransaction> = stacks_tx_stream
        .take_until(sleep_fut)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    more_asserts::assert_ge!(broadcast_stacks_txs.len(), 2);
    // Check that the first N - 1 are all rotate keys contract calls.
    let rotate_keys_count = broadcast_stacks_txs.len() - 1;
    for tx in broadcast_stacks_txs.iter().take(rotate_keys_count) {
        assert_stacks_transaction_kind::<RotateKeysV1>(tx);
    }
    // Check that the Nth transaction is the accept-withdrawal-request
    // contract call.
    let tx = broadcast_stacks_txs.last().unwrap();
    assert_stacks_transaction_kind::<AcceptWithdrawalV1>(tx);

    // Now lets check the bitcoin transaction, first we get it.
    let txid = txids.pop().unwrap();
    let tx_info = ctx
        .bitcoin_client
        .get_tx_info(&txid, &block_hash)
        .unwrap()
        .unwrap();
    // We check that the scriptPubKey of the first input is the signers'
    let actual_script_pub_key = tx_info.prevout(0).unwrap().script_pubkey.as_bytes();

    assert_eq!(actual_script_pub_key, script_pub_key.as_bytes());
    assert_eq!(&tx_info.tx.output[0].script_pubkey, &script_pub_key);

    let recipient = &*withdrawal_request.recipient;
    assert_eq!(&tx_info.tx.output[2].script_pubkey, recipient);

    let withdrawal_amount = withdrawal_request.amount;
    assert_eq!(tx_info.tx.output[2].value.to_sat(), withdrawal_amount);

    // We check that our database has the sweep transaction
    let script_pubkey = sqlx::query_scalar::<_, model::ScriptPubKey>(
        r#"
        SELECT script_pubkey
        FROM sbtc_signer.bitcoin_tx_outputs
        WHERE txid = $1
          AND output_type = 'signers_output'
        "#,
    )
    .bind(txid.to_byte_array())
    .fetch_one(ctx.storage.pool())
    .await
    .unwrap();

    // We check that that our database has the withdrawal tx output
    let withdrawal_output = sqlx::query_as::<_, WithdrawalTxOutput>(
        r#"
        SELECT txid, output_index, request_id
        FROM sbtc_signer.bitcoin_withdrawal_tx_outputs
        WHERE txid = $1
        "#,
    )
    .bind(txid.to_byte_array())
    .fetch_one(ctx.storage.pool())
    .await
    .unwrap();

    assert_eq!(withdrawal_output.output_index, 2);
    assert_eq!(withdrawal_output.request_id, withdrawal_request.request_id);

    for (_, db, _, _) in signers {
        assert!(db.is_signer_script_pub_key(&script_pubkey).await.unwrap());
        testing::storage::drop_db(db).await;
    }
}

#[test_case(false, false; "rejectable")]
#[test_case(true, false; "completed")]
#[test_case(false, true; "in mempool")]
#[tokio::test]
async fn process_rejected_withdrawal(is_completed: bool, is_in_mempool: bool) {
    let db = testing::storage::new_test_database().await;
    let mut rng = get_rng();
    let (rpc, faucet) = regtest::initialize_blockchain();

    let mut context = TestContext::builder()
        .with_storage(db.clone())
        .with_first_bitcoin_core_client()
        .with_mocked_stacks_client()
        .with_mocked_emily_client()
        .build();

    let expect_tx = !is_completed && !is_in_mempool;

    let nonce = 12;
    let db2 = db.clone();
    // Mock required stacks client functions
    context
        .with_stacks_client(|client| {
            client.expect_get_account().once().returning(move |_| {
                Box::pin(async move {
                    Ok(AccountInfo {
                        balance: 0,
                        locked: 0,
                        unlock_height: 0u64.into(),
                        // The nonce is used to create the stacks tx
                        nonce,
                    })
                })
            });

            // The coordinator broadcasts a rotate keys transaction if it
            // is not up-to-date with their view of the current aggregate
            // key. The response of here means that the stacks node has a
            // record of a rotate keys contract call being executed once we
            // have verified shares.
            client
                .expect_get_current_signer_set_info()
                .returning(move |_| {
                    let db2 = db2.clone();
                    Box::pin(async move {
                        let shares = db2.get_latest_verified_dkg_shares().await?;
                        Ok(shares.map(SignerSetInfo::from))
                    })
                });

            // Dummy value
            client
                .expect_estimate_fees()
                .returning(move |_, _, _| Box::pin(async move { Ok(25505) }));

            client
                .expect_is_withdrawal_completed()
                .returning(move |_, _| Box::pin(async move { Ok(is_completed) }));
        })
        .await;

    let num_signers = 7;
    let signing_threshold = 5;
    let context_window = 100;

    let network = network::in_memory::InMemoryNetwork::new();
    let signer_info = testing::wsts::generate_signer_info(&mut rng, num_signers);

    let mut testing_signer_set =
        testing::wsts::SignerSet::new(&signer_info, signing_threshold, || network.connect());

    let bitcoin_chain_tip = rpc.get_blockchain_info().unwrap().best_block_hash;
    backfill_bitcoin_blocks(&db, rpc, &bitcoin_chain_tip).await;

    // Ensure we have a stacks chain tip
    let genesis_block = model::StacksBlock {
        block_hash: Faker.fake_with_rng(&mut OsRng),
        block_height: 0u64.into(),
        parent_hash: StacksBlockId::first_mined().into(),
        bitcoin_anchor: bitcoin_chain_tip.into(),
    };
    db.write_stacks_blocks([&genesis_block]).await;

    let (aggregate_key, _) = run_dkg(&context, &mut rng, &mut testing_signer_set).await;

    // We need to set the signer's UTXO since that is necessary to know if
    // there is a transaction sweeping out any withdrawals.
    let script_pub_key = aggregate_key.signers_script_pubkey();
    let bitcoin_network = bitcoin::Network::Regtest;
    let address = Address::from_script(&script_pub_key, bitcoin_network).unwrap();
    let donation = faucet.send_to(100_000, &address);

    // Okay, the donation exists, but it's in the mempool. In order to get
    // it into our database it needs to be confirmed. We also feed it
    // through the block observer will consider since it handles all the
    // logic for writing it to the database.
    let bitcoin_chain_tip = faucet.generate_block();
    backfill_bitcoin_blocks(&db, rpc, &bitcoin_chain_tip).await;

    let tx = context
        .bitcoin_client
        .get_tx_info(&donation.txid, &bitcoin_chain_tip)
        .unwrap()
        .unwrap();
    let bootstrap_script_pubkey = context.config().signer.bootstrap_aggregate_key;
    block_observer::extract_sbtc_transactions(
        &db,
        bootstrap_script_pubkey,
        bitcoin_chain_tip,
        &[tx],
    )
    .await
    .unwrap();

    let (bitcoin_chain_tip, stacks_chain_tip) = db.get_chain_tips().await;
    assert_eq!(stacks_chain_tip, genesis_block.block_hash);

    // Now we create a withdrawal request (without voting for it)
    let request = WithdrawalRequest {
        block_hash: genesis_block.block_hash,
        bitcoin_block_height: bitcoin_chain_tip.block_height,
        ..fake::Faker.fake_with_rng(&mut rng)
    };
    db.write_withdrawal_request(&request).await.unwrap();

    // The request should not be pending yet (missing enough confirmation)
    assert!(
        context
            .get_storage()
            .get_pending_rejected_withdrawal_requests(&bitcoin_chain_tip, context_window)
            .await
            .unwrap()
            .is_empty()
    );

    let new_tip = faucet
        .generate_blocks(WITHDRAWAL_BLOCKS_EXPIRY + 1)
        .pop()
        .unwrap();
    backfill_bitcoin_blocks(&db, rpc, &new_tip).await;
    let (bitcoin_chain_tip, _) = db.get_chain_tips().await;

    // We've just updated the database with a new chain tip, so we need to
    // update the signer's state just like the block observer would.
    context.state().set_bitcoin_chain_tip(bitcoin_chain_tip);
    // Now it should be pending rejected
    assert_eq!(
        context
            .get_storage()
            .get_pending_rejected_withdrawal_requests(&bitcoin_chain_tip, context_window)
            .await
            .unwrap()
            .single(),
        request
    );

    if is_in_mempool {
        // If we are testing the mempool/submitted scenario, we need to fake it
        let outpoint = faucet.send_to(1000, &faucet.address);
        let bitcoin_txid = outpoint.txid.into();
        let withdrawal_output = model::BitcoinWithdrawalOutput {
            bitcoin_txid,
            bitcoin_chain_tip: bitcoin_chain_tip.block_hash,
            output_index: outpoint.vout,
            request_id: request.request_id,
            stacks_txid: request.txid,
            stacks_block_hash: request.block_hash,
            // We don't care about validation, as the majority of signers may
            // have validated it, so we err towards checking more rather than
            // less txids.
            validation_result: WithdrawalValidationResult::NoVote,
            is_valid_tx: false,
        };
        db.write_bitcoin_withdrawals_outputs(&[withdrawal_output])
            .await
            .unwrap();

        let sighash = BitcoinTxSigHash {
            txid: bitcoin_txid,
            prevout_type: model::TxPrevoutType::SignersInput,
            prevout_txid: donation.txid.into(),
            prevout_output_index: donation.vout,
            validation_result: signer::bitcoin::validation::InputValidationResult::Ok,
            aggregate_key: aggregate_key.into(),
            is_valid_tx: false,
            will_sign: false,
            chain_tip: bitcoin_chain_tip.block_hash,
            sighash: bitcoin::TapSighash::from_byte_array([1; 32]).into(),
        };
        db.write_bitcoin_txs_sighashes(&[sighash]).await.unwrap();
    }

    let (broadcasted_transaction_tx, _broadcasted_transaction_rx) =
        tokio::sync::broadcast::channel(1);

    // This task gets all transactions broadcasted by the coordinator.
    let mut wait_for_transaction_rx = broadcasted_transaction_tx.subscribe();
    let wait_for_transaction_task =
        tokio::spawn(async move { wait_for_transaction_rx.recv().await });

    let signer_set: BTreeSet<_> = testing_signer_set.signer_keys().into_iter().collect();
    // Setup the stacks client mock to broadcast the transaction to our channel.
    context
        .with_stacks_client(|client| {
            client
                .expect_submit_tx()
                .times(if expect_tx { 1 } else { 0 })
                .returning(move |tx| {
                    let tx = tx.clone();
                    let txid = tx.txid();
                    let broadcasted_transaction_tx = broadcasted_transaction_tx.clone();
                    Box::pin(async move {
                        broadcasted_transaction_tx
                            .send(tx)
                            .expect("Failed to send result");
                        Ok(SubmitTxResponse::Acceptance(txid))
                    })
                });

            client
                .expect_get_current_signer_set_info()
                .returning(move |_| {
                    Box::pin(std::future::ready(Ok(Some(SignerSetInfo {
                        aggregate_key,
                        signer_set: signer_set.clone(),
                        signatures_required: signing_threshold as u16,
                    }))))
                });
        })
        .await;

    // Get the private key of the coordinator of the signer set.
    let private_key = select_coordinator(&bitcoin_chain_tip.block_hash, &signer_info);

    let config = context.config_mut();
    config.signer.private_key = private_key;
    config.signer.bootstrap_signatures_required = signing_threshold as u16;
    config.signer.bootstrap_signing_set = signer_info.first().unwrap().signer_public_keys.clone();

    prevent_dkg_on_changed_signer_set_info(&context, aggregate_key);

    // Bootstrap the tx coordinator event loop
    context.state().set_sbtc_contracts_deployed();
    let tx_coordinator = transaction_coordinator::TxCoordinatorEventLoop {
        context: context.clone(),
        network: network.connect(),
        private_key,
        context_window,
        threshold: signing_threshold as u16,
        signing_round_max_duration: Duration::from_secs(5),
        bitcoin_presign_request_max_duration: Duration::from_secs(5),
        dkg_max_duration: Duration::from_secs(5),
        is_epoch3: true,
    };
    let tx_coordinator_handle = tokio::spawn(async move { tx_coordinator.run().await });

    // Here signers use all the same storage, but we don't care in this test
    let _event_loop_handles: Vec<_> = signer_info
        .clone()
        .into_iter()
        .map(|signer_info| {
            let event_loop_harness = TxSignerEventLoopHarness::create(
                context.clone(),
                network.connect(),
                context_window,
                signer_info.signer_private_key,
                signing_threshold,
                rng.clone(),
            );

            event_loop_harness.start()
        })
        .collect();

    // Yield to get signers ready
    Sleep::for_millis(100).await;

    // Wake coordinator up
    context
        .signal(RequestDeciderEvent::NewRequestsHandled.into())
        .expect("failed to signal");

    // Await for tenure completion
    let tenure_completed_signal = TxCoordinatorEvent::TenureCompleted.into();
    context
        .wait_for_signal(Duration::from_secs(5), |signal| {
            signal == &tenure_completed_signal
        })
        .await
        .unwrap();

    // Await the `wait_for_tx_task` to receive the first transaction broadcasted.
    let broadcasted_tx = wait_for_transaction_task
        .with_timeout(Duration::from_secs(1))
        .await;

    // Stop event loops
    tx_coordinator_handle.abort();

    if !expect_tx {
        assert!(broadcasted_tx.is_err());
        testing::storage::drop_db(db).await;
        return;
    }

    let broadcasted_tx = broadcasted_tx
        .unwrap()
        .expect("failed to receive message")
        .expect("no message received");

    broadcasted_tx.verify().unwrap();

    assert_eq!(broadcasted_tx.get_origin_nonce(), nonce);

    let TransactionPayload::ContractCall(contract_call) = broadcasted_tx.payload else {
        panic!("unexpected tx payload")
    };
    assert_eq!(
        contract_call.contract_name.to_string(),
        RejectWithdrawalV1::CONTRACT_NAME
    );
    assert_eq!(
        contract_call.function_name.to_string(),
        RejectWithdrawalV1::FUNCTION_NAME
    );
    assert_eq!(
        contract_call.function_args[0],
        ClarityValue::UInt(request.request_id as u128)
    );

    testing::storage::drop_db(db).await;
}

/// Test that the coordinator doesn't try to sign a complete deposit stacks tx
/// for a swept deposit if the smart contract consider the deposit confirmed.
#[test_case(true; "deposit completed")]
#[test_case(false; "deposit not completed")]
#[tokio::test]
async fn coordinator_skip_onchain_completed_deposits(deposit_completed: bool) {
    let (rpc, faucet) = regtest::initialize_blockchain();

    let db = testing::storage::new_test_database().await;

    let signer = Recipient::new(AddressType::P2tr);
    let mut ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .modify_settings(|settings| {
            let public_key = signer.keypair.public_key().into();
            settings.signer.bootstrap_signing_set = [public_key].into_iter().collect();
            settings.signer.bootstrap_signatures_required = 1;
        })
        .build();
    let network = WanNetwork::default();
    let signer_network = network.connect(&ctx);

    let signer_kp = signer.keypair;
    let signers = TestSignerSet {
        signer,
        keys: vec![signer_kp.public_key().into()],
    };
    let aggregate_key = signers.aggregate_key();

    ctx.state().set_sbtc_contracts_deployed();

    let signer_set: BTreeSet<_> = signers.signer_keys().iter().copied().collect();
    ctx.with_stacks_client(|client| {
        client
            .expect_estimate_fees()
            .returning(|_, _, _| Box::pin(std::future::ready(Ok(25))));

        client.expect_get_account().returning(|_| {
            let response = Ok(AccountInfo {
                balance: 0,
                locked: 0,
                unlock_height: 0u64.into(),
                // this is the only part used to create the stacks transaction.
                nonce: 12,
            });
            Box::pin(std::future::ready(response))
        });

        client
            .expect_get_current_signer_set_info()
            .returning(move |_| {
                Box::pin(std::future::ready(Ok(Some(SignerSetInfo {
                    aggregate_key,
                    signer_set: signer_set.clone(),
                    signatures_required: 1,
                }))))
            });
    })
    .await;

    // Setup the scenario: we want a swept deposit
    let amounts = [SweepAmounts {
        amount: 700_000,
        max_fee: 500_000,
        is_deposit: true,
    }];
    let mut setup = TestSweepSetup2::new_setup(signers.clone(), faucet, &amounts);

    // Store everything we need for the deposit to be considered swept
    setup.submit_sweep_tx(rpc, faucet);
    fetch_canonical_bitcoin_blockchain(&db, rpc).await;

    setup.store_stacks_genesis_block(&db).await;
    setup.store_dkg_shares(&db).await;
    setup.store_donation(&db).await;
    setup.store_deposit_txs(&db).await;
    setup.store_deposit_request(&db).await;
    setup.store_deposit_decisions(&db).await;
    setup.store_sweep_tx(&db).await;

    prevent_dkg_on_changed_signer_set_info(&ctx, aggregate_key);

    let (bitcoin_chain_tip, _) = db.get_chain_tips().await;
    ctx.state().set_bitcoin_chain_tip(bitcoin_chain_tip);
    // If we try to sign a complete deposit, we will ask the bitcoin node to
    // asses the fees, so we need to mock this.
    let sweep_tx_info = setup.sweep_tx_info.unwrap().tx_info;
    ctx.with_bitcoin_client(|client| {
        client.expect_get_tx_info().returning(move |_, _| {
            let sweep_tx_info = sweep_tx_info.clone();
            Box::pin(async { Ok(Some(sweep_tx_info)) })
        });
    })
    .await;

    // Start the coordinator event loop and wait for it to be ready
    let start_flag = Arc::new(AtomicBool::new(false));
    let flag = start_flag.clone();

    let signing_round_max_duration = Duration::from_secs(2);
    let ev = TxCoordinatorEventLoop {
        network: signer_network.spawn(),
        context: ctx.clone(),
        context_window: 10000,
        private_key: signers.private_key(),
        signing_round_max_duration,
        bitcoin_presign_request_max_duration: Duration::from_secs(1),
        threshold: ctx.config().signer.bootstrap_signatures_required,
        dkg_max_duration: Duration::from_secs(1),
        is_epoch3: true,
    };
    tokio::spawn(async move {
        flag.store(true, Ordering::Relaxed);
        ev.run().await
    });

    while !start_flag.load(Ordering::SeqCst) {
        Sleep::for_millis(10).await;
    }

    // We will use network messages to detect the coordinator attempt, so we
    // need to connect to the network
    let fake_ctx = ctx.clone();
    let mut fake_signer = network.connect(&fake_ctx).spawn();

    // Finally, set the deposit status according in the smart contract
    if deposit_completed {
        set_deposit_completed(&mut ctx).await;
    } else {
        set_deposit_incomplete(&mut ctx).await;
    }

    // Wake up the coordinator
    ctx.signal(RequestDeciderEvent::NewRequestsHandled.into())
        .expect("failed to signal");

    let network_msg = tokio::time::timeout(signing_round_max_duration, fake_signer.receive()).await;

    if deposit_completed {
        network_msg.expect_err("expected timeout, got something instead");
    } else {
        let network_msg = network_msg.expect("failed to get a msg").unwrap();
        assert_matches!(
            network_msg.payload,
            Payload::StacksTransactionSignRequest(_)
        );
    }

    testing::storage::drop_db(db).await;
}

/// Module containing a test suite and helpers specific to
/// [`TxCoordinatorEventLoop::get_eligible_pending_withdrawal_requests`].
mod get_eligible_pending_withdrawal_requests {
    use std::sync::atomic::AtomicU64;
    use test_case::test_case;

    use signer::{
        WITHDRAWAL_DUST_LIMIT,
        bitcoin::MockBitcoinInteract,
        emily_client::MockEmilyInteract,
        network::in_memory2::SignerNetworkInstance,
        storage::model::{
            BitcoinBlock, BitcoinBlockHeight, StacksBlock, WithdrawalRequest, WithdrawalSigner,
        },
        testing::{
            blocks::{BitcoinChain, StacksChain},
            storage::{DbReadTestExt as _, DbWriteTestExt as _},
        },
        transaction_coordinator::{GetPendingRequestsParams, TxCoordinatorEventLoop},
    };

    use super::*;

    // A type alias for [`TxCoordinatorEventLoop`], typed with [`PgStore`] and
    // mocked clients, which are what's used in this mod.
    type MockedCoordinator = TxCoordinatorEventLoop<
        TestContext<
            PgStore,
            WrappedMock<MockBitcoinInteract>,
            WrappedMock<MockStacksInteract>,
            WrappedMock<MockEmilyInteract>,
        >,
        SignerNetworkInstance,
    >;

    /// Creates [`WithdrawalSigner`]s for each vote in the provided slice,
    /// zipped together with the signer keys from the provided
    /// [`TestSignerSet`], and stores them in the database.
    async fn store_votes(
        db: &PgStore,
        request: &WithdrawalRequest,
        signer_set: &TestSignerSet,
        votes: &[bool],
    ) {
        // Create an iterator of signer keys and their corresponding votes.
        let signer_votes = signer_set
            .signer_keys()
            .iter()
            .cloned()
            .zip(votes.iter().cloned());

        for (signer_pub_key, is_accepted) in signer_votes {
            let signer = WithdrawalSigner {
                request_id: request.request_id,
                block_hash: request.block_hash,
                txid: request.txid,
                signer_pub_key,
                is_accepted,
            };

            // Write the decision to the database.
            db.write_withdrawal_signer_decision(&signer)
                .await
                .expect("failed to write signer decision");
        }
    }

    /// Creates and stores a withdrawal request, confirmed in the specified
    /// bitcoin & stacks blocks.
    async fn store_withdrawal_request(
        db: &PgStore,
        bitcoin_block: &BitcoinBlock,
        stacks_block: &StacksBlock,
        amount: u64,
        max_fee: u64,
    ) -> WithdrawalRequest {
        let withdrawal_request = WithdrawalRequest {
            request_id: next_request_id(),
            block_hash: stacks_block.block_hash,
            bitcoin_block_height: bitcoin_block.block_height,
            amount,
            max_fee,
            ..Faker.fake()
        };

        db.write_withdrawal_request(&withdrawal_request)
            .await
            .expect("failed to write withdrawal request");

        withdrawal_request
    }

    /// Gets the next withdrawal request ID to use for testing.
    fn next_request_id() -> u64 {
        static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(0);
        NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    }

    /// Helper function to set up the database with bitcoin and stacks chains,
    /// a set of signers and their DKG shares.
    async fn test_setup(
        db: &PgStore,
        chains_length: u64,
    ) -> (
        BitcoinChain,
        StacksChain,
        TestSignerSet,
        BTreeSet<PublicKey>,
    ) {
        let signer_set = TestSignerSet::new(&mut OsRng);
        let signer_keys = signer_set.keys.iter().copied().collect();

        // Create a new bitcoin chain with 31 blocks and a sibling stacks chain
        // anchored starting at block 0.
        let bitcoin_chain = BitcoinChain::new_with_length(chains_length as usize);
        let stacks_chain = StacksChain::new_anchored(&bitcoin_chain);

        // Write the blocks to the database.
        db.write_blocks(&bitcoin_chain, &stacks_chain).await;

        // Get the chain tips and assert that they're what we expect.
        let (bitcoin_chain_tip, stacks_chain_tip) = db.get_chain_tips().await;
        assert_eq!(
            bitcoin_chain_tip.block_hash,
            bitcoin_chain.chain_tip().block_hash
        );
        assert_eq!(stacks_chain_tip, stacks_chain.chain_tip().block_hash);

        // Create DKG shares and write them to the database.
        let dkg_shares = model::EncryptedDkgShares {
            aggregate_key: signer_set.aggregate_key(),
            started_at_bitcoin_block_hash: bitcoin_chain_tip.block_hash,
            started_at_bitcoin_block_height: bitcoin_chain_tip.block_height,
            signer_set_public_keys: signer_set.signer_keys().to_vec(),
            dkg_shares_status: DkgSharesStatus::Verified,
            ..Faker.fake()
        };
        db.write_encrypted_dkg_shares(&dkg_shares).await.unwrap();

        (bitcoin_chain, stacks_chain, signer_set, signer_keys)
    }

    struct TestParams {
        chain_length: u64,
        signature_threshold: u16,
        sbtc_limits: SbtcLimits,
        amount: u64,
        num_approves: usize,
        num_expected_results: usize,
        expiry_window: u64,
        expiry_buffer: u64,
        min_confirmations: u64,
        at_block_height: BitcoinBlockHeight,
    }

    impl Default for TestParams {
        fn default() -> Self {
            Self {
                chain_length: 8,
                signature_threshold: 2,
                sbtc_limits: SbtcLimits::unlimited(),
                amount: 1_000,
                num_approves: 3,
                num_expected_results: 1,
                expiry_window: 24,
                expiry_buffer: 0,
                min_confirmations: 0,
                at_block_height: 0u64.into(),
            }
        }
    }

    /// Asserts that
    /// [`TxCoordinatorEventLoop::get_eligible_pending_withdrawal_requests`]
    /// correctly filters requests based on its parameters.
    #[test_case(TestParams::default(); "should_pass_all_validations")]
    #[test_case(TestParams {
        amount: WITHDRAWAL_DUST_LIMIT - 1,
        num_expected_results: 0,
        ..Default::default()
    }; "amount_below_dust_limit_skipped")]
    #[test_case(TestParams {
        amount: WITHDRAWAL_DUST_LIMIT,
        num_expected_results: 1,
        ..Default::default()
    }; "amount_at_dust_limit_allowed")]
    #[test_case(TestParams {
        amount: 1_000,
        sbtc_limits: SbtcLimits::zero(),
        num_expected_results: 0,
        ..Default::default()
    }; "amount_over_per_withdrawal_limit")]
    #[test_case(TestParams {
        // This case will calculate the confirmations as:
        // chain_length (10) - min_confirmations (6) = 4 (maximum block height),
        // at_block_height (5) > 4 (maximum).
        chain_length: 10,
        at_block_height: 5u64.into(),
        min_confirmations: 6,
        num_expected_results: 0,
        ..Default::default()
    }; "insufficient_confirmations_one_too_few")]
    #[test_case(TestParams {
        // This case will calculate the confirmations as:
        // chain_length (10) - min_confirmations(6) = 4 (maximum block height),
        // at_block_height (4) <= 4.
        chain_length: 10,
        at_block_height: 4u64.into(),
        min_confirmations: 6,
        num_expected_results: 1,
        ..Default::default()
    }; "exact_number_of_confirmations_allowed")]
    #[test_case(TestParams {
        signature_threshold: 2,
        num_approves: 1,
        num_expected_results: 0,
        ..Default::default()
    }; "insufficient_votes")]
    #[test_case(TestParams {
        // This case will calculate the soft expiry as:
        // chain_length - expiry_window + expiry_buffer = 4 and 3 < 4.
        chain_length: 10,
        expiry_window: 10,
        expiry_buffer: 4,
        at_block_height: 3u64.into(),
        num_expected_results: 0,
        ..Default::default()
    }; "soft_expiry_one_block_too_old")]
    #[test_case(TestParams {
        // This case will calculate the soft expiry as:
        // chain_length (10) - expiry_window (10) + expiry_buffer (4) = 4,
        // and at_block_height (4) == 4.
        chain_length: 10,
        expiry_window: 10,
        expiry_buffer: 4,
        at_block_height: 4u64.into(),
        num_expected_results: 1,
        ..Default::default()
    }; "soft_expiry_exact_block_allowed")]
    #[test_case(TestParams {
        // This case will calculate the hard expiry as:
        // chain_length (10) - expiry_window (5) = 5,
        // and at_block_height (5) == 5
        chain_length: 10,
        expiry_window: 5,
        expiry_buffer: 0,
        at_block_height: 5u64.into(),
        num_expected_results: 1,
        ..Default::default()
    }; "hard_expiry_exact_block_allowed")]
    #[test_case(TestParams {
        // This case will calculate the hard expiry as:
        // chain_length (10) - expiry_window (5) = 5,
        // and at_block_height (4) < 5
        chain_length: 10,
        expiry_window: 5,
        expiry_buffer: 0,
        at_block_height: 4u64.into(),
        num_expected_results: 0,
        ..Default::default()
    }; "hard_expiry_one_block_too_old")]
    #[test_log::test(tokio::test)]
    async fn test_validations(params: TestParams) {
        let db = testing::storage::new_test_database().await;

        // Note: we create the chains with a length of `chain_length + 1` to
        // allow for 1-based indexing in the parameters above (the blockchain
        // starts at block height 0, so the chain tip of a chain with 10 blocks
        // has a height of 9).
        let (bitcoin_chain, stacks_chain, signer_set, _) =
            test_setup(&db, params.chain_length + 1).await;

        let (bitcoin_chain_tip, stacks_chain_tip) = db.get_chain_tips().await;

        // Define the parameters for the pending requests call.
        let get_requests_params = GetPendingRequestsParams {
            aggregate_key: &signer_set.aggregate_key(),
            bitcoin_chain_tip: &bitcoin_chain_tip,
            stacks_chain_tip: &stacks_chain_tip,
            signature_threshold: params.signature_threshold,
            sbtc_limits: &params.sbtc_limits,
        };

        // Create a request below the dust limit.
        let request = store_withdrawal_request(
            &db,
            bitcoin_chain.nth_block(params.at_block_height),
            stacks_chain.nth_block((*params.at_block_height).into()), // Here we can cast one height to another because in this test chains are 1 to 1.
            params.amount,
            1_000, // Max fee isn't validated here.
        )
        .await;

        // Create and store votes for the request.
        let votes = vec![true; params.num_approves];
        store_votes(&db, &request, &signer_set, &votes).await;

        //Get pending withdrawals from coordinator
        let pending_withdrawals = MockedCoordinator::get_eligible_pending_withdrawal_requests(
            &db,
            params.expiry_window,
            params.expiry_buffer,
            params.min_confirmations,
            &get_requests_params,
        )
        .await
        .expect("failed to fetch eligible pending withdrawal requests");

        assert_eq!(pending_withdrawals.len(), params.num_expected_results);

        testing::storage::drop_db(db).await;
    }
}

// This test checks that the coordinator attempts to fulfill its
// other duties if DKG encounters an error but there's an existing
// aggregate key to fallback on.
#[test_log::test(tokio::test)]
async fn should_handle_dkg_coordination_failure() {
    let mut rng = get_rng();
    let context = TestContext::builder()
        .with_in_memory_storage()
        .with_mocked_clients()
        .build();

    let storage = context.get_storage_mut();

    // Create a bitcoin block to serve as chain tip
    let bitcoin_block: model::BitcoinBlock = Faker.fake_with_rng(&mut rng);
    storage.write_bitcoin_block(&bitcoin_block).await.unwrap();

    // Get chain tip reference
    let chain_tip = storage
        .get_bitcoin_canonical_chain_tip_ref()
        .await
        .unwrap()
        .unwrap();

    // Create a set of signer public keys and update the context state
    let mut signer_keys = BTreeSet::new();
    for _ in 0..3 {
        // Create 3 signers
        let private_key = PrivateKey::new(&mut rng);
        let public_key = PublicKey::from_private_key(&private_key);
        signer_keys.insert(public_key);
    }
    context
        .state()
        .update_current_signer_set(signer_keys.clone());
    context.state().set_bitcoin_chain_tip(chain_tip);

    // Mock the stacks client to handle contract source checks
    context
        .with_stacks_client(|client| {
            client.expect_get_contract_source().returning(|_, _| {
                Box::pin(async {
                    Ok(ContractSrcResponse {
                        source: String::new(),
                        publish_height: 1,
                        marf_proof: None,
                    })
                })
            });
        })
        .await;

    // Verify DKG should run
    assert!(
        transaction_coordinator::should_coordinate_dkg(&context, &chain_tip)
            .await
            .unwrap(),
        "DKG should be triggered since no shares exist yet"
    );

    // Create coordinator with test parameters using SignerNetwork::single
    let network = SignerNetwork::single(&context);
    let mut coordinator = TxCoordinatorEventLoop {
        context: context.clone(),
        network: network.spawn(),
        private_key: PrivateKey::new(&mut rng),
        threshold: 3,
        context_window: 5,
        signing_round_max_duration: std::time::Duration::from_secs(5),
        bitcoin_presign_request_max_duration: std::time::Duration::from_secs(5),
        // short be short enough to broadcast, yet fail
        dkg_max_duration: Duration::from_millis(10),
        is_epoch3: true,
    };

    // We're verifying that the coordinator is currently
    // processing requests correctly. Since we previously checked
    // that 'should_coordinate_dkg' will trigger and we set the
    // 'dkg_max_duration' to 10 milliseconds we expect that
    // DKG will run & fail
    let result = coordinator.process_new_blocks().await;
    assert!(
        result.is_ok(),
        "process_new_blocks should complete successfully even with DKG failure"
    );

    // Here we check that DKG ran & correctly failed by fetching
    // the latest DKG shares from storage. We test it failed
    // by asserting that the latest row is_none()
    let dkg_shares = storage.get_latest_encrypted_dkg_shares().await.unwrap();
    assert!(
        dkg_shares.is_none(),
        "DKG shares should not exist since DKG failed to complete due to timeout"
    );

    // Verify that we can still process blocks after DKG failure,
    // this final assert specifically checks that blocks are still
    // being processed since there was an aggregate key to fallback on
    let result = coordinator.process_new_blocks().await;
    assert!(
        result.is_ok(),
        "Should be able to continue processing blocks after DKG failure"
    );
}

/// Similar to `create_signers_keys` in `wallet.rs`, but returning also the keypairs
fn generate_random_signers<R>(
    rng: &mut R,
    num_signers: usize,
    signatures_required: u16,
) -> (regtest::Recipient, SignerWallet, Vec<Keypair>)
where
    R: rand::Rng,
{
    let aggregated_signer = regtest::Recipient::new(bitcoin::AddressType::P2tr);

    // We only take an odd number of signers so that the math works out.
    assert_eq!(num_signers % 2, 1);

    // The private keys of half of the other signers
    let pks: Vec<secp256k1::SecretKey> = std::iter::repeat_with(|| secp256k1::SecretKey::new(rng))
        .take(num_signers / 2)
        .collect();

    let mut keypairs: Vec<Keypair> = pks
        .clone()
        .into_iter()
        .chain(pks.into_iter().map(secp256k1::SecretKey::negate))
        .map(|sk| Keypair::from_secret_key(SECP256K1, &sk))
        .chain([aggregated_signer.keypair])
        .collect();
    keypairs.reverse();

    let mut signer_keys: Vec<PublicKey> =
        keypairs.iter().map(|kp| kp.public_key().into()).collect();
    signer_keys.sort();

    let wallet = SignerWallet::new(
        &signer_keys,
        signatures_required,
        signer::config::NetworkKind::Regtest,
        0,
    )
    .unwrap();

    (aggregated_signer, wallet, keypairs)
}

/// Wait for the next stacks block, assuming someone else keeps producing blocks
async fn wait_next_stacks_block(stacks_client: &StacksClient) {
    let initial_height = stacks_client
        .get_node_info()
        .await
        .unwrap()
        .stacks_tip_height;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let height = stacks_client
                .get_node_info()
                .await
                .unwrap()
                .stacks_tip_height;
            if height > initial_height {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await
        }
    })
    .await
    .unwrap()
}

fn make_deposit_requests<U>(
    depositor: &Recipient,
    amounts: &[u64],
    utxo: U,
    max_fee: u64,
    signers_public_key: bitcoin::XOnlyPublicKey,
) -> (Transaction, Vec<DepositRequest>)
where
    U: regtest::AsUtxo,
{
    let deposit_inputs = DepositScriptInputs {
        signers_public_key,
        max_fee,
        recipient: PrincipalData::from(StacksAddress::burn_address(false)),
    };
    let reclaim_inputs = ReclaimScriptInputs::try_new(50, bitcoin::ScriptBuf::new()).unwrap();

    let deposit_script = deposit_inputs.deposit_script();
    let reclaim_script = reclaim_inputs.reclaim_script();

    let mut outputs = vec![];
    for amount in amounts {
        outputs.push(bitcoin::TxOut {
            value: Amount::from_sat(*amount),
            script_pubkey: sbtc::deposits::to_script_pubkey(
                deposit_script.clone(),
                reclaim_script.clone(),
            ),
        })
    }

    let fee = regtest::BITCOIN_CORE_FALLBACK_FEE.to_sat();
    outputs.push(bitcoin::TxOut {
        value: utxo.amount() - Amount::from_sat(amounts.iter().sum::<u64>() + fee),
        script_pubkey: depositor.address.script_pubkey(),
    });

    let mut deposit_tx = Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: bitcoin::OutPoint::new(utxo.txid(), utxo.vout()),
            sequence: bitcoin::Sequence::ZERO,
            script_sig: bitcoin::ScriptBuf::new(),
            witness: bitcoin::Witness::new(),
        }],
        output: outputs,
    };

    regtest::p2tr_sign_transaction(&mut deposit_tx, 0, &[utxo], &depositor.keypair);

    let mut requests = vec![];
    for (index, amount) in amounts.iter().enumerate() {
        let req = CreateDepositRequest {
            outpoint: bitcoin::OutPoint::new(deposit_tx.compute_txid(), index as u32),
            deposit_script: deposit_script.clone(),
            reclaim_script: reclaim_script.clone(),
        };

        requests.push(DepositRequest {
            outpoint: req.outpoint,
            max_fee,
            signer_bitmap: BitArray::ZERO,
            amount: *amount,
            deposit_script: deposit_script.clone(),
            reclaim_script: reclaim_script.clone(),
            reclaim_script_hash: Some(model::TaprootScriptHash::from(&reclaim_script)),
            signers_public_key,
        });
    }

    (deposit_tx, requests)
}

/// This test requires a running stacks node.
/// To run this test first run devenv:
/// ```bash
/// make devenv-up
/// ```
/// And wait for nakamoto to kick in; finally, stop the bitcoin miner.
///
/// You also need to fund the faucet (after a while it will unlock coinbase):
/// ```bash
/// cargo run -p signer --bin demo-cli fund-btc --recipient BCRT1QW508D6QEJXTDG4Y5R3ZARVARY0C5XW7KYGT080 --amount 1000000000
/// cargo run -p signer --bin demo-cli generate-block
/// ```
#[ignore = "This is an integration test that requires devenv running"]
#[test_log::test(tokio::test)]
async fn reuse_nonce_attack() {
    let stacks = StacksClient::new(Url::parse("http://127.0.0.1:20443").unwrap()).unwrap();
    let (rpc, faucet) = regtest::initialize_blockchain_devenv();
    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@127.0.0.1:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    let mut rng = get_rng();

    let signatures_required = 2;
    let (_, signer_wallet, signer_key_pairs) =
        generate_random_signers(&mut rng, 3, signatures_required);
    let deployer = *signer_wallet.address();

    testing_api::wipe_databases(&emily_client.config().as_testing())
        .await
        .unwrap();

    let network = WanNetwork::default();

    // =========================================================================
    // Funds the (randomized) signer set multisig
    // =========================================================================
    let (regtest_signer_wallet, regtest_signer_key_pairs): (_, [Keypair; 3]) =
        testing::wallet::regtest_bootstrap_wallet();
    let regtest_signer_wallet_account = stacks
        .get_account(regtest_signer_wallet.address())
        .await
        .unwrap();
    regtest_signer_wallet.set_nonce(regtest_signer_wallet_account.nonce);
    let signer_stx_state = SignerStxState {
        wallet: regtest_signer_wallet,
        keys: regtest_signer_key_pairs,
        stacks_client: stacks.clone(),
    };
    let stx_funding = TransactionPayload::TokenTransfer(
        signer_wallet.address().to_account_principal(),
        100_000_000,
        TokenTransferMemo([0u8; 34]),
    );
    signer_stx_state.sign_and_submit(&stx_funding).await;
    // To ensure the tx is mined and anchored before we attempt to deploy the
    // contracts we generate a some blocks
    for _ in 0..2 {
        Sleep::for_secs(3).await;
        faucet.generate_block();
    }

    // =========================================================================
    // Create a database, an associated context, and a Keypair for each of the
    // signers in the signing set.
    // =========================================================================
    let signer_set_public_keys: BTreeSet<PublicKey> = signer_key_pairs
        .iter()
        .map(|kp| kp.public_key().into())
        .collect();

    let mut signers = Vec::new();
    for kp in signer_key_pairs.iter() {
        let db = testing::storage::new_test_database().await;
        let ctx = TestContext::builder()
            .with_storage(db.clone())
            .with_first_bitcoin_core_client()
            .with_emily_client(emily_client.clone())
            .with_stacks_client(stacks.clone())
            .modify_settings(|settings| {
                settings.signer.bootstrap_signatures_required = signatures_required;
                settings.signer.bootstrap_signing_set = signer_set_public_keys.clone();
                settings.signer.deployer = deployer;
                settings.signer.requests_processing_delay = Duration::from_secs(1);
                settings.signer.bitcoin_processing_delay = Duration::from_secs(1);
            })
            .build();

        fetch_canonical_bitcoin_blockchain(&db, rpc).await;

        for signer in &signer_set_public_keys {
            ctx.state().current_signer_set().add_signer(*signer);
        }
        let network = network.connect(&ctx);

        signers.push((ctx, db, kp, network));
    }

    // We need to inspect the signer status, so we pick the first one for it
    let (ctx, db, _, _) = signers.first().unwrap();

    // =========================================================================
    // Start the TxCoordinatorEventLoop, TxSignerEventLoop and BlockObserver
    // processes for each signer.
    // -------------------------------------------------------------------------
    // - We only proceed with the test after all processes have started, and
    //   we use a counter to notify us when that happens.
    // =========================================================================
    let start_count = Arc::new(AtomicU8::new(0));

    for (ctx, _, kp, network) in signers.iter() {
        let ev = TxCoordinatorEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 10000,
            private_key: kp.secret_key().into(),
            signing_round_max_duration: Duration::from_secs(10),
            bitcoin_presign_request_max_duration: Duration::from_secs(10),
            threshold: ctx.config().signer.bootstrap_signatures_required,
            dkg_max_duration: Duration::from_secs(10),
            is_epoch3: true,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = TxSignerEventLoop {
            network: network.spawn(),
            threshold: ctx.config().signer.bootstrap_signatures_required as u32,
            context: ctx.clone(),
            context_window: 10000,
            wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
            signer_private_key: kp.secret_key().into(),
            last_presign_block: None,
            rng: rand::rngs::OsRng,
            dkg_begin_pause: None,
            dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
            stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let ev = RequestDeciderEventLoop {
            network: network.spawn(),
            context: ctx.clone(),
            context_window: 1000,
            deposit_decisions_retry_window: 1,
            withdrawal_decisions_retry_window: 1,
            blocklist_checker: Some(()),
            signer_private_key: kp.secret_key().into(),
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            ev.run().await
        });

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: testing::btc::new_zmq_block_hash_stream(BITCOIN_CORE_ZMQ_ENDPOINT)
                .await,
        };
        let counter = start_count.clone();
        tokio::spawn(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            block_observer.run().await
        });
    }

    while start_count.load(Ordering::SeqCst) < 12 {
        Sleep::for_millis(10).await;
    }

    // =========================================================================
    // Wait for contract deployment
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Wait for DKG + key rotation
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    let shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();
    assert_eq!(shares.dkg_shares_status, DkgSharesStatus::Verified);

    wait_next_stacks_block(&stacks).await;
    assert!(
        stacks
            .get_current_signers_aggregate_key(&deployer)
            .await
            .unwrap()
            .is_some()
    );

    // =========================================================================
    // Create signers UTXO
    // =========================================================================
    let script_pub_key = shares.aggregate_key.signers_script_pubkey();
    let address = Address::from_script(&script_pub_key, bitcoin::Network::Regtest).unwrap();
    faucet.send_to(100_000, &address);

    // =========================================================================
    // Fund depositor
    // =========================================================================
    let depositor = Recipient::new(AddressType::P2tr);
    faucet.send_to(50_000_000, &depositor.address);

    faucet.generate_block();
    wait_for_signers(&signers).await;

    // =========================================================================
    // Create deposits
    // =========================================================================
    let utxo = depositor.get_utxos(rpc, None).pop().unwrap();

    let num_deposits = 5;
    let amounts = vec![100_000; num_deposits];
    let signers_public_key = shares.aggregate_key.into();
    let max_fee = 50_000;

    let (deposit_tx, deposit_requests) =
        make_deposit_requests(&depositor, &amounts, utxo, max_fee, signers_public_key);

    rpc.send_raw_transaction(&deposit_tx).unwrap();

    for request in &deposit_requests {
        assert_eq!(deposit_tx.compute_txid(), request.outpoint.txid);

        let body = request.as_emily_request(&deposit_tx);
        let _ = deposit_api::create_deposit(emily_client.config(), body)
            .await
            .unwrap();
    }

    // Confirming the deposits, the signers should fulfill them
    faucet.generate_block();
    wait_for_signers(&signers).await;

    // Check that we have the sweep tx in the mempool servicing all deposits
    let txids = ctx.bitcoin_client.inner_client().get_raw_mempool().unwrap();
    // The sweep plus the stacks commitment
    assert_eq!(txids.len(), 2);

    let sweep_tx = txids
        .iter()
        .filter_map(|txid| {
            let tx = ctx.bitcoin_client.get_tx(txid).unwrap().unwrap();
            // Stacks commitment txs first output is a op return
            if tx.tx.output[0].value == Amount::ZERO {
                None
            } else {
                Some(tx)
            }
        })
        .next()
        .unwrap();
    assert_eq!(sweep_tx.tx.input.len(), num_deposits + 1);

    // =========================================================================
    // Preparare concurrent tx
    // =========================================================================
    let signer_wallet_account = stacks.get_account(&deployer).await.unwrap();
    // We want to mess with the third complete-deposit, for no specific reason
    signer_wallet.set_nonce(signer_wallet_account.nonce + 2);

    let signer_stx_state = SignerStxState {
        wallet: signer_wallet,
        keys: signer_key_pairs.clone().try_into().unwrap(),
        stacks_client: stacks.clone(),
    };
    let stx_funding = TransactionPayload::TokenTransfer(
        PrincipalData::from(StacksAddress::burn_address(false)),
        50_000_000,
        TokenTransferMemo([0u8; 34]),
    );
    // The tx will be submitted correctly and just wait in the mempool until txs
    // filling the previous nonces will be mined
    signer_stx_state.sign_and_submit(&stx_funding).await;
    // Ensure the signers/miner pick the tx up
    Sleep::for_secs(1).await;

    // =========================================================================
    // Actual test: process the pending completion deposits with a concurrent tx
    // already in the mempool
    // =========================================================================
    faucet.generate_block();
    wait_for_signers(&signers).await;

    wait_next_stacks_block(&stacks).await;

    // =========================================================================
    // Test assertions
    // =========================================================================
    // Ensure the concurrent tx (burning some funds) was mined
    let dpeloyer_balance = stacks.get_account(&deployer).await.unwrap().balance;
    assert_lt!(dpeloyer_balance, 50_000_000);

    // Check how many deposits we have completed
    let mut completed_deposits = 0;
    for request in deposit_requests {
        completed_deposits += stacks
            .is_deposit_completed(&deployer, &request.outpoint)
            .await
            .unwrap() as usize;
    }
    assert_eq!(completed_deposits, num_deposits - 1);

    for (_, db, _, _) in signers {
        testing::storage::drop_db(db).await;
    }
}
