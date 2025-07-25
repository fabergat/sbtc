//! # Block observer
//!
//! This module contains the block observer implementation for the sBTC signer.
//! The block observer is responsible for populating the signer database with
//! information from the Bitcoin and Stacks blockchains, and notifying
//! the signer event loop whenever the state has been updated.
//!
//! The following information is extracted by the block observer:
//! - Bitcoin blocks
//! - Stacks blocks
//! - Deposit requests
//! - sBTC transactions
//! - Withdraw requests
//! - Deposit accept transactions
//! - Withdraw accept transactions
//! - Withdraw reject transactions
//! - Update signer set transactions
//! - Set aggregate key transactions

use std::future::Future;
use std::time::Duration;

use crate::bitcoin::BitcoinInteract;
use crate::bitcoin::rpc::BitcoinBlockHeader;
use crate::bitcoin::rpc::BitcoinTxInfo;
use crate::bitcoin::utxo::TxDeconstructor as _;
use crate::context::Context;
use crate::context::SbtcLimits;
use crate::context::SignerEvent;
use crate::emily_client::EmilyInteract;
use crate::error::Error;
use crate::keys::PublicKey;
use crate::keys::SignerScriptPubKey as _;
use crate::metrics::BITCOIN_BLOCKCHAIN;
use crate::metrics::Metrics;
use crate::stacks::api::GetNakamotoStartHeight as _;
use crate::stacks::api::SignerSetInfo;
use crate::stacks::api::StacksInteract;
use crate::stacks::api::TenureBlockHeaders;
use crate::storage::DbRead;
use crate::storage::DbWrite;
use crate::storage::Transactable;
use crate::storage::TransactionHandle;
use crate::storage::model;
use crate::storage::model::EncryptedDkgShares;
use bitcoin::Amount;
use bitcoin::BlockHash;
use bitcoin::ScriptBuf;
use futures::stream::Stream;
use futures::stream::StreamExt;
use sbtc::deposits::CreateDepositRequest;
use sbtc::deposits::DepositInfo;
use std::collections::HashSet;

/// Block observer
#[derive(Debug)]
pub struct BlockObserver<Context, BlockHashStream> {
    /// Signer context
    pub context: Context,
    /// Stream of blocks from the block notifier
    pub bitcoin_blocks: BlockHashStream,
}

/// A full "deposit", containing the bitcoin transaction and a fully
/// extracted and verified `scriptPubKey` from one of the transaction's
/// UTXOs.
#[derive(Debug, Clone)]
pub struct Deposit {
    /// The transaction spent to the signers as a deposit for sBTC.
    pub tx_info: BitcoinTxInfo,
    /// The deposit information included in one of the output
    /// `scriptPubKey`s of the above transaction.
    pub info: DepositInfo,
    /// The block hash of the Bitcoin block that includes this transaction.
    pub block_hash: BlockHash,
}

impl DepositRequestValidator for CreateDepositRequest {
    async fn validate<C>(&self, client: &C, is_mainnet: bool) -> Result<Option<Deposit>, Error>
    where
        C: BitcoinInteract,
    {
        // Fetch the transaction from either a block or from the mempool
        let Some(response) = client.get_tx(&self.outpoint.txid).await? else {
            return Ok(None);
        };

        // If the transaction has not been confirmed yet, then the block
        // hash will be None. The transaction has not failed validation,
        // let's try again when it gets confirmed.
        let Some(block_hash) = response.block_hash else {
            return Ok(None);
        };

        if response.tx.is_coinbase() {
            return Err(Error::BitcoinTxCoinbase(self.outpoint.txid));
        }

        // The `get_tx_info` call here should not return None, we know that
        // it has been included in a block.
        let Some(tx_info) = client.get_tx_info(&self.outpoint.txid, &block_hash).await? else {
            return Ok(None);
        };

        // Check that the necessary data is present for the transaction
        // info struct.
        tx_info.validate()?;

        Ok(Some(Deposit {
            info: self.validate_tx(&tx_info.tx, is_mainnet)?,
            tx_info,
            block_hash,
        }))
    }
}

/// A trait to add validation functionality to the [`CreateDepositRequest`]
/// type.
pub trait DepositRequestValidator {
    /// Validate this deposit request from the transaction.
    ///
    /// This function fetches the transaction using the given client and
    /// checks that the transaction has been submitted. The transaction
    /// need not be confirmed.
    fn validate<C>(
        &self,
        client: &C,
        is_mainnet: bool,
    ) -> impl Future<Output = Result<Option<Deposit>, Error>>
    where
        C: BitcoinInteract;
}

impl<C, S> BlockObserver<C, S>
where
    C: Context,
    S: Stream<Item = Result<bitcoin::BlockHash, Error>> + Unpin,
{
    /// Run the block observer
    #[tracing::instrument(skip_all, name = "block-observer")]
    pub async fn run(mut self) -> Result<(), Error> {
        let term = self.context.get_termination_handle();

        loop {
            if term.shutdown_signalled() {
                break;
            }

            // Bitcoin blocks will generally arrive in ~10 minute intervals, so
            // we don't need to be so aggressive in our timeout here.
            let poll = tokio::time::timeout(Duration::from_millis(100), self.bitcoin_blocks.next());

            match poll.await {
                Ok(Some(Ok(block_hash))) => {
                    tracing::info!("observed new bitcoin block from stream");
                    metrics::counter!(
                        Metrics::BlocksObservedTotal,
                        "blockchain" => BITCOIN_BLOCKCHAIN,
                    )
                    .increment(1);

                    if let Err(error) = self.process_bitcoin_blocks_until(block_hash).await {
                        tracing::warn!(%error, %block_hash, "could not process bitcoin blocks");
                    }

                    if let Err(error) = self.process_stacks_blocks().await {
                        tracing::warn!(%error, "could not process stacks blocks");
                    }

                    if let Err(error) = self.check_pending_dkg_shares(block_hash).await {
                        tracing::warn!(%error, "could not check pending dkg shares");
                        continue;
                    }

                    tracing::debug!("updating the signer state");
                    if let Err(error) = self.update_signer_state(block_hash).await {
                        tracing::warn!(%error, "could not update the signer state");
                        continue;
                    }

                    tracing::info!("loading latest deposit requests from Emily");
                    if let Err(error) = self.load_latest_deposit_requests().await {
                        tracing::warn!(%error, "could not load latest deposit requests from Emily");
                    }

                    self.context
                        .signal(SignerEvent::BitcoinBlockObserved.into())?;
                }
                Ok(Some(Err(error))) => {
                    tracing::warn!(%error, "error decoding new bitcoin block hash from stream");
                    continue;
                }
                _ => continue,
            };
        }

        tracing::info!("block observer has stopped");

        Ok(())
    }
}

impl<C: Context, B> BlockObserver<C, B> {
    /// Fetch deposit requests from Emily and store the ones that pass
    /// validation into the database.
    #[tracing::instrument(skip_all)]
    async fn load_latest_deposit_requests(&self) -> Result<(), Error> {
        let requests = self.context.get_emily_client().get_deposits().await?;
        self.load_requests(&requests).await
    }

    /// Validate the given deposit requests and store the ones that pass
    /// validation into the database.
    ///
    /// There are three types of errors that can happen during validation
    /// 1. The transaction fails primary validation. This means the deposit
    ///    script itself does not align with what we expect. If probably
    ///    does not follow our protocol.
    /// 2. The transaction passes step (1), but we don't recognize the
    ///    x-only public key in the deposit script.
    /// 3. We cannot find the associated transaction confirmed on a bitcoin
    ///    block, or when we encountered some unexpected error when
    ///    reaching out to bitcoin-core or our database.
    #[tracing::instrument(skip_all)]
    pub async fn load_requests(&self, requests: &[CreateDepositRequest]) -> Result<(), Error> {
        let mut deposit_requests = Vec::new();
        let mut deposit_request_txs = Vec::new();
        let bitcoin_client = self.context.get_bitcoin_client();
        let is_mainnet = self.context.config().signer.network.is_mainnet();

        for request in requests {
            let deposit = request
                .validate(&bitcoin_client, is_mainnet)
                .await
                .inspect_err(|error| tracing::warn!(%error, "could not validate deposit request"));

            // We log the error above, so we just need to extract the
            // deposit now.
            Metrics::increment_deposit_total(&deposit);
            let Ok(Some(deposit)) = deposit else { continue };

            self.process_bitcoin_blocks_until(deposit.block_hash)
                .await?;

            let tx = model::BitcoinTxRef {
                txid: deposit.tx_info.compute_txid().into(),
                block_hash: deposit.block_hash.into(),
            };

            deposit_requests.push(model::DepositRequest::from(deposit));
            deposit_request_txs.push(tx);
        }

        let db = self.context.get_storage_mut();
        db.write_bitcoin_transactions(deposit_request_txs).await?;
        db.write_deposit_requests(deposit_requests).await?;

        tracing::debug!("finished processing deposit requests");
        Ok(())
    }

    /// Set the sbtc start height, if it has not been set already.
    async fn set_sbtc_bitcoin_start_height(&self) -> Result<(), Error> {
        if self.context.state().is_sbtc_bitcoin_start_height_set() {
            return Ok(());
        }

        let pox_info = self.context.get_stacks_client().get_pox_info().await?;
        let nakamoto_start_height = pox_info
            .nakamoto_start_height()
            .ok_or(Error::MissingNakamotoStartHeight)?;

        self.context
            .state()
            .set_sbtc_bitcoin_start_height(nakamoto_start_height);

        Ok(())
    }

    /// Find the parent blocks from the given block that are also missing
    /// from our database.
    ///
    /// # Notes
    ///
    /// This function does two things:
    /// 1. Set the `sbtc_bitcoin_start_height` if it has not been set already. If
    ///    it is not set, then we fetch the stacks nakamoto start height
    ///    from stacks-core and use that value.
    /// 2. Continually fetches block headers from bitcoin-core until it
    ///    encounters a known block header or if the height of the block is
    ///    less than or equal to the `sbtc_bitcoin_start_height`.
    ///
    /// If there are many unknown blocks then this function can take some
    /// time. Since each header is 80 bytes, we should be able to fetch
    /// headers for the entire bitcoin blockchain (~900k blocks at the time
    /// of writing) into memory.
    #[tracing::instrument(skip_all, fields(%block_hash))]
    pub async fn next_headers_to_process(
        &self,
        mut block_hash: BlockHash,
    ) -> Result<Vec<BitcoinBlockHeader>, Error> {
        self.set_sbtc_bitcoin_start_height().await?;

        let start_height = self.context.state().get_sbtc_bitcoin_start_height();
        let mut headers = std::collections::VecDeque::new();
        let db = self.context.get_storage();
        let bitcoin_client = self.context.get_bitcoin_client();

        while !db.is_known_bitcoin_block_hash(&block_hash.into()).await? {
            let Some(header) = bitcoin_client.get_block_header(&block_hash).await? else {
                tracing::error!(%block_hash, "bitcoin-core does not know about block header");
                return Err(Error::BitcoinCoreUnknownBlockHeader(block_hash));
            };

            // We don't even try to write blocks to the database if the
            // height is less than the start height.
            if header.height < start_height {
                break;
            }

            let at_start_height = header.height == start_height;
            block_hash = header.previous_block_hash;
            headers.push_front(header);

            // We can write the block at the start height to the database.
            if at_start_height {
                break;
            }
        }

        Ok(headers.into())
    }

    /// Process bitcoin blocks until we get caught up to the given
    /// `block_hash`.
    ///
    /// This function starts at the given block hash and:
    /// 1. Works backwards, fetching block headers until it fetches one
    ///    that is already in the database or reaches a block that is at or
    ///    below the `sbtc_bitcoin_start_height`.
    /// 2. Starts from the header associated with the block with the least
    ///    height and writes the blocks and sweep transactions into the
    ///    database.
    /// 3. Bails if an error is encountered when fetching block headers or
    ///    when processing blocks.
    ///
    /// This means that if we stop processing blocks midway though,
    /// subsequent calls to this function will properly pick up from where
    /// we left off and update the database.
    async fn process_bitcoin_blocks_until(&self, block_hash: BlockHash) -> Result<(), Error> {
        let block_headers = self.next_headers_to_process(block_hash).await?;

        for block_header in block_headers {
            self.process_bitcoin_block(block_header).await?;
        }

        Ok(())
    }

    /// Write the bitcoin block and any transactions that spend to any of
    /// the signers `scriptPubKey`s to the database.
    #[tracing::instrument(skip_all, fields(block_hash = %block_header.hash))]
    async fn process_bitcoin_block(&self, block_header: BitcoinBlockHeader) -> Result<(), Error> {
        let block = self
            .context
            .get_bitcoin_client()
            .get_block(&block_header.hash)
            .await?
            .ok_or(Error::BitcoinCoreMissingBlock(block_header.hash))?;
        let db_block = model::BitcoinBlock::from(&block);

        let storage = self.context.get_storage_mut();

        // When a signer is not part of the bootstrap signing set but is
        // joining the set as a new signer, it will not have the signers
        // original scriptPubKey in its database, so it relies on the config
        // to inform them of what it is.
        let bootstrap_script_pubkey = self.context.config().signer.bootstrap_aggregate_key;

        // Begin a storage transaction.
        let storage_tx = storage.begin_transaction().await?;

        // Write the bitcoin block to the database (in the transaction).
        storage_tx.write_bitcoin_block(&db_block).await?;

        // Extract the sBTC-related transactions from the block and write them
        // to the database (within the transaction).
        extract_sbtc_transactions(
            &storage_tx,
            bootstrap_script_pubkey,
            block_header.hash,
            &block.transactions,
        )
        .await?;

        // Commit the storage transaction.
        storage_tx.commit().await?;

        tracing::debug!("finished processing bitcoin block");
        Ok(())
    }

    /// Process all recent stacks blocks.
    #[tracing::instrument(skip_all)]
    async fn process_stacks_blocks(&self) -> Result<(), Error> {
        tracing::info!("processing stacks block");
        let stacks_client = self.context.get_stacks_client();
        let db = self.context.get_storage_mut();
        let tenure_info = stacks_client.get_tenure_info().await?;

        tracing::debug!("fetching unknown ancestral blocks from stacks-core");
        let stacks_block_headers = crate::stacks::api::fetch_unknown_ancestors(
            &stacks_client,
            &db,
            tenure_info.tip_block_id,
        )
        .await?;

        let headers = stacks_block_headers
            .into_iter()
            .flat_map(TenureBlockHeaders::into_iter)
            .collect::<Vec<_>>();

        db.write_stacks_block_headers(headers).await?;

        tracing::debug!("finished processing stacks block");
        Ok(())
    }

    /// Update the sBTC peg limits from Emily
    async fn update_sbtc_limits(&self, chain_tip: BlockHash) -> Result<(), Error> {
        let limits = self.context.get_emily_client().get_limits().await?;
        let sbtc_deployed = self.context.state().sbtc_contracts_deployed();

        let max_mintable = if limits.total_cap_exists() && sbtc_deployed {
            let sbtc_supply = self
                .context
                .get_stacks_client()
                .get_sbtc_total_supply(&self.context.config().signer.deployer)
                .await?;
            // The maximum amount of sBTC that can be minted is the total cap
            // minus the current supply.
            limits
                .total_cap()
                .checked_sub(sbtc_supply)
                .unwrap_or(Amount::ZERO)
        } else {
            Amount::MAX_MONEY
        };

        let rolling_limits = limits.rolling_withdrawal_limits();
        let withdrawn_total = self
            .context
            .get_storage()
            .compute_withdrawn_total(&chain_tip.into(), rolling_limits.blocks)
            .await?;

        let limits = SbtcLimits::new(
            Some(limits.total_cap()),
            Some(limits.per_deposit_minimum()),
            Some(limits.per_deposit_cap()),
            Some(limits.per_withdrawal_cap()),
            Some(rolling_limits.blocks),
            Some(rolling_limits.cap),
            Some(withdrawn_total),
            Some(max_mintable),
        );
        let signer_state = self.context.state();
        if limits == signer_state.get_current_limits() {
            tracing::trace!(%limits, "sBTC limits have not changed");
        } else {
            tracing::debug!(%limits, "updated sBTC limits from Emily");
            signer_state.update_current_limits(limits);
        }
        Ok(())
    }

    /// Update the `SignerState` object with the current signer set,
    /// signatures required, and aggregate key data.
    ///
    /// # Notes
    ///
    /// The query used for fetching the cached information can take quite a
    /// lot of some time to complete on mainnet. So this function updates
    /// the signers state once so that the other event loops do not need to
    /// execute them. The cached information is the current signer set
    /// info. It gets this information from the last successful
    /// key-rotation contract call if it exists.
    async fn set_signer_set_info(&self) -> Result<(), Error> {
        let info = get_signer_set_info(&self.context).await?;

        let state = self.context.state();
        if let Some(info) = info {
            state.update_registry_signer_set_info(info);
        }

        Ok(())
    }

    /// Update the `SignerState` object with current bitcoin chain tip.
    async fn update_bitcoin_chain_tip(&self, chain_tip: BlockHash) -> Result<(), Error> {
        let db = self.context.get_storage();
        let chain_tip = db
            .get_bitcoin_block(&chain_tip.into())
            .await?
            .map(model::BitcoinBlockRef::from)
            .ok_or_else(|| Error::UnknownBitcoinBlock(chain_tip))?;

        self.context.state().set_bitcoin_chain_tip(chain_tip);
        Ok(())
    }

    /// Update the `SignerState` object with data that is unlikely to
    /// change until the arrival of the next bitcoin block.
    ///
    /// # Notes
    ///
    /// The function updates the following:
    /// * sBTC limits from Emily.
    /// * The current signer set.
    /// * The current aggregate key.
    /// * The current bitcoin chain tip.
    async fn update_signer_state(&self, chain_tip: BlockHash) -> Result<(), Error> {
        tracing::info!("loading sbtc limits from Emily");
        self.update_sbtc_limits(chain_tip).await?;

        tracing::info!("updating the signer state with the current signer set");
        self.set_signer_set_info().await?;

        tracing::info!("updating the signer state with the current bitcoin chain tip");
        self.update_bitcoin_chain_tip(chain_tip).await
    }

    /// Checks if the latest dkg share is pending and is no longer valid
    async fn check_pending_dkg_shares(&self, chain_tip: BlockHash) -> Result<(), Error> {
        let db = self.context.get_storage_mut();

        let last_dkg = db.get_latest_encrypted_dkg_shares().await?;

        if let Some(ref shares) = last_dkg {
            tracing::info!(
                aggregate_key = %shares.aggregate_key,
                status = ?shares.dkg_shares_status,
                "checking latest DKG shares"
            );
        }

        let Some(
            last_dkg @ EncryptedDkgShares {
                dkg_shares_status: model::DkgSharesStatus::Unverified,
                ..
            },
        ) = last_dkg
        else {
            return Ok(());
        };

        let chain_tip = db
            .get_bitcoin_block(&chain_tip.into())
            .await?
            .ok_or(Error::NoChainTip)?;
        let verification_window = self.context.config().signer.dkg_verification_window;

        let max_verification_height = last_dkg
            .started_at_bitcoin_block_height
            .saturating_add(verification_window as u64);

        if max_verification_height < chain_tip.block_height {
            tracing::info!(
                aggregate_key = %last_dkg.aggregate_key,
                "latest DKG shares are unverified and the verification window expired, marking them as failed"
            );
            db.revoke_dkg_shares(last_dkg.aggregate_key).await?;
        }

        Ok(())
    }
}

/// Extract all BTC transactions from the block where one of the UTXOs
/// can be spent by the signers.
///
/// # Note
///
/// When using the postgres storage, we need to make sure that this
/// function is called after the `Self::write_bitcoin_block` function
/// because of the foreign key constraints.
pub async fn extract_sbtc_transactions<Storage>(
    db: &Storage,
    bootstrap_aggregate_key: Option<PublicKey>,
    block_hash: BlockHash,
    txs: &[BitcoinTxInfo],
) -> Result<(), Error>
where
    Storage: DbRead + DbWrite,
{
    // Convert the bootstrap script public key to a `ScriptBuf` if it is
    // provided. This is used to check if the transaction outputs are
    // spent to the bootstrap signers' addresses.
    let bootstrap_script_pubkey = bootstrap_aggregate_key.map(|key| key.signers_script_pubkey());

    // Define a closure to extract the sBTC transactions from the given
    // transactions and write them to the database.
    let extract_fut = || async {
        // We store all the scriptPubKeys associated with the signers'
        // aggregate public key. Let's get the last years worth of them.
        let signer_script_pubkeys: HashSet<ScriptBuf> = db
            .get_signers_script_pubkeys()
            .await?
            .into_iter()
            .map(ScriptBuf::from_bytes)
            .chain(bootstrap_script_pubkey.clone())
            .collect();

        // Look through all the UTXOs in the given transaction slice and
        // keep the transactions where a UTXO is locked with a
        // `scriptPubKey` controlled by the signers.
        let mut sbtc_txs = Vec::new();
        for tx_info in txs {
            let txid = tx_info.compute_txid();
            tracing::trace!(%txid, "attempting to extract sbtc transaction");
            if tx_info.tx.is_coinbase() {
                continue;
            }

            // Bail if bitcoin-core doesn't return all the data that we
            // care about for a non-coinbase transaction. This will happen
            // if bitcoin core hasn't computed the undo data for the block
            // with these transactions, of it there is a bug in bitcoin
            // core.
            tx_info.validate()?;

            // If any of the outputs are spent to one of the signers'
            // addresses, then we care about it
            let outputs_spent_to_signers = tx_info
                .tx
                .output
                .iter()
                .any(|tx_out| signer_script_pubkeys.contains(&tx_out.script_pubkey));

            // We might not know about the new scriptPubKey, but we are
            // supposed to know about all existing scriptPubKeys, so we
            // check the inputs as well.
            let inputs_spent_by_signers = tx_info
                .vin
                .iter()
                .filter_map(|vin| vin.prevout.as_ref())
                .any(|prevout| signer_script_pubkeys.contains(&prevout.script_pubkey.script));

            if !outputs_spent_to_signers && !inputs_spent_by_signers {
                continue;
            }

            sbtc_txs.push(model::BitcoinTxRef {
                txid: txid.into(),
                block_hash: block_hash.into(),
            });

            for prevout in tx_info.to_inputs(&signer_script_pubkeys) {
                db.write_tx_prevout(&prevout).await?;
                if prevout.prevout_type == model::TxPrevoutType::Deposit {
                    metrics::counter!(
                        Metrics::DepositsSweptTotal,
                        "blockchain" => BITCOIN_BLOCKCHAIN,
                    )
                    .increment(1);
                }
            }

            let (tx_outputs, withdrawal_outputs) = tx_info.to_outputs(&signer_script_pubkeys)?;
            for output in tx_outputs {
                db.write_tx_output(&output).await?;
            }
            for output in withdrawal_outputs {
                db.write_withdrawal_tx_output(&output).await?;
            }
        }

        // Write these transactions into storage.
        db.write_bitcoin_transactions(sbtc_txs).await?;
        Ok(())
    };

    // The first time, we get all sweep transactions with inputs that
    // we know about. However, we could have locked the UTXO with a new
    // scriptPubKey, and we have no way of knowing that ahead of time.
    // The first pass over will populate the database with the new
    // scriptPubKeys.
    extract_fut().await?;

    // This will catch cases where the signers have locked up their
    // UTXO with a new scriptPubKey and there are a chain of
    // transactions in the block.
    extract_fut().await
}

/// Return the signing set that can make sBTC related contract calls along
/// with the current aggregate key to use for locking UTXOs on bitcoin.
///
/// The aggregate key fetched here is the one confirmed on the canonical
/// Stacks blockchain as part of a `rotate-keys` contract call. It will be
/// the public key that is the result of a DKG run. If there are no
/// rotate-keys transactions on the canonical stacks blockchain, then we
/// return None.
#[tracing::instrument(skip_all)]
pub async fn get_signer_set_info<C>(ctx: &C) -> Result<Option<SignerSetInfo>, Error>
where
    C: Context,
{
    let stacks = ctx.get_stacks_client();
    let address = &ctx.config().signer.deployer;
    // If the sBTC contracts have not been deployed, then we don't have any
    // signer set info in the registry.
    if !ctx.state().sbtc_contracts_deployed() {
        return Ok(None);
    }

    // This returns Ok(None) if API call returns a response with values
    // that are only set when we first deploy the sBTC contracts.
    stacks.get_current_signer_set_info(address).await
}

#[cfg(test)]
mod tests {
    use bitcoin::Amount;
    use bitcoin::BlockHash;
    use bitcoin::TxOut;
    use bitcoin::hashes::Hash as _;
    use fake::Dummy;
    use fake::Fake;
    use model::BitcoinTxId;
    use model::ScriptPubKey;
    use test_log::test;

    use crate::bitcoin::rpc::GetTxResponse;
    use crate::context::SignerSignal;
    use crate::keys::PublicKey;
    use crate::keys::SignerScriptPubKey as _;
    use crate::storage;
    use crate::storage::model::DkgSharesStatus;
    use crate::testing::block_observer::TestHarness;
    use crate::testing::context::*;
    use crate::testing::get_rng;

    use super::*;

    #[test(tokio::test)]
    async fn should_be_able_to_extract_bitcoin_blocks_given_a_block_header_stream() {
        let mut rng = get_rng();
        let storage = storage::memory::Store::new_shared();
        let test_harness = TestHarness::generate(&mut rng, 20, 0..5);
        let min_height = test_harness.min_block_height();
        let ctx = TestContext::builder()
            .with_storage(storage.clone())
            .with_stacks_client(test_harness.clone())
            .with_emily_client(test_harness.clone())
            .with_bitcoin_client(test_harness.clone())
            .modify_settings(|settings| settings.signer.sbtc_bitcoin_start_height = min_height)
            .build();

        // There must be at least one signal receiver alive when the block observer
        // later tries to send a signal, hence this line.
        let _signal_rx = ctx.get_signal_receiver();
        let block_hash_stream = test_harness.spawn_block_hash_stream();

        let block_observer = BlockObserver {
            context: ctx.clone(),
            bitcoin_blocks: block_hash_stream,
        };

        let handle = tokio::spawn(block_observer.run());
        ctx.wait_for_signal(Duration::from_secs(3), |signal| {
            matches!(
                signal,
                SignerSignal::Event(SignerEvent::BitcoinBlockObserved)
            )
        })
        .await
        .expect("block observer failed to complete within timeout");

        for block in test_harness.bitcoin_blocks() {
            let persisted = storage
                .get_bitcoin_block(&block.block_hash.into())
                .await
                .expect("storage error")
                .expect("block wasn't persisted");

            assert_eq!(persisted.block_hash, block.block_hash.into())
        }

        handle.abort();
    }

    /// Test that `BlockObserver::load_latest_deposit_requests` takes
    /// deposits from emily, validates them and only keeps the ones that
    /// pass validation and have been confirmed.
    #[tokio::test]
    async fn validated_confirmed_deposits_get_added_to_state() {
        let mut rng = get_rng();
        let mut test_harness = TestHarness::generate(&mut rng, 20, 0..5);
        // We want the test harness to fetch a block from our
        // "bitcoin-core", which in this case is the test harness. So we
        // use a block hash that the test harness knows about.
        let block_hash = test_harness
            .bitcoin_blocks()
            .first()
            .map(|block| block.block_hash);

        let lock_time = 150;
        let max_fee = 32000;
        let amount = 500_000;

        // We're going to create two deposit requests, the first one valid
        // and the second one invalid. Emily will return both the valid and
        // invalid requests (even though it should've validated them) and
        // BitcoinClient will return the right transaction for both of
        // them.
        let tx_setup0 = sbtc::testing::deposits::tx_setup(lock_time, max_fee, &[amount]);
        let deposit_request0 = CreateDepositRequest {
            outpoint: bitcoin::OutPoint {
                txid: tx_setup0.tx.compute_txid(),
                vout: 0,
            },
            deposit_script: tx_setup0.deposits.first().unwrap().deposit_script(),
            reclaim_script: tx_setup0.reclaims.first().unwrap().reclaim_script(),
        };
        let req0 = deposit_request0.clone();
        // When we validate the deposit request, we fetch the transaction
        // from bitcoin-core's blockchain. The stubs out that response.
        let get_tx_resp0 = GetTxResponse {
            tx: tx_setup0.tx.clone(),
            block_hash,
            confirmations: None,
            block_time: None,
        };

        let tx_setup1 = sbtc::testing::deposits::tx_setup(300, 2000, &[amount]);
        // This one is an invalid deposit request because the deposit
        // script is wrong
        let deposit_request1 = CreateDepositRequest {
            outpoint: bitcoin::OutPoint {
                txid: tx_setup1.tx.compute_txid(),
                vout: 0,
            },
            deposit_script: bitcoin::ScriptBuf::new(),
            reclaim_script: tx_setup1.reclaims.first().unwrap().reclaim_script(),
        };
        // The transaction is also in the mempool, even though it is an
        // invalid deposit.
        let get_tx_resp1 = GetTxResponse {
            tx: tx_setup1.tx.clone(),
            block_hash: None,
            confirmations: None,
            block_time: None,
        };

        // This deposit transaction is a fine deposit, it just hasn't been
        // confirmed yet.
        let tx_setup2 = sbtc::testing::deposits::tx_setup(400, 3000, &[amount]);
        let get_tx_resp2 = GetTxResponse {
            tx: tx_setup2.tx.clone(),
            block_hash: None,
            confirmations: None,
            block_time: None,
        };

        let deposit_request2 = CreateDepositRequest {
            outpoint: bitcoin::OutPoint {
                txid: tx_setup2.tx.compute_txid(),
                vout: 0,
            },
            deposit_script: tx_setup2.deposits.first().unwrap().deposit_script(),
            reclaim_script: tx_setup2.reclaims.first().unwrap().reclaim_script(),
        };

        // Let's add the "responses" to the field that feeds the
        // response to the `BitcoinClient::get_tx` call.
        test_harness.add_deposits(&[
            (get_tx_resp0.tx.compute_txid(), get_tx_resp0),
            (get_tx_resp1.tx.compute_txid(), get_tx_resp1),
            (get_tx_resp2.tx.compute_txid(), get_tx_resp2),
        ]);

        // Add the deposit requests to the pending deposits which
        // would be returned by Emily.
        test_harness.add_pending_deposits(&[deposit_request0, deposit_request1, deposit_request2]);
        let min_height = test_harness.min_block_height();

        // Now we finish setting up the block observer.
        let storage = storage::memory::Store::new_shared();
        let ctx = TestContext::builder()
            .with_storage(storage.clone())
            .with_stacks_client(test_harness.clone())
            .with_emily_client(test_harness.clone())
            .with_bitcoin_client(test_harness.clone())
            .modify_settings(|settings| settings.signer.sbtc_bitcoin_start_height = min_height)
            .build();

        let block_observer = BlockObserver {
            context: ctx,
            bitcoin_blocks: (),
        };

        {
            let db = storage.lock().await;
            assert_eq!(db.deposit_requests.len(), 0);
        }

        block_observer.load_latest_deposit_requests().await.unwrap();
        // Only the transaction from tx_setup0 was valid. Note that, since
        // we are not using a real block hash stored in the database. Our
        // DbRead function won't actually find it. And in prod we won't
        // actually store the deposit request transaction.
        let deposit = {
            let db = storage.lock().await;
            assert_eq!(db.deposit_requests.len(), 1);
            db.deposit_requests.values().next().cloned().unwrap()
        };

        assert_eq!(deposit.outpoint(), req0.outpoint);
    }

    /// Test that `BlockObserver::extract_deposit_requests` after
    /// `BlockObserver::load_latest_deposit_requests` stores validated
    /// deposit requests into "storage".
    #[tokio::test]
    async fn extract_deposit_requests_stores_validated_deposits() {
        let mut rng = get_rng();
        let mut test_harness = TestHarness::generate(&mut rng, 20, 0..5);

        // We want the test harness to fetch a block from our
        // "bitcoin-core", which in this case is the test harness. So we
        // use a block hash that the test harness knows about.
        let block_hash = test_harness
            .bitcoin_blocks()
            .first()
            .map(|block| block.block_hash);
        let lock_time = 150;
        let max_fee = 32000;
        let amount = 500_000;

        // We're going to create two deposit requests, the first one valid
        // and the second one invalid. Emily will return both the valid and
        // invalid requests (even though it should've validated them) and
        // BitcoinClient will return the right transaction for both of
        // them.
        let tx_setup0 = sbtc::testing::deposits::tx_setup(lock_time, max_fee, &[amount]);
        let deposit_request0 = CreateDepositRequest {
            outpoint: bitcoin::OutPoint {
                txid: tx_setup0.tx.compute_txid(),
                vout: 0,
            },
            deposit_script: tx_setup0.deposits.first().unwrap().deposit_script(),
            reclaim_script: tx_setup0.reclaims.first().unwrap().reclaim_script(),
        };
        // When we validate the deposit request, we fetch the transaction
        // from bitcoin-core's blockchain. The stubs out that
        // response.
        let get_tx_resp0 = GetTxResponse {
            tx: tx_setup0.tx.clone(),
            block_hash,
            confirmations: None,
            block_time: None,
        };

        // Let's add the "responses" to the field that feeds the
        // response to the `BitcoinClient::get_tx` call.
        test_harness.add_deposit(get_tx_resp0.tx.compute_txid(), get_tx_resp0);
        // Add the deposit request to the pending deposits which
        // would be returned by Emily.
        test_harness.add_pending_deposit(deposit_request0);

        let min_height = test_harness.min_block_height();
        // Now we finish setting up the block observer.
        let storage = storage::memory::Store::new_shared();
        let ctx = TestContext::builder()
            .with_storage(storage.clone())
            .with_stacks_client(test_harness.clone())
            .with_emily_client(test_harness.clone())
            .with_bitcoin_client(test_harness.clone())
            .modify_settings(|settings| settings.signer.sbtc_bitcoin_start_height = min_height)
            .build();

        let block_observer = BlockObserver {
            context: ctx,
            bitcoin_blocks: (),
        };

        block_observer.load_latest_deposit_requests().await.unwrap();

        let storage = storage.lock().await;
        assert_eq!(storage.deposit_requests.len(), 1);
        let db_outpoint: (BitcoinTxId, u32) = (tx_setup0.tx.compute_txid().into(), 0);
        assert!(storage.deposit_requests.contains_key(&db_outpoint));

        assert!(
            storage
                .bitcoin_transactions_to_blocks
                .contains_key(&db_outpoint.0)
        );
    }

    /// Test that `BlockObserver::extract_sbtc_transactions` takes the
    /// stored signer `scriptPubKey`s and stores all transactions from a
    /// bitcoin block that match one of those `scriptPubkey`s.
    #[tokio::test]
    async fn sbtc_transactions_get_stored() {
        let mut rng = get_rng();
        let mut test_harness = TestHarness::generate(&mut rng, 20, 0..5);

        let block_hash = BlockHash::from_byte_array([1u8; 32]);
        // We're going to do the following:
        // 1. pretend that the below bytes represent the signers
        //    `scriptPubKey`. We store it in our datastore along with some
        //    "DKG shares".
        // 2. We then create two transactions, one spending to our
        //    scriptPubKey and another not spending to it.
        // 3. We try "extracting" a block with one transaction that does
        //    not spend to the signers. This one transaction should not be
        //    extracted (we should not see it in storage).
        // 4. We try "extracting" a block with two transactions where one
        //    of them spends to the signers. The one transaction should be
        //    stored in our storage.
        let signers_script_pubkey: ScriptPubKey = fake::Faker.fake_with_rng(&mut rng);

        // We start by storing our `scriptPubKey`.
        let storage = storage::memory::Store::new_shared();
        let aggregate_key = PublicKey::dummy_with_rng(&fake::Faker, &mut rng);
        let shares = model::EncryptedDkgShares {
            aggregate_key,
            tweaked_aggregate_key: aggregate_key.signers_tweaked_pubkey().unwrap(),
            script_pubkey: signers_script_pubkey.clone(),
            encrypted_private_shares: Vec::new(),
            public_shares: Vec::new(),
            signer_set_public_keys: vec![aggregate_key],
            signature_share_threshold: 1,
            dkg_shares_status: DkgSharesStatus::Unverified,
            started_at_bitcoin_block_hash: block_hash.into(),
            started_at_bitcoin_block_height: 1u64.into(),
        };
        storage.write_encrypted_dkg_shares(&shares).await.unwrap();

        // Now let's create two transactions, one spending to the signers
        // and another not spending to the signers. We use
        // sbtc::testing::deposits::tx_setup just to quickly create a
        // transaction; any one will do since we will be adding the UTXO
        // that spends to the signer afterward.
        let mut tx_setup0 = sbtc::testing::deposits::tx_setup(0, 0, &[100]);
        tx_setup0.tx.output.push(TxOut {
            value: Amount::ONE_BTC,
            script_pubkey: signers_script_pubkey.into(),
        });

        // This one does not spend to the signers :(
        let tx_setup1 = sbtc::testing::deposits::tx_setup(1, 10, &[2000]);
        let txid0 = tx_setup0.tx.compute_txid();
        let txid1 = tx_setup1.tx.compute_txid();

        let response0 = GetTxResponse {
            tx: tx_setup0.tx.clone(),
            block_hash: Some(block_hash),
            confirmations: None,
            block_time: None,
        };
        let response1 = GetTxResponse {
            tx: tx_setup1.tx.clone(),
            block_hash: Some(block_hash),
            confirmations: None,
            block_time: None,
        };
        test_harness.add_deposit(txid0, response0);
        test_harness.add_deposit(txid1, response1);

        // First we try extracting the transactions from a block that does
        // not contain any transactions spent to the signers
        let txs = [tx_setup1.tx.fake_with_rng(&mut rng)];
        extract_sbtc_transactions(&storage, None, block_hash, &txs)
            .await
            .unwrap();

        // We need to change the scope so that the mutex guard is dropped.
        {
            let store = storage.lock().await;
            // Under the hood, bitcoin transactions get stored in the
            // `bitcoin_block_to_transactions` field, so lets check there
            let stored_transactions = store.bitcoin_block_to_transactions.get(&block_hash.into());

            // Nothing should be stored so the map get call should return
            // None.
            assert!(stored_transactions.is_none());
        }

        // Now we try again, but we include the transaction that spends to
        // the signer. This one should turn out differently.
        let txs = [
            tx_setup0.tx.fake_with_rng(&mut rng),
            tx_setup1.tx.fake_with_rng(&mut rng),
        ];
        extract_sbtc_transactions(&storage, None, block_hash, &txs)
            .await
            .unwrap();

        let store = storage.lock().await;
        let stored_transactions = store.bitcoin_block_to_transactions.get(&block_hash.into());

        // Is our one transaction stored? This block hash should now have
        // only one transaction with the expected txid.
        let tx_ids = stored_transactions.unwrap();
        let expected_tx_id = tx_setup0.tx.compute_txid().into();
        assert_eq!(tx_ids.len(), 1);
        assert!(tx_ids.contains(&expected_tx_id));
    }
}
