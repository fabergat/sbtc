//! Postgres storage implementation.

use std::collections::BTreeSet;

use bitcoin::OutPoint;
use bitcoin::hashes::Hash as _;
use blockstack_lib::types::chainstate::StacksBlockId;
use sqlx::Executor as _;
use sqlx::PgExecutor;
use sqlx::postgres::PgPoolOptions;

use crate::bitcoin::utxo::SignerUtxo;
use crate::bitcoin::validation::DepositConfirmationStatus;
use crate::bitcoin::validation::DepositRequestReport;
use crate::bitcoin::validation::WithdrawalRequestReport;
use crate::bitcoin::validation::WithdrawalRequestStatus;
use crate::error::Error;
use crate::keys::PublicKey;
use crate::keys::PublicKeyXOnly;
use crate::storage::model;
use crate::storage::model::BitcoinBlockHeight;
use crate::storage::model::CompletedDepositEvent;
use crate::storage::model::StacksBlockHeight;
use crate::storage::model::WithdrawalAcceptEvent;
use crate::storage::model::WithdrawalRejectEvent;

use crate::DEPOSIT_LOCKTIME_BLOCK_BUFFER;
use crate::MAX_MEMPOOL_PACKAGE_TX_COUNT;
use crate::MAX_REORG_BLOCK_COUNT;
use crate::WITHDRAWAL_BLOCKS_EXPIRY;

/// All migration scripts from the `signer/migrations` directory.
static PGSQL_MIGRATIONS: include_dir::Dir =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/migrations");

/// A convenience struct for retrieving a deposit request report
#[derive(sqlx::FromRow)]
struct DepositStatusSummary {
    /// The current signer may not have a record of their vote for
    /// the deposit. When that happens the `can_accept` and
    /// `can_sign` fields will be None.
    can_accept: Option<bool>,
    /// Whether this signer is a member of the signing set that generated
    /// the public key locking the deposit.
    can_sign: Option<bool>,
    /// The height of the block that confirmed the deposit request
    /// transaction.
    block_height: Option<BitcoinBlockHeight>,
    /// The block hash that confirmed the deposit request.
    block_hash: Option<model::BitcoinBlockHash>,
    /// The bitcoin consensus encoded locktime in the reclaim script.
    #[sqlx(try_from = "i64")]
    lock_time: u32,
    /// The amount associated with the deposit UTXO in sats.
    #[sqlx(try_from = "i64")]
    amount: u64,
    /// The maximum amount to spend for the bitcoin miner fee when sweeping
    /// in the funds.
    #[sqlx(try_from = "i64")]
    max_fee: u64,
    /// The deposit script used so that the signers' can spend funds.
    deposit_script: model::ScriptPubKey,
    /// The reclaim script for the deposit.
    reclaim_script: model::ScriptPubKey,
    /// The public key used in the deposit script.
    signers_public_key: PublicKeyXOnly,
}

/// A convenience struct for retrieving a withdrawal request report
#[derive(sqlx::FromRow)]
struct WithdrawalStatusSummary {
    /// The current signer may not have a record of their vote for the
    /// withdrawal. When that happens the `is_accepted` field will be
    /// [`None`].
    is_accepted: Option<bool>,
    /// The height of the bitcoin chain tip during the execution of the
    /// contract call that generated the withdrawal request.
    bitcoin_block_height: BitcoinBlockHeight,
    /// The amount associated with the deposit UTXO in sats.
    #[sqlx(try_from = "i64")]
    amount: u64,
    /// The maximum amount to spend for the bitcoin miner fee when sweeping
    /// in the funds.
    #[sqlx(try_from = "i64")]
    max_fee: u64,
    /// The recipient scriptPubKey of the withdrawn funds.
    recipient: model::ScriptPubKey,
    /// Stacks block ID of the block that includes the transaction
    /// associated with this withdrawal request.
    stacks_block_hash: model::StacksBlockHash,
    /// Stacks block ID of the block that includes the transaction
    /// associated with this withdrawal request.
    stacks_block_height: StacksBlockHeight,
}

// A convenience struct for retrieving the signers' UTXO
#[derive(sqlx::FromRow)]
struct PgSignerUtxo {
    txid: model::BitcoinTxId,
    #[sqlx(try_from = "i32")]
    output_index: u32,
    #[sqlx(try_from = "i64")]
    amount: u64,
    aggregate_key: PublicKey,
}

impl From<PgSignerUtxo> for SignerUtxo {
    fn from(pg_txo: PgSignerUtxo) -> Self {
        SignerUtxo {
            outpoint: OutPoint::new(pg_txo.txid.into(), pg_txo.output_index),
            amount: pg_txo.amount,
            public_key: pg_txo.aggregate_key.into(),
        }
    }
}

/// A wrapper around a [`sqlx::PgPool`] which implements
/// [`crate::storage::DbRead`] and [`crate::storage::DbWrite`].
#[derive(Debug, Clone)]
pub struct PgStore(sqlx::PgPool);

impl PgStore {
    /// Connect to the Postgres database at `url`.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let pool = PgPoolOptions::new()
            .after_connect(|conn, _meta| Box::pin(async move {
                conn.execute("SET application_name = 'sbtc-signer'; SET search_path = sbtc_signer,public;")
                    .await?;
                Ok(())
            }))
            .connect(url)
            .await
            .map_err(Error::SqlxConnect)?;

        Ok(Self(pool))
    }

    /// Apply the migrations to the database.
    pub async fn apply_migrations(&self) -> Result<(), Error> {
        // Related to https://github.com/stacks-network/sbtc/issues/411
        // TODO(537) - Revisit this prior to public launch
        //
        // Note 1: This could be generalized and moved up to the `storage` module, but
        // left that for a future exercise if we need to support other databases.
        //
        // Note 2: The `sqlx` "migration" feature results in dependency conflicts
        // with sqlite from the clarity crate.
        //
        // Note 3: The migration code paths have no explicit integration tests, but are
        // implicitly tested by all integration tests using `new_test_database()`.
        tracing::info!("Preparing to run database migrations");

        sqlx::raw_sql(
            r#"
                CREATE TABLE IF NOT EXISTS public.__sbtc_migrations (
                    key TEXT PRIMARY KEY
                );
            "#,
        )
        .execute(&self.0)
        .await
        .map_err(Error::SqlxMigrate)?;

        let mut trx = self
            .pool()
            .begin()
            .await
            .map_err(Error::SqlxBeginTransaction)?;

        // Collect all migration scripts and sort them by filename. It is important
        // that the migration scripts are named in a way that they are executed in
        // the correct order, i.e. the current naming of `0001__`, `0002__`, etc.
        let mut migrations = PGSQL_MIGRATIONS.files().collect::<Vec<_>>();
        migrations.sort_by_key(|file| file.path().file_name());
        for migration in migrations {
            let key = migration
                .path()
                .file_name()
                .expect("failed to get filename from migration script path")
                .to_string_lossy();

            // Just in-case we end up with a README.md or some other non-SQL file
            // in the migrations directory.
            if !key.ends_with(".sql") {
                tracing::debug!(migration = %key, "Skipping non-SQL migration file");
            }

            // Check if the migration has already been applied. If so, we should
            // be able to safely skip it.
            if self.check_migration_existence(&mut *trx, &key).await? {
                tracing::debug!(migration = %key, "Database migration already applied");
                continue;
            }

            // Attempt to apply the migration. If we encounter an error, we abort
            // the entire migration process.
            if let Some(script) = migration.contents_utf8() {
                tracing::info!(migration = %key, "Applying database migration");

                // Execute the migration.
                sqlx::raw_sql(script)
                    .execute(&mut *trx)
                    .await
                    .map_err(Error::SqlxMigrate)?;

                // Save the migration as applied.
                self.insert_migration(&key).await?;
            } else {
                // The trx should be rolled back on drop but let's be explicit.
                trx.rollback()
                    .await
                    .map_err(Error::SqlxRollbackTransaction)?;

                // We failed to read the migration script as valid UTF-8. This
                // shouldn't happen since it's our own migration scripts, but
                // just in case...
                return Err(Error::ReadSqlMigration(
                    migration.path().as_os_str().to_string_lossy(),
                ));
            }
        }

        trx.commit().await.map_err(Error::SqlxCommitTransaction)?;

        Ok(())
    }

    /// Check if a migration with the given `key` exists.
    async fn check_migration_existence(
        &self,
        executor: impl PgExecutor<'_>,
        key: &str,
    ) -> Result<bool, Error> {
        let result = sqlx::query_scalar::<_, i64>(
            // Note: db_name + key are PK so we can only get max 1 row.
            r#"
            SELECT COUNT(*) FROM public.__sbtc_migrations
                WHERE
                    key = $1
            ;
            "#,
        )
        .bind(key)
        .fetch_one(executor)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(result > 0)
    }

    /// Insert a migration with the given `key`.
    async fn insert_migration(&self, key: &str) -> Result<(), Error> {
        sqlx::query(
            r#"
            INSERT INTO public.__sbtc_migrations (key)
                VALUES ($1)
            "#,
        )
        .bind(key)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    /// Get a reference to the underlying pool.
    pub fn pool(&self) -> &sqlx::PgPool {
        &self.0
    }

    async fn get_utxo(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        output_type: model::TxOutputType,
        min_block_height: BitcoinBlockHeight,
    ) -> Result<Option<SignerUtxo>, Error> {
        let pg_utxo = sqlx::query_as::<_, PgSignerUtxo>(
            r#"
            WITH bitcoin_blockchain AS (
                SELECT block_hash
                FROM bitcoin_blockchain_until($1, $2)
            ),
            confirmed_sweeps AS (
                SELECT
                    prevout_txid
                  , prevout_output_index
                FROM sbtc_signer.bitcoin_tx_inputs
                JOIN sbtc_signer.bitcoin_transactions AS bt USING (txid)
                JOIN bitcoin_blockchain AS bb USING (block_hash)
                WHERE prevout_type = 'signers_input'
            )
            SELECT
                bo.txid
              , bo.output_index
              , bo.amount
              , ds.aggregate_key
            FROM sbtc_signer.bitcoin_tx_outputs AS bo
            JOIN sbtc_signer.bitcoin_transactions AS bt USING (txid)
            JOIN bitcoin_blockchain AS bb USING (block_hash)
            JOIN sbtc_signer.dkg_shares AS ds USING (script_pubkey)
            LEFT JOIN confirmed_sweeps AS cs
              ON cs.prevout_txid = bo.txid
              AND cs.prevout_output_index = bo.output_index
            WHERE cs.prevout_txid IS NULL
              AND bo.output_type = $3
            ORDER BY bo.amount DESC
            LIMIT 1;
            "#,
        )
        .bind(chain_tip)
        .bind(i64::try_from(min_block_height).map_err(Error::ConversionDatabaseInt)?)
        .bind(output_type)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(pg_utxo.map(SignerUtxo::from))
    }

    /// This function returns the bitcoin block height of the first
    /// confirmed sweep that happened on or after the given minimum block
    /// height.
    async fn get_least_txo_height(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        min_block_height: BitcoinBlockHeight,
    ) -> Result<Option<BitcoinBlockHeight>, Error> {
        sqlx::query_scalar::<_, BitcoinBlockHeight>(
            r#"
            SELECT bb.block_height
            FROM sbtc_signer.bitcoin_tx_inputs AS bi
            JOIN sbtc_signer.bitcoin_tx_outputs AS bo
              ON bo.txid = bi.txid
            JOIN sbtc_signer.bitcoin_transactions AS bt
              ON bt.txid = bi.txid
            JOIN bitcoin_blockchain_until($1, $2) AS bb
              ON bb.block_hash = bt.block_hash
            WHERE bo.output_type = 'signers_output'
              AND bi.prevout_type = 'signers_input'
            ORDER BY bb.block_height ASC
            LIMIT 1;
            "#,
        )
        .bind(chain_tip)
        .bind(i64::try_from(min_block_height).map_err(Error::ConversionDatabaseInt)?)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    /// Return the height of the earliest block in which a donation UTXO
    /// has been confirmed.
    ///
    /// # Notes
    ///
    /// This function does not check whether the donation output has been
    /// spent.
    pub async fn minimum_donation_txo_height(&self) -> Result<Option<BitcoinBlockHeight>, Error> {
        sqlx::query_scalar::<_, BitcoinBlockHeight>(
            r#"
            SELECT bb.block_height
            FROM sbtc_signer.bitcoin_tx_outputs AS bo
            JOIN sbtc_signer.bitcoin_transactions AS bt USING (txid)
            JOIN sbtc_signer.bitcoin_blocks AS bb USING (block_hash)
            WHERE bo.output_type = 'donation'
            ORDER BY bb.block_height ASC
            LIMIT 1;
            "#,
        )
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    /// Return a donation UTXO with minimum height.
    pub async fn get_donation_utxo(
        &self,
        chain_tip: &model::BitcoinBlockHash,
    ) -> Result<Option<SignerUtxo>, Error> {
        let Some(min_block_height) = self.minimum_donation_txo_height().await? else {
            return Ok(None);
        };
        let output_type = model::TxOutputType::Donation;
        self.get_utxo(chain_tip, output_type, min_block_height)
            .await
    }
    /// Return a block height that is less than or equal to the block that
    /// confirms the signers' UTXO.
    ///
    /// # Notes
    ///
    /// * This function only returns `Ok(None)` if there have been no
    ///   confirmed sweep transactions.
    /// * As the signers sweep funds between BTC and sBTC, they leave a
    ///   chain of transactions, where each transaction spends the signers'
    ///   sole UTXO and creates a new one. This function "crawls" the chain
    ///   of transactions, starting at the most recently confirmed one,
    ///   until it goes back at least [`MAX_REORG_BLOCK_COUNT`] blocks
    ///   worth of transactions. A block with height greater than or equal
    ///   to the height returned here should contain the transaction with
    ///   the signers' UTXO, and won't if there is a reorg spanning more
    ///   than [`MAX_REORG_BLOCK_COUNT`] blocks.
    pub async fn minimum_utxo_height(&self) -> Result<Option<BitcoinBlockHeight>, Error> {
        #[derive(sqlx::FromRow)]
        struct PgCandidateUtxo {
            txid: model::BitcoinTxId,
            block_height: BitcoinBlockHeight,
        }

        // Get the block height of the unspent transaction that was most
        // recently confirmed. Note that we are not filtering by the
        // blockchain identified by a chain tip, we just want the UTXO with
        // maximum height, even if it has been reorged.
        let utxo_candidate = sqlx::query_as::<_, PgCandidateUtxo>(
            r#"
            WITH confirmed_sweeps AS (
                SELECT
                    prevout_txid
                  , prevout_output_index
                FROM sbtc_signer.bitcoin_tx_inputs
                JOIN sbtc_signer.bitcoin_transactions AS bt USING (txid)
                WHERE prevout_type = 'signers_input'
            )
            SELECT
                bo.txid
              , bb.block_height
            FROM sbtc_signer.bitcoin_tx_outputs AS bo
            JOIN sbtc_signer.bitcoin_transactions AS bt USING (txid)
            JOIN sbtc_signer.bitcoin_blocks AS bb USING (block_hash)
            LEFT JOIN confirmed_sweeps AS cs
              ON cs.prevout_txid = bo.txid
              AND cs.prevout_output_index = bo.output_index
            WHERE cs.prevout_txid IS NULL
              AND bo.output_type = 'signers_output'
            ORDER BY bb.block_height DESC
            LIMIT 1;
            "#,
        )
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        // If such a UTXO candidate doesn't exist then we know that there is no
        // UTXO at all the given transaction output type.
        let Some(utxo_candidate) = utxo_candidate else {
            return Ok(None);
        };

        // Now we want the max block height[1] of all sweep transactions
        // that occurred more than MAX_REORG_BLOCK_COUNT blocks ago, because
        // this sweep transaction is considered fully confirmed.
        //
        // [1]: The sweep transaction that occurred more than
        //      MAX_REORG_BLOCK_COUNT blocks ago may have been confirmed
        //      more than once. If this is the case, we want the min height
        //      of all of them.

        // Given the utxo candidate above, this is our best guess of the
        // minimum UTXO height. It might be wrong, we'll find out shortly.
        let min_block_height_candidate = utxo_candidate
            .block_height
            .saturating_sub(MAX_REORG_BLOCK_COUNT);

        // We want to go back at least MAX_REORG_BLOCK_COUNT blocks worth
        // of transactions. The number here is the maximum number of
        // transactions that the signers could get confirmed in
        // MAX_REORG_BLOCK_COUNT bitcoin blocks, plus one. We add the one
        // because we want the transaction right after
        // MAX_REORG_BLOCK_COUNT worth of transactions.
        let max_transactions =
            i64::try_from(MAX_MEMPOOL_PACKAGE_TX_COUNT * MAX_REORG_BLOCK_COUNT + 1)
                .map_err(Error::ConversionDatabaseInt)?;

        // Find the block height of the sweep transaction that occurred at
        // or before block "best candidate block height" minus
        // MAX_REORG_BLOCK_COUNT.
        //
        // We do this because the block that confirmed the UTXO with max
        // height need not be the signers' UTXO; it does not need to be on
        // the best blockchain. But if we go back at least
        // `MAX_REORG_BLOCK_COUNT` bitcoin blocks then that UTXO is assumed
        // to still be confirmed.
        let prev_confirmed_height_candidate = sqlx::query_scalar::<_, BitcoinBlockHeight>(
            r#"
            WITH RECURSIVE signer_inputs AS (
                SELECT
                    bti.txid
                  , bti.prevout_txid
                  , MIN(bb.block_height) AS block_height
                FROM sbtc_signer.bitcoin_tx_inputs AS bti
                JOIN sbtc_signer.bitcoin_transactions USING (txid)
                JOIN sbtc_signer.bitcoin_blocks AS bb USING (block_hash)
                WHERE bti.prevout_type = 'signers_input'
                  AND bb.block_height <= $1
                GROUP BY bti.txid, bti.prevout_txid
            ),
            tx_chain AS (
                SELECT
                    si.txid
                  , si.prevout_txid
                  , si.block_height
                  , 1 AS tx_count
                FROM signer_inputs AS si
                WHERE si.txid = $3

                UNION ALL

                SELECT
                    si.txid
                  , si.prevout_txid
                  , si.block_height
                  , tc.tx_count + 1
                FROM signer_inputs AS si
                JOIN tx_chain AS tc
                  ON tc.prevout_txid = si.txid
                WHERE tc.tx_count < $2
            )
            SELECT block_height
            FROM tx_chain
            WHERE block_height <= $4
            ORDER BY block_height DESC
            LIMIT 1;
            "#,
        )
        .bind(utxo_candidate.block_height)
        .bind(max_transactions)
        .bind(utxo_candidate.txid)
        .bind(min_block_height_candidate)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        // We need to go back at least MAX_REORG_BLOCK_COUNT blocks before
        // the confirmation height of our best candidate height. If there
        // were no sweeps at least MAX_REORG_BLOCK_COUNT blocks ago, then
        // we can use min_block_height_candidate.
        let min_block_height =
            prev_confirmed_height_candidate.unwrap_or(min_block_height_candidate);

        Ok(Some(min_block_height))
    }

    /// Return the least height for which the deposit request was confirmed
    /// on a bitcoin blockchain.
    ///
    /// Transactions can be confirmed on more than one blockchain and this
    /// function returns the least height out of all bitcoin blocks for
    /// which the deposit has been confirmed.
    ///
    /// None is returned if we do not have a record of the deposit request.
    pub async fn get_deposit_request_least_height(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
    ) -> Result<Option<BitcoinBlockHeight>, Error> {
        // Before the deposit request is written a signer also stores the
        // bitcoin transaction and (after #731) the bitcoin block
        // confirming the deposit to the database. So this will return zero
        // rows only when we cannot find the deposit request.
        sqlx::query_scalar::<_, BitcoinBlockHeight>(
            r#"
            SELECT block_height
            FROM sbtc_signer.deposit_requests AS dr
            JOIN sbtc_signer.bitcoin_transactions USING (txid)
            JOIN sbtc_signer.bitcoin_blocks USING (block_hash)
            WHERE dr.txid = $1
              AND dr.output_index = $2
            ORDER BY block_height
            LIMIT 1
            "#,
        )
        .bind(txid)
        .bind(i32::try_from(output_index).map_err(Error::ConversionDatabaseInt)?)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    /// Return the txid of the bitcoin transaction that swept in the
    /// deposit UTXO. The sweep transaction must be confirmed on the
    /// blockchain identified by the given chain tip.
    ///
    /// This query only looks back at transactions that are confirmed at or
    /// after the given `min_block_height`.
    async fn get_deposit_sweep_txid(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        txid: &model::BitcoinTxId,
        output_index: u32,
        min_block_height: BitcoinBlockHeight,
    ) -> Result<Option<model::BitcoinTxId>, Error> {
        sqlx::query_scalar::<_, model::BitcoinTxId>(
            r#"
            SELECT bti.txid
            FROM sbtc_signer.bitcoin_tx_inputs AS bti
            JOIN sbtc_signer.bitcoin_transactions AS bt USING (txid)
            JOIN sbtc_signer.bitcoin_blockchain_until($1, $2) USING (block_hash)
            WHERE bti.prevout_txid = $3
              AND bti.prevout_output_index = $4
            LIMIT 1
            "#,
        )
        .bind(chain_tip)
        .bind(i64::try_from(min_block_height).map_err(Error::ConversionDatabaseInt)?)
        .bind(txid)
        .bind(i32::try_from(output_index).map_err(Error::ConversionDatabaseInt)?)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    /// Fetch a status summary of a deposit request.
    ///
    /// In this query we list out the blockchain identified by the chain
    /// tip as far back as necessary. We then check if this signer accepted
    /// the deposit request, and whether it was confirmed on the blockchain
    /// that we just listed out.
    ///
    /// `None` is returned if no deposit request is in the database (we
    /// always write the associated transaction to the database for each
    /// deposit so that cannot be the reason for why the query here returns
    /// `None`).
    async fn get_deposit_request_status_summary(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        txid: &model::BitcoinTxId,
        output_index: u32,
        signer_public_key: &PublicKey,
    ) -> Result<Option<DepositStatusSummary>, Error> {
        // We first get the least height for when the deposit request was
        // confirmed. This height serves as the stopping criteria for the
        // recursive part of the subsequent query.
        let min_block_height_fut = self.get_deposit_request_least_height(txid, output_index);
        // None is only returned if we do not have a record of the deposit
        // request or the deposit transaction.
        let Some(min_block_height) = min_block_height_fut.await? else {
            return Ok(None);
        };
        sqlx::query_as::<_, DepositStatusSummary>(
            r#"
            SELECT
                ds.can_accept
              , ds.can_sign
              , dr.amount
              , dr.max_fee
              , dr.lock_time
              , dr.spend_script AS deposit_script
              , dr.reclaim_script
              , dr.signers_public_key
              , bc.block_height
              , bc.block_hash
            FROM sbtc_signer.deposit_requests AS dr
            JOIN sbtc_signer.bitcoin_transactions USING (txid)
            LEFT JOIN sbtc_signer.bitcoin_blockchain_until($1, $2) AS bc USING (block_hash)
            LEFT JOIN sbtc_signer.deposit_signers AS ds
              ON dr.txid = ds.txid
             AND dr.output_index = ds.output_index
             AND ds.signer_pub_key = $5
            WHERE dr.txid = $3
              AND dr.output_index = $4
            LIMIT 1
            "#,
        )
        .bind(chain_tip)
        .bind(min_block_height)
        .bind(txid)
        .bind(i32::try_from(output_index).map_err(Error::ConversionDatabaseInt)?)
        .bind(signer_public_key)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    /// Check whether the given block hash is a part of the stacks
    /// blockchain identified by the given chain-tip.
    pub async fn in_canonical_stacks_blockchain(
        &self,
        chain_tip: &model::StacksBlockHash,
        block_hash: &model::StacksBlockHash,
        block_height: StacksBlockHeight,
    ) -> Result<bool, Error> {
        sqlx::query_scalar::<_, bool>(
            r#"
            WITH RECURSIVE tx_block_chain AS (
                SELECT
                    block_hash
                  , block_height
                  , parent_hash
                FROM sbtc_signer.stacks_blocks
                WHERE block_hash = $1

                UNION ALL

                SELECT
                    parent.block_hash
                  , parent.block_height
                  , parent.parent_hash
                FROM sbtc_signer.stacks_blocks AS parent
                JOIN tx_block_chain AS child
                  ON parent.block_hash = child.parent_hash
                WHERE child.block_height > $2
            )
            SELECT EXISTS (
                SELECT TRUE
                FROM tx_block_chain AS tbc
                WHERE tbc.block_hash = $3
            );
        "#,
        )
        .bind(chain_tip)
        .bind(i64::try_from(block_height).map_err(Error::ConversionDatabaseInt)?)
        .bind(block_hash)
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    /// Fetch a status summary of a withdrawal request.
    ///
    /// In this query we fetch the raw withdrawal request and add some
    /// information about whether this signer accepted the request.
    ///
    /// `None` is returned if withdrawal request is not in the database or
    /// if the withdrawal request is not associated with a stacks block in
    /// the database.
    async fn get_withdrawal_request_status_summary(
        &self,
        id: &model::QualifiedRequestId,
        signer_public_key: &PublicKey,
    ) -> Result<Option<WithdrawalStatusSummary>, Error> {
        sqlx::query_as::<_, WithdrawalStatusSummary>(
            r#"
            SELECT
                ws.is_accepted
              , wr.amount
              , wr.max_fee
              , wr.recipient
              , wr.bitcoin_block_height
              , wr.block_hash   AS stacks_block_hash
              , sb.block_height AS stacks_block_height
            FROM sbtc_signer.withdrawal_requests AS wr
            JOIN sbtc_signer.stacks_blocks AS sb
              ON sb.block_hash = wr.block_hash
            LEFT JOIN sbtc_signer.withdrawal_signers AS ws
              ON ws.request_id = wr.request_id
             AND ws.block_hash = wr.block_hash
             AND ws.signer_pub_key = $1
            WHERE wr.request_id = $2
              AND wr.block_hash = $3
            LIMIT 1
            "#,
        )
        .bind(signer_public_key)
        .bind(i64::try_from(id.request_id).map_err(Error::ConversionDatabaseInt)?)
        .bind(id.block_hash)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    /// Fetch the bitcoin transaction ID that swept the withdrawal along
    /// with the block hash that confirmed the transaction.
    ///
    /// `None` is returned if there is no transaction sweeping out the
    /// funds that has been confirmed on the blockchain identified by the
    /// given chain-tip.
    async fn get_withdrawal_sweep_info(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        id: &model::QualifiedRequestId,
    ) -> Result<Option<model::BitcoinTxRef>, Error> {
        sqlx::query_as::<_, model::BitcoinTxRef>(
            r#"
            SELECT
                bwo.bitcoin_txid AS txid
              , bt.block_hash
            FROM sbtc_signer.withdrawal_requests AS wr
            JOIN sbtc_signer.bitcoin_withdrawals_outputs AS bwo
              ON bwo.request_id = wr.request_id
             AND bwo.stacks_block_hash = wr.block_hash
            JOIN sbtc_signer.bitcoin_transactions AS bt
              ON bt.txid = bwo.bitcoin_txid
            JOIN sbtc_signer.bitcoin_blockchain_until($1, wr.bitcoin_block_height) AS bbu
              ON bbu.block_hash = bt.block_hash
            WHERE wr.request_id = $2
              AND wr.block_hash = $3
            LIMIT 1
            "#,
        )
        .bind(chain_tip)
        .bind(i64::try_from(id.request_id).map_err(Error::ConversionDatabaseInt)?)
        .bind(id.block_hash)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }
}

impl From<sqlx::PgPool> for PgStore {
    fn from(value: sqlx::PgPool) -> Self {
        Self(value)
    }
}

impl super::DbRead for PgStore {
    async fn get_bitcoin_block(
        &self,
        block_hash: &model::BitcoinBlockHash,
    ) -> Result<Option<model::BitcoinBlock>, Error> {
        sqlx::query_as::<_, model::BitcoinBlock>(
            "SELECT
                block_hash
              , block_height
              , parent_hash
            FROM sbtc_signer.bitcoin_blocks
            WHERE block_hash = $1;",
        )
        .bind(block_hash)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_stacks_block(
        &self,
        block_hash: &model::StacksBlockHash,
    ) -> Result<Option<model::StacksBlock>, Error> {
        sqlx::query_as::<_, model::StacksBlock>(
            "SELECT
                block_hash
              , block_height
              , parent_hash
              , bitcoin_anchor
            FROM sbtc_signer.stacks_blocks
            WHERE block_hash = $1;",
        )
        .bind(block_hash)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_bitcoin_canonical_chain_tip(
        &self,
    ) -> Result<Option<model::BitcoinBlockHash>, Error> {
        sqlx::query_as::<_, model::BitcoinBlock>(
            "SELECT
                block_hash
              , block_height
              , parent_hash
             FROM sbtc_signer.bitcoin_blocks
             ORDER BY block_height DESC, block_hash DESC
             LIMIT 1",
        )
        .fetch_optional(&self.0)
        .await
        .map(|maybe_block| maybe_block.map(|block| block.block_hash))
        .map_err(Error::SqlxQuery)
    }

    async fn get_bitcoin_canonical_chain_tip_ref(
        &self,
    ) -> Result<Option<model::BitcoinBlockRef>, Error> {
        sqlx::query_as::<_, model::BitcoinBlockRef>(
            "SELECT
                block_hash
              , block_height
             FROM sbtc_signer.bitcoin_blocks
             ORDER BY block_height DESC, block_hash DESC
             LIMIT 1",
        )
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_stacks_chain_tip(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
    ) -> Result<Option<model::StacksBlock>, Error> {
        // TODO: stop recursion after the first bitcoin block having stacks block anchored?
        // Note that in tests generated data we may get a taller stacks chain anchored to a
        // bitcoin block that may not be the first one we encounter having stacks block anchored
        sqlx::query_as::<_, model::StacksBlock>(
            r#"
            WITH RECURSIVE context_window AS (
                SELECT
                    block_hash
                  , block_height
                  , parent_hash
                FROM sbtc_signer.bitcoin_blocks
                WHERE block_hash = $1

                UNION ALL

                SELECT
                    parent.block_hash
                  , parent.block_height
                  , parent.parent_hash
                FROM sbtc_signer.bitcoin_blocks AS parent
                JOIN context_window AS child
                  ON parent.block_hash = child.parent_hash
            )
            SELECT
                stacks_blocks.block_hash
              , stacks_blocks.block_height
              , stacks_blocks.parent_hash
              , stacks_blocks.bitcoin_anchor
            FROM context_window bitcoin_blocks
            JOIN sbtc_signer.stacks_blocks stacks_blocks
                ON bitcoin_blocks.block_hash = stacks_blocks.bitcoin_anchor
            ORDER BY block_height DESC, block_hash DESC
            LIMIT 1;
            "#,
        )
        .bind(bitcoin_chain_tip)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_pending_deposit_requests(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
        signer_public_key: &PublicKey,
    ) -> Result<Vec<model::DepositRequest>, Error> {
        sqlx::query_as::<_, model::DepositRequest>(
            r#"
            WITH RECURSIVE context_window AS (
                -- Anchor member: Initialize the recursion with the chain tip
                SELECT block_hash, block_height, parent_hash, created_at, 1 AS depth
                FROM sbtc_signer.bitcoin_blocks
                WHERE block_hash = $1

                UNION ALL

                -- Recursive member: Fetch the parent block using the last block's parent_hash
                SELECT parent.block_hash, parent.block_height, parent.parent_hash,
                       parent.created_at, last.depth + 1
                FROM sbtc_signer.bitcoin_blocks parent
                JOIN context_window last ON parent.block_hash = last.parent_hash
                WHERE last.depth < $2
            ),
            transactions_in_window AS (
                SELECT transactions.txid
                FROM context_window blocks_in_window
                JOIN sbtc_signer.bitcoin_transactions transactions ON
                    transactions.block_hash = blocks_in_window.block_hash
            )
            SELECT
                deposit_requests.txid
              , deposit_requests.output_index
              , deposit_requests.spend_script
              , deposit_requests.reclaim_script
              , deposit_requests.recipient
              , deposit_requests.amount
              , deposit_requests.max_fee
              , deposit_requests.lock_time
              , deposit_requests.signers_public_key
              , deposit_requests.sender_script_pub_keys
            FROM transactions_in_window transactions
            JOIN sbtc_signer.deposit_requests AS deposit_requests USING (txid)
            LEFT JOIN sbtc_signer.deposit_signers AS ds
              ON ds.txid = deposit_requests.txid
             AND ds.output_index = deposit_requests.output_index
             AND ds.signer_pub_key = $3
            WHERE ds.txid IS NULL
            "#,
        )
        .bind(chain_tip)
        .bind(i32::from(context_window))
        .bind(signer_public_key)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_pending_accepted_deposit_requests(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
        threshold: u16,
    ) -> Result<Vec<model::DepositRequest>, Error> {
        // Add one to the acceptable unlock height because the chain tip is at height one less
        // than the height of the next block, which is the block for which we are assessing
        // the threshold.
        let minimum_acceptable_unlock_height = *self
            .get_bitcoin_block(chain_tip)
            .await?
            .ok_or(Error::MissingBitcoinBlock(*chain_tip))?
            .block_height as i32
            + DEPOSIT_LOCKTIME_BLOCK_BUFFER as i32
            + 1;

        sqlx::query_as::<_, model::DepositRequest>(
            r#"
            WITH transactions_in_window AS (
                SELECT
                    transactions.txid
                  , blocks_in_window.block_height
                FROM bitcoin_blockchain_of($1, $2) AS blocks_in_window
                JOIN sbtc_signer.bitcoin_transactions transactions ON
                    transactions.block_hash = blocks_in_window.block_hash
            ),
            -- First we get all the deposits that are accepted by enough signers
            accepted_deposits AS (
                SELECT
                    deposit_requests.txid
                  , deposit_requests.output_index
                  , deposit_requests.spend_script
                  , deposit_requests.reclaim_script
                  , deposit_requests.recipient
                  , deposit_requests.amount
                  , deposit_requests.max_fee
                  , deposit_requests.lock_time
                  , deposit_requests.signers_public_key
                  , deposit_requests.sender_script_pub_keys
                FROM transactions_in_window transactions
                JOIN sbtc_signer.deposit_requests deposit_requests USING(txid)
                JOIN sbtc_signer.deposit_signers signers USING(txid, output_index)
                WHERE
                    signers.can_accept
                    AND signers.can_sign
                    AND (transactions.block_height + deposit_requests.lock_time) >= $4
                GROUP BY deposit_requests.txid, deposit_requests.output_index
                HAVING COUNT(signers.txid) >= $3
            )
            -- Then we only consider the ones not swept yet (in the canonical chain)
            SELECT accepted_deposits.*
            FROM accepted_deposits
            LEFT JOIN sbtc_signer.bitcoin_tx_inputs AS bti
              ON bti.prevout_txid = accepted_deposits.txid
             AND bti.prevout_output_index = accepted_deposits.output_index
            LEFT JOIN transactions_in_window
              ON bti.txid = transactions_in_window.txid
            GROUP BY
                accepted_deposits.txid
              , accepted_deposits.output_index
              , accepted_deposits.spend_script
              , accepted_deposits.reclaim_script
              , accepted_deposits.recipient
              , accepted_deposits.amount
              , accepted_deposits.max_fee
              , accepted_deposits.lock_time
              , accepted_deposits.signers_public_key
              , accepted_deposits.sender_script_pub_keys
            HAVING
                COUNT(transactions_in_window.txid) = 0
            "#,
        )
        .bind(chain_tip)
        .bind(i32::from(context_window))
        .bind(i32::from(threshold))
        .bind(minimum_acceptable_unlock_height)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_deposit_request_signer_votes(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
        aggregate_key: &PublicKey,
    ) -> Result<model::SignerVotes, Error> {
        sqlx::query_as::<_, model::SignerVote>(
            r#"
            WITH signer_set_rows AS (
                SELECT DISTINCT UNNEST(signer_set_public_keys) AS signer_public_key
                FROM sbtc_signer.dkg_shares
                WHERE aggregate_key = $1
            ),
            deposit_votes AS (
                SELECT
                    signer_pub_key AS signer_public_key
                  , can_accept AND can_sign AS is_accepted
                FROM sbtc_signer.deposit_signers AS ds
                WHERE TRUE
                  AND ds.txid = $2
                  AND ds.output_index = $3
            )
            SELECT
                signer_public_key
              , is_accepted
            FROM signer_set_rows AS ss
            LEFT JOIN deposit_votes AS ds USING(signer_public_key)
            "#,
        )
        .bind(aggregate_key)
        .bind(txid)
        .bind(i64::from(output_index))
        .fetch_all(&self.0)
        .await
        .map(model::SignerVotes::from)
        .map_err(Error::SqlxQuery)
    }

    async fn get_withdrawal_request_signer_votes(
        &self,
        id: &model::QualifiedRequestId,
        aggregate_key: &PublicKey,
    ) -> Result<model::SignerVotes, Error> {
        sqlx::query_as::<_, model::SignerVote>(
            r#"
            WITH signer_set_rows AS (
                SELECT DISTINCT UNNEST(signer_set_public_keys) AS signer_public_key
                FROM sbtc_signer.dkg_shares
                WHERE aggregate_key = $1
            ),
            withdrawal_votes AS (
                SELECT
                    signer_pub_key AS signer_public_key
                  , is_accepted
                FROM sbtc_signer.withdrawal_signers AS ws
                WHERE TRUE
                  AND ws.txid = $2
                  AND ws.block_hash = $3
                  AND ws.request_id = $4
            )
            SELECT
                signer_public_key
              , is_accepted
            FROM signer_set_rows AS ss
            LEFT JOIN withdrawal_votes AS wv USING(signer_public_key)
            "#,
        )
        .bind(aggregate_key)
        .bind(id.txid)
        .bind(id.block_hash)
        .bind(i64::try_from(id.request_id).map_err(Error::ConversionDatabaseInt)?)
        .fetch_all(&self.0)
        .await
        .map(model::SignerVotes::from)
        .map_err(Error::SqlxQuery)
    }

    async fn get_deposit_request_report(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        txid: &model::BitcoinTxId,
        output_index: u32,
        signer_public_key: &PublicKey,
    ) -> Result<Option<DepositRequestReport>, Error> {
        // Now fetch the deposit summary
        let summary_fut = self.get_deposit_request_status_summary(
            chain_tip,
            txid,
            output_index,
            signer_public_key,
        );
        let Some(summary) = summary_fut.await? else {
            return Ok(None);
        };

        // The block height and block hash are always None or not None at
        // the same time.
        let block_info = summary.block_height.zip(summary.block_hash);

        // Lastly we map the block_height variable to a status enum.
        let status = match block_info {
            // Now that we know that it has been confirmed, check to see if
            // it has been swept in a bitcoin transaction that has been
            // confirmed already. We use the height of when the deposit was
            // confirmed for the min height for when a sweep transaction
            // could be confirmed. We could also use block_height + 1.
            Some((block_height, block_hash)) => {
                let deposit_sweep_txid =
                    self.get_deposit_sweep_txid(chain_tip, txid, output_index, block_height);

                match deposit_sweep_txid.await? {
                    Some(txid) => DepositConfirmationStatus::Spent(txid),
                    None => DepositConfirmationStatus::Confirmed(block_height, block_hash),
                }
            }
            // If we didn't grab the block height in the above query, then
            // we know that the deposit transaction is not on the
            // blockchain identified by the chain tip.
            None => DepositConfirmationStatus::Unconfirmed,
        };

        let dkg_shares = self
            .get_encrypted_dkg_shares(summary.signers_public_key)
            .await?;

        Ok(Some(DepositRequestReport {
            status,
            can_sign: summary.can_sign,
            can_accept: summary.can_accept,
            amount: summary.amount,
            max_fee: summary.max_fee,
            lock_time: bitcoin::relative::LockTime::from_consensus(summary.lock_time)
                .map_err(Error::DisabledLockTime)?,
            outpoint: bitcoin::OutPoint::new((*txid).into(), output_index),
            deposit_script: summary.deposit_script.into(),
            reclaim_script: summary.reclaim_script.into(),
            signers_public_key: summary.signers_public_key.into(),
            dkg_shares_status: dkg_shares.map(|shares| shares.dkg_shares_status),
        }))
    }

    async fn get_deposit_signers(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
    ) -> Result<Vec<model::DepositSigner>, Error> {
        sqlx::query_as::<_, model::DepositSigner>(
            "SELECT
                txid
              , output_index
              , signer_pub_key
              , can_accept
              , can_sign
            FROM sbtc_signer.deposit_signers
            WHERE txid = $1 AND output_index = $2",
        )
        .bind(txid)
        .bind(i32::try_from(output_index).map_err(Error::ConversionDatabaseInt)?)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn can_sign_deposit_tx(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
        signer_public_key: &PublicKey,
    ) -> Result<Option<bool>, Error> {
        sqlx::query_scalar::<_, bool>(
            r#"
            WITH x_only_public_keys AS (
                -- These are the aggregate public keys that this signer is
                -- a party on. We lop off the first byte because we want
                -- x-only aggregate keys here.
                SELECT substring(aggregate_key FROM 2) AS signers_public_key
                FROM sbtc_signer.dkg_shares AS ds
                WHERE $3 = ANY(signer_set_public_keys)
            )
            SELECT xo.signers_public_key IS NOT NULL
            FROM sbtc_signer.deposit_requests AS dr
            LEFT JOIN x_only_public_keys AS xo USING (signers_public_key)
            WHERE dr.txid = $1
              AND dr.output_index = $2
            LIMIT 1
            "#,
        )
        .bind(txid)
        .bind(i32::try_from(output_index).map_err(Error::ConversionDatabaseInt)?)
        .bind(signer_public_key)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn deposit_request_exists(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
    ) -> Result<bool, Error> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT TRUE
                FROM sbtc_signer.deposit_requests AS dr
                WHERE dr.txid = $1
                  AND dr.output_index = $2
            )
            "#,
        )
        .bind(txid)
        .bind(i32::try_from(output_index).map_err(Error::ConversionDatabaseInt)?)
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_withdrawal_signers(
        &self,
        request_id: u64,
        block_hash: &model::StacksBlockHash,
    ) -> Result<Vec<model::WithdrawalSigner>, Error> {
        sqlx::query_as::<_, model::WithdrawalSigner>(
            "SELECT
                request_id
              , txid
              , block_hash
              , signer_pub_key
              , is_accepted
              , created_at
            FROM sbtc_signer.withdrawal_signers
            WHERE request_id = $1 AND block_hash = $2",
        )
        .bind(i64::try_from(request_id).map_err(Error::ConversionDatabaseInt)?)
        .bind(block_hash)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_pending_withdrawal_requests(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
        signer_public_key: &PublicKey,
    ) -> Result<Vec<model::WithdrawalRequest>, Error> {
        let Some(stacks_chain_tip) = self.get_stacks_chain_tip(chain_tip).await? else {
            return Ok(Vec::new());
        };
        sqlx::query_as::<_, model::WithdrawalRequest>(
            r#"
            WITH RECURSIVE extended_context_window AS (
                SELECT
                    block_hash
                  , parent_hash
                  , 1 AS depth
                FROM sbtc_signer.bitcoin_blocks
                WHERE block_hash = $1

                UNION ALL

                SELECT
                    parent.block_hash
                  , parent.parent_hash
                  , last.depth + 1
                FROM sbtc_signer.bitcoin_blocks parent
                JOIN extended_context_window last ON parent.block_hash = last.parent_hash
                WHERE last.depth <= $3
            ),
            stacks_context_window AS (
                SELECT
                    stacks_blocks.block_hash
                  , stacks_blocks.block_height
                  , stacks_blocks.parent_hash
                FROM sbtc_signer.stacks_blocks stacks_blocks
                WHERE stacks_blocks.block_hash = $2

                UNION ALL

                SELECT
                    parent.block_hash
                  , parent.block_height
                  , parent.parent_hash
                FROM sbtc_signer.stacks_blocks parent
                JOIN stacks_context_window last
                        ON parent.block_hash = last.parent_hash
                JOIN extended_context_window block
                        ON block.block_hash = parent.bitcoin_anchor
            )
            SELECT
                wr.request_id
              , wr.txid
              , wr.block_hash
              , wr.recipient
              , wr.amount
              , wr.max_fee
              , wr.sender_address
              , wr.bitcoin_block_height
            FROM sbtc_signer.withdrawal_requests wr
            JOIN stacks_context_window sc USING (block_hash)
            LEFT JOIN sbtc_signer.withdrawal_signers AS ws
              ON ws.request_id = wr.request_id
             AND ws.block_hash = wr.block_hash
             AND ws.signer_pub_key = $4
            WHERE ws.request_id IS NULL
            "#,
        )
        .bind(chain_tip)
        .bind(stacks_chain_tip.block_hash)
        .bind(i32::from(context_window))
        .bind(signer_public_key)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_pending_accepted_withdrawal_requests(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        stacks_chain_tip: &model::StacksBlockHash,
        min_bitcoin_height: BitcoinBlockHeight,
        signature_threshold: u16,
    ) -> Result<Vec<model::WithdrawalRequest>, Error> {
        sqlx::query_as::<_, model::WithdrawalRequest>(
            r#"
            -- get_pending_accepted_withdrawal_requests
            WITH recursive

            -- Get all withdrawal requests which have a bitcoin block height
            -- of at least minimum height provided.
            requests AS (
                SELECT
                    wr.request_id
                  , wr.txid
                  , wr.block_hash
                  , wr.recipient
                  , wr.amount
                  , wr.max_fee
                  , wr.sender_address
                  , wr.bitcoin_block_height
                  , bt.block_hash as sweep_block_hash
                  , wre.block_hash as reject_block_hash
                FROM sbtc_signer.withdrawal_requests wr

                -- Join in any sweep transactions we know about.
                LEFT JOIN sbtc_signer.bitcoin_withdrawals_outputs bwo
                    ON bwo.request_id = wr.request_id
                    AND bwo.stacks_block_hash = wr.block_hash
                LEFT JOIN sbtc_signer.bitcoin_transactions bt
                    ON bt.txid = bwo.bitcoin_txid

                -- Join in any rejection events we know about.
                LEFT JOIN sbtc_signer.withdrawal_reject_events AS wre
                    ON wre.request_id = wr.request_id

                -- Only requests where the bitcoin height is >= than the minimum.
                WHERE wr.bitcoin_block_height >= $3
            ),

            -- Fetch the canonical bitcoin blockchain from the chain tip back
            -- to the lowest bitcoin block height of the requests.
            bitcoin_blockchain AS (
                SELECT
                    block_hash
                  , block_height
                FROM bitcoin_blockchain_until($1, $3 - 1)
            ),

            -- Fetch the canonical stacks blockchain from the chain tip back
            -- to the anchor block with the lowest bitcoin block height of all of
            -- the requests.
            stacks_blockchain AS (
                SELECT
                    stacks_blocks.block_hash
                  , stacks_blocks.block_height
                  , stacks_blocks.parent_hash
                FROM sbtc_signer.stacks_blocks stacks_blocks
                WHERE stacks_blocks.block_hash = $2

                UNION ALL

                SELECT
                    parent.block_hash
                  , parent.block_height
                  , parent.parent_hash
                FROM sbtc_signer.stacks_blocks parent
                JOIN stacks_blockchain last
                    ON parent.block_hash = last.parent_hash
                JOIN bitcoin_blockchain anchor
                    ON anchor.block_hash = parent.bitcoin_anchor
            )

            -- Main select clause (what we're returning).
            SELECT
                wr.request_id
              , wr.txid
              , wr.block_hash
              , wr.recipient
              , wr.amount
              , wr.max_fee
              , wr.sender_address
              , wr.bitcoin_block_height

            -- We start the query from the `requests` CTE.
            FROM requests wr

            -- Return a row for each signer vote.
            JOIN sbtc_signer.withdrawal_signers signers ON
                wr.request_id = signers.request_id
                AND wr.block_hash = signers.block_hash
                AND signers.is_accepted = TRUE

            -- Ensure the request is confirmed on the canonical stacks chain
            JOIN stacks_blockchain canonical_confirmed
                ON wr.block_hash = canonical_confirmed.block_hash

            -- Are there any confirmed sweep transactions?
            LEFT JOIN bitcoin_blockchain AS canonical_sweep
                ON wr.sweep_block_hash = canonical_sweep.block_hash

            -- Are there any confirmed reject transactions?
            LEFT JOIN stacks_blockchain AS canonical_reject
                ON wr.reject_block_hash = canonical_reject.block_hash

            GROUP BY
                wr.request_id
              , wr.block_hash
              , wr.txid
              , wr.recipient
              , wr.amount
              , wr.max_fee
              , wr.sender_address
              , wr.bitcoin_block_height

            HAVING
                -- Ensure there are enough 'yes' votes.
                COUNT(wr.request_id) >= $4
                -- Ensure there are no confirmed sweep transactions.
                AND COUNT(canonical_sweep.block_hash) = 0
                -- Ensure there are no confirmed reject contract-calls.
                AND COUNT(canonical_reject.block_hash) = 0

            ORDER BY
                wr.request_id ASC
            "#,
        )
        .bind(bitcoin_chain_tip)
        .bind(stacks_chain_tip)
        .bind(i64::try_from(min_bitcoin_height).map_err(Error::ConversionDatabaseInt)?)
        .bind(i64::from(signature_threshold))
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_pending_rejected_withdrawal_requests(
        &self,
        chain_tip: &model::BitcoinBlockRef,
        context_window: u16,
    ) -> Result<Vec<model::WithdrawalRequest>, Error> {
        let Some(stacks_chain_tip) = self.get_stacks_chain_tip(&chain_tip.block_hash).await? else {
            return Ok(Vec::new());
        };

        let expiration_height = chain_tip
            .block_height
            .saturating_sub(WITHDRAWAL_BLOCKS_EXPIRY);

        sqlx::query_as::<_, model::WithdrawalRequest>(
            r#"
            -- get_pending_rejected_withdrawal_requests
            WITH RECURSIVE bitcoin_blockchain AS (
                SELECT
                    block_hash
                  , block_height
                FROM bitcoin_blockchain_of($1, $2)
            ),
            stacks_context_window AS (
                SELECT
                    stacks_blocks.block_hash
                  , stacks_blocks.block_height
                  , stacks_blocks.parent_hash
                FROM sbtc_signer.stacks_blocks stacks_blocks
                WHERE stacks_blocks.block_hash = $3

                UNION ALL

                SELECT
                    parent.block_hash
                  , parent.block_height
                  , parent.parent_hash
                FROM sbtc_signer.stacks_blocks parent
                JOIN stacks_context_window last
                  ON parent.block_hash = last.parent_hash
                -- Limit the recursion to the bitcoin context window height. We
                -- are not joining directly on `bitcoin_blockchain` as once we
                -- get the stacks chain tip considering its anchor block, then
                -- we can just walk backwards.
                JOIN sbtc_signer.bitcoin_blocks block
                  ON block.block_hash = parent.bitcoin_anchor
                WHERE block.block_height >= (SELECT MIN(block_height) FROM bitcoin_blockchain)
            )
            SELECT
                wr.request_id
              , wr.txid
              , wr.block_hash
              , wr.recipient
              , wr.amount
              , wr.max_fee
              , wr.sender_address
              , wr.bitcoin_block_height
            FROM sbtc_signer.withdrawal_requests wr
            -- Request confirmed on stacks chain
            JOIN stacks_context_window sc ON wr.block_hash = sc.block_hash
            -- Request not accepted
            LEFT JOIN sbtc_signer.bitcoin_withdrawals_outputs AS bwo
                ON bwo.request_id = wr.request_id
                AND bwo.stacks_block_hash = wr.block_hash
            LEFT JOIN bitcoin_transactions AS bc_trx
                ON bc_trx.txid = bwo.bitcoin_txid
            LEFT JOIN bitcoin_blockchain
                ON bc_trx.block_hash = bitcoin_blockchain.block_hash
            -- Request not rejected
            LEFT JOIN withdrawal_reject_events AS wre
                ON wre.request_id = wr.request_id
            LEFT JOIN stacks_context_window sc2
                ON wre.block_hash = sc2.block_hash
            -- Request is expired
            WHERE wr.bitcoin_block_height < $4

            -- we need to group since we could have multiple withdrawals
            -- outputs for a single request, and some of them may not be in
            -- the canonical chain, resulting in a NULL bc_trx.block_hash;
            -- so we group and check that all the rows have NULL
            GROUP BY
                wr.request_id
              , wr.txid
              , wr.block_hash
              , wr.recipient
              , wr.amount
              , wr.max_fee
              , wr.sender_address
              , wr.bitcoin_block_height
            HAVING
                -- Request not accepted (cont'd)
                COUNT(bitcoin_blockchain.block_height) = 0
                -- Request not rejected (cont'd)
            AND COUNT(sc2.block_hash) = 0
            "#,
        )
        .bind(chain_tip.block_hash)
        .bind(i32::from(context_window))
        .bind(stacks_chain_tip.block_hash)
        .bind(i64::try_from(expiration_height).map_err(Error::ConversionDatabaseInt)?)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_withdrawal_request_report(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        stacks_chain_tip: &model::StacksBlockHash,
        id: &model::QualifiedRequestId,
        signer_public_key: &PublicKey,
    ) -> Result<Option<WithdrawalRequestReport>, Error> {
        let summary_fut = self.get_withdrawal_request_status_summary(id, signer_public_key);
        let Some(summary) = summary_fut.await? else {
            return Ok(None);
        };

        let sweep_info_fut = self.get_withdrawal_sweep_info(bitcoin_chain_tip, id);
        let status = match sweep_info_fut.await? {
            Some(tx_ref) => WithdrawalRequestStatus::Fulfilled(tx_ref),
            None => {
                let in_canonical_stacks_blockchain_fut = self.in_canonical_stacks_blockchain(
                    stacks_chain_tip,
                    &summary.stacks_block_hash,
                    summary.stacks_block_height,
                );
                if in_canonical_stacks_blockchain_fut.await? {
                    WithdrawalRequestStatus::Confirmed
                } else {
                    WithdrawalRequestStatus::Unconfirmed
                }
            }
        };

        Ok(Some(WithdrawalRequestReport {
            id: *id,
            amount: summary.amount,
            max_fee: summary.max_fee,
            is_accepted: summary.is_accepted,
            recipient: summary.recipient.into(),
            status,
            bitcoin_block_height: summary.bitcoin_block_height,
        }))
    }

    async fn compute_withdrawn_total(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
    ) -> Result<u64, Error> {
        let total_amount = sqlx::query_scalar::<_, Option<i64>>(
            r#"
            SELECT SUM(bto.amount)::BIGINT
            FROM sbtc_signer.bitcoin_tx_outputs AS bto
            JOIN sbtc_signer.bitcoin_transactions AS bt
              ON bt.txid = bto.txid
            JOIN bitcoin_blockchain_of($1, $2) AS bbo
              ON bbo.block_hash = bt.block_hash
            WHERE bto.output_type = 'withdrawal'
            "#,
        )
        .bind(bitcoin_chain_tip)
        .bind(i32::from(context_window))
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        // Amounts are always positive in the database, so this conversion
        // is always fine.
        u64::try_from(total_amount.unwrap_or(0)).map_err(|_| Error::TypeConversion)
    }

    async fn get_bitcoin_blocks_with_transaction(
        &self,
        txid: &model::BitcoinTxId,
    ) -> Result<Vec<model::BitcoinBlockHash>, Error> {
        sqlx::query_as::<_, model::BitcoinTxRef>(
            "SELECT txid, block_hash FROM sbtc_signer.bitcoin_transactions WHERE txid = $1",
        )
        .bind(txid)
        .fetch_all(&self.0)
        .await
        .map(|res| {
            res.into_iter()
                .map(|junction| junction.block_hash)
                .collect()
        })
        .map_err(Error::SqlxQuery)
    }

    async fn stacks_block_exists(&self, block_id: StacksBlockId) -> Result<bool, Error> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT TRUE AS exists
            FROM sbtc_signer.stacks_blocks
            WHERE block_hash = $1;"#,
        )
        .bind(block_id.0)
        .fetch_optional(&self.0)
        .await
        .map(|row| row.is_some())
        .map_err(Error::SqlxQuery)
    }

    async fn get_encrypted_dkg_shares<X>(
        &self,
        aggregate_key: X,
    ) -> Result<Option<model::EncryptedDkgShares>, Error>
    where
        X: Into<PublicKeyXOnly> + Send,
    {
        // The aggregate_key column stores compressed public keys, which
        // always include a parity byte. Since the input here is an x-only
        // public key we don't have a parity byte, so we lop it off when
        // filtering.
        let key: PublicKeyXOnly = aggregate_key.into();
        sqlx::query_as::<_, model::EncryptedDkgShares>(
            r#"
            SELECT
                aggregate_key
              , tweaked_aggregate_key
              , script_pubkey
              , encrypted_private_shares
              , public_shares
              , signer_set_public_keys
              , signature_share_threshold
              , dkg_shares_status
              , started_at_bitcoin_block_hash
              , started_at_bitcoin_block_height
            FROM sbtc_signer.dkg_shares
            WHERE substring(aggregate_key FROM 2) = $1;
            "#,
        )
        .bind(key)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_latest_encrypted_dkg_shares(
        &self,
    ) -> Result<Option<model::EncryptedDkgShares>, Error> {
        sqlx::query_as::<_, model::EncryptedDkgShares>(
            r#"
            SELECT
                aggregate_key
              , tweaked_aggregate_key
              , script_pubkey
              , encrypted_private_shares
              , public_shares
              , signer_set_public_keys
              , signature_share_threshold
              , dkg_shares_status
              , started_at_bitcoin_block_hash
              , started_at_bitcoin_block_height
            FROM sbtc_signer.dkg_shares
            ORDER BY created_at DESC
            LIMIT 1;
            "#,
        )
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_latest_verified_dkg_shares(
        &self,
    ) -> Result<Option<model::EncryptedDkgShares>, Error> {
        sqlx::query_as::<_, model::EncryptedDkgShares>(
            r#"
            SELECT
                aggregate_key
              , tweaked_aggregate_key
              , script_pubkey
              , encrypted_private_shares
              , public_shares
              , signer_set_public_keys
              , signature_share_threshold
              , dkg_shares_status
              , started_at_bitcoin_block_hash
              , started_at_bitcoin_block_height
            FROM sbtc_signer.dkg_shares
            WHERE dkg_shares_status = 'verified'
            ORDER BY created_at DESC
            LIMIT 1;
            "#,
        )
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    /// Returns the number of non-failed rows in the `dkg_shares` table.
    async fn get_encrypted_dkg_shares_count(&self) -> Result<u32, Error> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sbtc_signer.dkg_shares WHERE dkg_shares_status != 'failed';",
        )
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        u32::try_from(count).map_err(Error::ConversionDatabaseInt)
    }

    /// Find the last key rotation by iterating backwards from the stacks
    /// chain tip scanning all transactions until we encounter a key
    /// rotation transactions.
    ///
    /// This might become quite inefficient for long chains with infrequent
    /// key rotations, so we might have to consider data model updates to
    /// allow more efficient querying of the last key rotation.
    async fn get_last_key_rotation(
        &self,
        chain_tip: &model::BitcoinBlockHash,
    ) -> Result<Option<model::KeyRotationEvent>, Error> {
        let Some(stacks_chain_tip) = self.get_stacks_chain_tip(chain_tip).await? else {
            return Ok(None);
        };

        sqlx::query_as::<_, model::KeyRotationEvent>(
            r#"
            WITH RECURSIVE stacks_blocks AS (
                SELECT
                    block_hash
                  , parent_hash
                  , block_height
                  , 1 AS depth
                FROM sbtc_signer.stacks_blocks
                WHERE block_hash = $1

                UNION ALL

                SELECT
                    parent.block_hash
                  , parent.parent_hash
                  , parent.block_height
                  , last.depth + 1
                FROM sbtc_signer.stacks_blocks parent
                JOIN stacks_blocks last ON parent.block_hash = last.parent_hash
            )
            SELECT
                rkt.txid
              , rkt.block_hash
              , rkt.address
              , rkt.aggregate_key
              , rkt.signer_set
              , rkt.signatures_required
            FROM sbtc_signer.rotate_keys_transactions rkt
            JOIN stacks_blocks AS sb
              ON rkt.block_hash = sb.block_hash
            ORDER BY sb.block_height DESC, sb.block_hash DESC, rkt.txid DESC
            LIMIT 1
            "#,
        )
        .bind(stacks_chain_tip.block_hash)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn key_rotation_exists(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        signer_set: &BTreeSet<PublicKey>,
        aggregate_key: &PublicKey,
        signatures_required: u16,
    ) -> Result<bool, Error> {
        let Some(stacks_chain_tip) = self.get_stacks_chain_tip(chain_tip).await? else {
            return Err(Error::NoStacksChainTip);
        };

        sqlx::query_scalar::<_, bool>(
            r#"
            WITH RECURSIVE stacks_blocks AS (
                SELECT
                    block_hash
                  , parent_hash
                  , block_height
                  , 1 AS depth
                FROM sbtc_signer.stacks_blocks
                WHERE block_hash = $1

                UNION ALL

                SELECT
                    parent.block_hash
                  , parent.parent_hash
                  , parent.block_height
                  , last.depth + 1
                FROM sbtc_signer.stacks_blocks parent
                JOIN stacks_blocks last ON parent.block_hash = last.parent_hash
            )
            SELECT EXISTS (
                SELECT TRUE
                FROM sbtc_signer.rotate_keys_transactions AS rkt
                JOIN stacks_blocks AS sb 
                  ON rkt.block_hash = sb.block_hash
                WHERE rkt.signer_set = $2
                  AND rkt.aggregate_key = $3
                  AND rkt.signatures_required = $4
            )
            "#,
        )
        .bind(stacks_chain_tip.block_hash)
        .bind(signer_set.iter().collect::<Vec<_>>())
        .bind(aggregate_key)
        .bind(i32::from(signatures_required))
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_signers_script_pubkeys(&self) -> Result<Vec<model::Bytes>, Error> {
        sqlx::query_scalar::<_, model::Bytes>(
            r#"
            WITH last_script_pubkey AS (
                SELECT script_pubkey
                FROM sbtc_signer.dkg_shares
                ORDER BY created_at DESC
                LIMIT 1
            )
            SELECT script_pubkey
            FROM last_script_pubkey

            UNION

            SELECT script_pubkey
            FROM sbtc_signer.dkg_shares
            WHERE created_at > CURRENT_TIMESTAMP - INTERVAL '365 DAYS';
            "#,
        )
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_signer_utxo(
        &self,
        chain_tip: &model::BitcoinBlockHash,
    ) -> Result<Option<SignerUtxo>, Error> {
        // If we've swept funds before, then will have a signer output and
        // a minimum UTXO height, so let's try that first.
        let Some(min_block_height) = self.minimum_utxo_height().await? else {
            // If the above function returns None then we know that there
            // have been no confirmed sweep transactions thus far, so let's
            // try looking for a donation UTXO.
            return self.get_donation_utxo(chain_tip).await;
        };
        // Okay, so we know that there has been at least one sweep
        // transaction. Let's look for the UTXO in a block after our
        // min_block_height. Note that `Self::get_utxo` returns `None` only
        // when a reorg has affected all sweep transactions. If this
        // happens we try searching for a donation.
        let output_type = model::TxOutputType::SignersOutput;
        let fut = self.get_utxo(chain_tip, output_type, min_block_height);
        match fut.await? {
            res @ Some(_) => Ok(res),
            None => self.get_donation_utxo(chain_tip).await,
        }
    }

    async fn is_known_bitcoin_block_hash(
        &self,
        block_hash: &model::BitcoinBlockHash,
    ) -> Result<bool, Error> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT TRUE
                FROM sbtc_signer.bitcoin_blocks AS bb
                WHERE bb.block_hash = $1
            );
        "#,
        )
        .bind(block_hash)
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn in_canonical_bitcoin_blockchain(
        &self,
        chain_tip: &model::BitcoinBlockRef,
        block_ref: &model::BitcoinBlockRef,
    ) -> Result<bool, Error> {
        let height_diff: u64 = *chain_tip
            .block_height
            .saturating_sub(block_ref.block_height);

        sqlx::query_scalar::<_, bool>(
            r#"
            WITH RECURSIVE tx_block_chain AS (
                SELECT
                    block_hash
                  , block_height
                  , parent_hash
                  , 0 AS counter
                FROM sbtc_signer.bitcoin_blocks
                WHERE block_hash = $1

                UNION ALL

                SELECT
                    child.block_hash
                  , child.block_height
                  , child.parent_hash
                  , parent.counter + 1
                FROM sbtc_signer.bitcoin_blocks AS child
                JOIN tx_block_chain AS parent
                  ON child.block_hash = parent.parent_hash
                WHERE parent.counter <= $3
            )
            SELECT EXISTS (
                SELECT TRUE
                FROM tx_block_chain AS tbc
                WHERE tbc.block_hash = $2
                  AND tbc.block_height = $4
            );
        "#,
        )
        .bind(chain_tip.block_hash)
        .bind(block_ref.block_hash)
        .bind(i64::try_from(height_diff).map_err(Error::ConversionDatabaseInt)?)
        .bind(i64::try_from(block_ref.block_height).map_err(Error::ConversionDatabaseInt)?)
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn is_signer_script_pub_key(&self, script: &model::ScriptPubKey) -> Result<bool, Error> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT TRUE
                FROM sbtc_signer.dkg_shares AS ds
                WHERE ds.script_pubkey = $1
            );
        "#,
        )
        .bind(script)
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn is_withdrawal_inflight(
        &self,
        id: &model::QualifiedRequestId,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
    ) -> Result<bool, Error> {
        let Some(signer_utxo) = self.get_signer_utxo(bitcoin_chain_tip).await? else {
            return Ok(false);
        };
        let txid: model::BitcoinTxId = signer_utxo.outpoint.txid.into();

        // This should execute quite quickly, since the recursive part of
        // this query should be limited to 25 transactions when the most
        // recent signer UTXO hasn't been reorged. When a reorg affects
        // sweep transactions, this recursive part of the query is bounded
        // by the reorg depth length multiplied by 25.
        sqlx::query_scalar::<_, bool>(
            r#"
            WITH RECURSIVE proposed_transactions AS (
                SELECT
                    bts.txid
                  , bts.prevout_txid
                FROM sbtc_signer.bitcoin_tx_sighashes AS bts
                WHERE bts.prevout_txid = $1

                UNION ALL

                SELECT
                    bts.txid
                  , bts.prevout_txid
                FROM sbtc_signer.bitcoin_tx_sighashes AS bts
                JOIN proposed_transactions AS parent
                  ON bts.prevout_txid = parent.txid
                WHERE bts.prevout_type = 'signers_input'
            )
            SELECT EXISTS (
                SELECT TRUE
                FROM sbtc_signer.bitcoin_withdrawals_outputs AS bwo
                JOIN proposed_transactions AS pt
                  ON pt.txid = bwo.bitcoin_txid
                WHERE bwo.request_id = $2
                  AND bwo.stacks_block_hash = $3
            )"#,
        )
        .bind(txid)
        .bind(i64::try_from(id.request_id).map_err(Error::ConversionDatabaseInt)?)
        .bind(id.block_hash)
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn is_withdrawal_active(
        &self,
        id: &model::QualifiedRequestId,
        bitcoin_chain_tip: &model::BitcoinBlockRef,
        min_confirmations: u64,
    ) -> Result<bool, Error> {
        // We want to find the first sweep transaction that happened
        // *after* the last time the signers considered sweeping the
        // withdrawal request. We do this in two stages:
        // 1. Find the block height of the last time that the signers
        //    considered the withdrawal request (the query right below this
        //    comment).
        // 2. Find the least height of any sweep transaction confirmed in a
        //    block with height greater than the height returned from (1).
        let last_considered_height = sqlx::query_scalar::<_, Option<BitcoinBlockHeight>>(
            r#"
            SELECT MAX(bb.block_height)
            FROM sbtc_signer.bitcoin_withdrawals_outputs AS bwo
            JOIN sbtc_signer.bitcoin_blocks AS bb
              ON bb.block_hash = bwo.bitcoin_chain_tip
            WHERE bwo.request_id = $1
              AND bwo.stacks_block_hash = $2
            "#,
        )
        .bind(i64::try_from(id.request_id).map_err(Error::ConversionDatabaseInt)?)
        .bind(id.block_hash)
        .fetch_one(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        // We add one because we are interested in sweeps that were
        // confirmed after the signers last considered the withdrawal.
        let Some(min_block_height) = last_considered_height.map(|height| height + 1) else {
            // This means that there are no rows associated with the ID in the
            // `bitcoin_withdrawals_outputs` table.
            return Ok(false);
        };

        // We now know that the signers considered this withdrawal request
        // at least once. We now try to find the height of the oldest
        // signer TXO whose height is greater than the height from above.
        let chain_tip_hash = &bitcoin_chain_tip.block_hash;
        let least_txo_height = self
            .get_least_txo_height(chain_tip_hash, min_block_height)
            .await?;
        // If this returns None, then the sweep itself could be in the
        // mempool. If that's the case then this is definitely active.
        let Some(least_txo_height) = least_txo_height else {
            return Ok(true);
        };
        // We test whether the TXO height has a minimum number of
        // confirmations. If it doesn't have enough confirmations, then it
        // is considered active since a fork could put it into the mempool
        // again.
        let txo_confirmations = bitcoin_chain_tip
            .block_height
            .saturating_sub(least_txo_height);
        Ok(txo_confirmations <= min_confirmations.into())
    }

    async fn get_swept_deposit_requests(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
    ) -> Result<Vec<model::SweptDepositRequest>, Error> {
        // The following tests define the criteria for this query:
        // - [X] get_swept_deposit_requests_returns_swept_deposit_requests
        // - [X] get_swept_deposit_requests_does_not_return_unswept_deposit_requests
        // - [X] get_swept_deposit_requests_does_not_return_deposit_requests_with_responses
        // - [X] get_swept_deposit_requests_response_tx_reorged
        //
        // Note that this query may return completed requests if the stacks
        // event is anchored to a bitcoin block that is outside the context
        // window, while the sweep is still inside it.

        let Some(stacks_chain_tip) = self.get_stacks_chain_tip(chain_tip).await? else {
            return Ok(Vec::new());
        };

        sqlx::query_as::<_, model::SweptDepositRequest>(
            "
            WITH RECURSIVE bitcoin_blockchain AS (
                SELECT
                    block_hash
                  , block_height
                FROM bitcoin_blockchain_of($1, $2)
            ),
            stacks_blockchain AS (
                SELECT
                    stacks_blocks.block_hash
                  , stacks_blocks.block_height
                  , stacks_blocks.parent_hash
                FROM sbtc_signer.stacks_blocks stacks_blocks
                JOIN bitcoin_blockchain as bb
                    ON bb.block_hash = stacks_blocks.bitcoin_anchor
                WHERE stacks_blocks.block_hash = $3

                UNION ALL

                SELECT
                    parent.block_hash
                  , parent.block_height
                  , parent.parent_hash
                FROM sbtc_signer.stacks_blocks parent
                JOIN stacks_blockchain last
                  ON parent.block_hash = last.parent_hash
                JOIN bitcoin_blockchain AS bb
                  ON bb.block_hash = parent.bitcoin_anchor
            )
            SELECT
                bc_trx.txid AS sweep_txid
              , bc_trx.block_hash AS sweep_block_hash
              , bc_blocks.block_height AS sweep_block_height
              , deposit_req.txid
              , deposit_req.output_index
              , deposit_req.recipient
              , deposit_req.amount
              , deposit_req.max_fee
            FROM bitcoin_blockchain AS bc_blocks
            INNER JOIN bitcoin_transactions AS bc_trx USING (block_hash)
            INNER JOIN bitcoin_tx_inputs AS bti USING (txid)
            INNER JOIN deposit_requests AS deposit_req
              ON deposit_req.txid = bti.prevout_txid
             AND deposit_req.output_index = bti.prevout_output_index
            LEFT JOIN completed_deposit_events AS cde
              ON cde.bitcoin_txid = deposit_req.txid
             AND cde.output_index = deposit_req.output_index
            LEFT JOIN stacks_blockchain AS sb
              ON sb.block_hash = cde.block_hash
            GROUP BY
                bc_trx.txid
              , bc_trx.block_hash
              , bc_blocks.block_height
              , deposit_req.txid
              , deposit_req.output_index
              , deposit_req.recipient
              , deposit_req.amount
            HAVING
                COUNT(sb.block_hash) = 0
        ",
        )
        .bind(chain_tip)
        .bind(i32::from(context_window))
        .bind(stacks_chain_tip.block_hash)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_swept_withdrawal_requests(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
    ) -> Result<Vec<model::SweptWithdrawalRequest>, Error> {
        // The following tests define the criteria for this query:
        // - [X] get_swept_withdrawal_requests_returns_swept_withdrawal_requests
        // - [X] get_swept_withdrawal_requests_does_not_return_unswept_withdrawal_requests
        // - [X] get_swept_withdrawal_requests_does_not_return_withdrawal_requests_with_responses
        // - [X] get_swept_withdrawal_requests_response_tx_reorged
        //
        // Note that this query may return completed requests if the stacks
        // event is anchored to a bitcoin block that is outside the context
        // window, while the sweep is still inside it.

        let Some(stacks_chain_tip) = self.get_stacks_chain_tip(chain_tip).await? else {
            return Ok(Vec::new());
        };

        sqlx::query_as::<_, model::SweptWithdrawalRequest>(
            "
                WITH RECURSIVE bitcoin_blockchain AS (
                    SELECT
                        block_hash
                      , block_height
                    FROM bitcoin_blockchain_of($1, $2)
                ),
                stacks_blockchain AS (
                    SELECT
                        stacks_blocks.block_hash
                      , stacks_blocks.block_height
                      , stacks_blocks.parent_hash
                    FROM sbtc_signer.stacks_blocks stacks_blocks
                    JOIN bitcoin_blockchain AS bb
                        ON bb.block_hash = stacks_blocks.bitcoin_anchor
                    WHERE stacks_blocks.block_hash = $3
                    UNION ALL
                    SELECT
                        parent.block_hash
                      , parent.block_height
                      , parent.parent_hash
                    FROM sbtc_signer.stacks_blocks parent
                    JOIN stacks_blockchain last
                        ON parent.block_hash = last.parent_hash
                    JOIN bitcoin_blockchain AS bb
                        ON bb.block_hash = parent.bitcoin_anchor
                )
                SELECT
                    bwo.output_index AS output_index
                  , bwo.bitcoin_txid AS sweep_txid
                  , bc_blocks.block_hash AS sweep_block_hash
                  , bc_blocks.block_height AS sweep_block_height
                  , wr.request_id
                  , wr.txid
                  , wr.block_hash AS block_hash
                  , wr.recipient
                  , wr.amount
                  , wr.max_fee
                  , wr.sender_address
                FROM sbtc_signer.bitcoin_withdrawals_outputs AS bwo
                JOIN sbtc_signer.bitcoin_transactions AS bt
                    ON bt.txid = bwo.bitcoin_txid
                JOIN sbtc_signer.withdrawal_requests AS wr
                    ON wr.request_id = bwo.request_id
                    AND wr.block_hash = bwo.stacks_block_hash
                JOIN bitcoin_blockchain AS bc_blocks
                    ON bc_blocks.block_hash = bt.block_hash
                LEFT JOIN sbtc_signer.withdrawal_accept_events AS wae
                    ON wae.request_id = wr.request_id
                LEFT JOIN stacks_blockchain AS sb
                    ON sb.block_hash = wae.block_hash

                GROUP BY
                    bwo.output_index
                  , bwo.bitcoin_txid
                  , bc_blocks.block_hash
                  , bc_blocks.block_height
                  , wr.request_id
                  , wr.txid
                  , wr.block_hash
                  , wr.recipient
                  , wr.amount
                  , wr.max_fee
                  , wr.sender_address

                HAVING
                    COUNT(sb.block_hash) = 0
        ",
        )
        .bind(chain_tip)
        .bind(i32::from(context_window))
        .bind(stacks_chain_tip.block_hash)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_deposit_request(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
    ) -> Result<Option<model::DepositRequest>, Error> {
        sqlx::query_as::<_, model::DepositRequest>(
            r#"
            SELECT txid
                 , output_index
                 , spend_script
                 , reclaim_script
                 , recipient
                 , amount
                 , max_fee
                 , lock_time
                 , signers_public_key
                 , sender_script_pub_keys
            FROM sbtc_signer.deposit_requests
            WHERE txid = $1
              AND output_index = $2
            "#,
        )
        .bind(txid)
        .bind(i32::try_from(output_index).map_err(Error::ConversionDatabaseInt)?)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn will_sign_bitcoin_tx_sighash(
        &self,
        sighash: &model::SigHash,
    ) -> Result<Option<(bool, PublicKeyXOnly)>, Error> {
        sqlx::query_as::<_, (bool, PublicKeyXOnly)>(
            r#"
            SELECT
                will_sign
              , x_only_public_key
            FROM sbtc_signer.bitcoin_tx_sighashes
            WHERE sighash = $1
            "#,
        )
        .bind(sighash)
        .fetch_optional(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_withdrawal_signer_decisions(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
        signer_public_key: &PublicKey,
    ) -> Result<Vec<model::WithdrawalSigner>, Error> {
        sqlx::query_as::<_, model::WithdrawalSigner>(
            r#"
            WITH target_block AS (
                SELECT blocks.block_hash, blocks.created_at
                FROM sbtc_signer.bitcoin_blockchain_of($1, $2) chain
                JOIN sbtc_signer.bitcoin_blocks blocks USING (block_hash)
                ORDER BY chain.block_height ASC
                LIMIT 1
            )
            SELECT
                ws.request_id
              , ws.txid
              , ws.block_hash
              , ws.signer_pub_key
              , ws.is_accepted

            FROM sbtc_signer.withdrawal_signers ws
            WHERE ws.signer_pub_key = $3
              AND ws.created_at >= (SELECT created_at FROM target_block)
            "#,
        )
        .bind(chain_tip)
        .bind(i32::from(context_window))
        .bind(signer_public_key)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }

    async fn get_deposit_signer_decisions(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
        signer_public_key: &PublicKey,
    ) -> Result<Vec<model::DepositSigner>, Error> {
        sqlx::query_as::<_, model::DepositSigner>(
            r#"
            WITH target_block AS (
                SELECT blocks.block_hash, blocks.created_at
                FROM sbtc_signer.bitcoin_blockchain_of($1, $2) chain
                JOIN sbtc_signer.bitcoin_blocks blocks USING (block_hash)
                ORDER BY chain.block_height ASC
                LIMIT 1
            )
            SELECT
                ds.txid
              , ds.output_index
              , ds.signer_pub_key
              , ds.can_sign
              , ds.can_accept
            FROM sbtc_signer.deposit_signers ds
            WHERE ds.signer_pub_key = $3
              AND ds.created_at >= (SELECT created_at FROM target_block)
            "#,
        )
        .bind(chain_tip)
        .bind(i32::from(context_window))
        .bind(signer_public_key)
        .fetch_all(&self.0)
        .await
        .map_err(Error::SqlxQuery)
    }
}

impl super::DbWrite for PgStore {
    async fn write_bitcoin_block(&self, block: &model::BitcoinBlock) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO sbtc_signer.bitcoin_blocks
              ( block_hash
              , block_height
              , parent_hash
              )
            VALUES ($1, $2, $3)
            ON CONFLICT DO NOTHING",
        )
        .bind(block.block_hash)
        .bind(i64::try_from(block.block_height).map_err(Error::ConversionDatabaseInt)?)
        .bind(block.parent_hash)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_stacks_block(&self, block: &model::StacksBlock) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO sbtc_signer.stacks_blocks
              ( block_hash
              , block_height
              , parent_hash
              , bitcoin_anchor
              )
            VALUES ($1, $2, $3, $4)
            ON CONFLICT DO NOTHING",
        )
        .bind(block.block_hash)
        .bind(i64::try_from(block.block_height).map_err(Error::ConversionDatabaseInt)?)
        .bind(block.parent_hash)
        .bind(block.bitcoin_anchor)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_deposit_request(
        &self,
        deposit_request: &model::DepositRequest,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO sbtc_signer.deposit_requests
              ( txid
              , output_index
              , spend_script
              , reclaim_script
              , recipient
              , amount
              , max_fee
              , lock_time
              , signers_public_key
              , sender_script_pub_keys
              )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT DO NOTHING",
        )
        .bind(deposit_request.txid)
        .bind(i32::try_from(deposit_request.output_index).map_err(Error::ConversionDatabaseInt)?)
        .bind(&deposit_request.spend_script)
        .bind(&deposit_request.reclaim_script)
        .bind(&deposit_request.recipient)
        .bind(i64::try_from(deposit_request.amount).map_err(Error::ConversionDatabaseInt)?)
        .bind(i64::try_from(deposit_request.max_fee).map_err(Error::ConversionDatabaseInt)?)
        .bind(i64::from(deposit_request.lock_time))
        .bind(deposit_request.signers_public_key)
        .bind(&deposit_request.sender_script_pub_keys)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_deposit_requests(
        &self,
        deposit_requests: Vec<model::DepositRequest>,
    ) -> Result<(), Error> {
        if deposit_requests.is_empty() {
            return Ok(());
        }

        let mut txid = Vec::with_capacity(deposit_requests.len());
        let mut output_index = Vec::with_capacity(deposit_requests.len());
        let mut spend_script = Vec::with_capacity(deposit_requests.len());
        let mut reclaim_script = Vec::with_capacity(deposit_requests.len());
        let mut recipient = Vec::with_capacity(deposit_requests.len());
        let mut amount = Vec::with_capacity(deposit_requests.len());
        let mut max_fee = Vec::with_capacity(deposit_requests.len());
        let mut lock_time = Vec::with_capacity(deposit_requests.len());
        let mut signers_public_key = Vec::with_capacity(deposit_requests.len());
        let mut sender_script_pubkeys = Vec::with_capacity(deposit_requests.len());

        for req in deposit_requests {
            let vout = i32::try_from(req.output_index).map_err(Error::ConversionDatabaseInt)?;
            txid.push(req.txid);
            output_index.push(vout);
            spend_script.push(req.spend_script);
            reclaim_script.push(req.reclaim_script);
            recipient.push(req.recipient);
            amount.push(i64::try_from(req.amount).map_err(Error::ConversionDatabaseInt)?);
            max_fee.push(i64::try_from(req.max_fee).map_err(Error::ConversionDatabaseInt)?);
            lock_time.push(i64::from(req.lock_time));
            signers_public_key.push(req.signers_public_key);
            // We need to join the addresses like this (and later split
            // them), because handling of multidimensional arrays in
            // postgres is tough. The naive approach of doing
            // UNNEST($1::VARCHAR[][]) doesn't work, since that completely
            // flattens the array.
            let addresses: Vec<String> = req
                .sender_script_pub_keys
                .iter()
                .map(|x| x.to_hex_string())
                .collect();
            sender_script_pubkeys.push(addresses.join(","));
        }

        sqlx::query(
            r#"
            WITH tx_ids       AS (SELECT ROW_NUMBER() OVER (), txid FROM UNNEST($1::BYTEA[]) AS txid)
            , output_index    AS (SELECT ROW_NUMBER() OVER (), output_index FROM UNNEST($2::INTEGER[]) AS output_index)
            , spend_script    AS (SELECT ROW_NUMBER() OVER (), spend_script FROM UNNEST($3::BYTEA[]) AS spend_script)
            , reclaim_script  AS (SELECT ROW_NUMBER() OVER (), reclaim_script FROM UNNEST($4::BYTEA[]) AS reclaim_script)
            , recipient       AS (SELECT ROW_NUMBER() OVER (), recipient FROM UNNEST($5::TEXT[]) AS recipient)
            , amount          AS (SELECT ROW_NUMBER() OVER (), amount FROM UNNEST($6::BIGINT[]) AS amount)
            , max_fee         AS (SELECT ROW_NUMBER() OVER (), max_fee FROM UNNEST($7::BIGINT[]) AS max_fee)
            , lock_time       AS (SELECT ROW_NUMBER() OVER (), lock_time FROM UNNEST($8::BIGINT[]) AS lock_time)
            , signer_pub_keys AS (SELECT ROW_NUMBER() OVER (), signers_public_key FROM UNNEST($9::BYTEA[]) AS signers_public_key)
            , script_pub_keys AS (SELECT ROW_NUMBER() OVER (), senders FROM UNNEST($10::VARCHAR[]) AS senders)
            INSERT INTO sbtc_signer.deposit_requests (
                  txid
                , output_index
                , spend_script
                , reclaim_script
                , recipient
                , amount
                , max_fee
                , lock_time
                , signers_public_key
                , sender_script_pub_keys)
            SELECT
                txid
              , output_index
              , spend_script
              , reclaim_script
              , recipient
              , amount
              , max_fee
              , lock_time
              , signers_public_key
              , ARRAY(SELECT decode(UNNEST(regexp_split_to_array(senders, ',')), 'hex'))
            FROM tx_ids
            JOIN output_index USING (row_number)
            JOIN spend_script USING (row_number)
            JOIN reclaim_script USING (row_number)
            JOIN recipient USING (row_number)
            JOIN amount USING (row_number)
            JOIN max_fee USING (row_number)
            JOIN lock_time USING (row_number)
            JOIN signer_pub_keys USING (row_number)
            JOIN script_pub_keys USING (row_number)
            ON CONFLICT DO NOTHING"#,
        )
        .bind(txid)
        .bind(output_index)
        .bind(spend_script)
        .bind(reclaim_script)
        .bind(recipient)
        .bind(amount)
        .bind(max_fee)
        .bind(lock_time)
        .bind(signers_public_key)
        .bind(sender_script_pubkeys)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_withdrawal_request(
        &self,
        request: &model::WithdrawalRequest,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO sbtc_signer.withdrawal_requests
              ( request_id
              , txid
              , block_hash
              , recipient
              , amount
              , max_fee
              , sender_address
              , bitcoin_block_height
              )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT DO NOTHING",
        )
        .bind(i64::try_from(request.request_id).map_err(Error::ConversionDatabaseInt)?)
        .bind(request.txid)
        .bind(request.block_hash)
        .bind(&request.recipient)
        .bind(i64::try_from(request.amount).map_err(Error::ConversionDatabaseInt)?)
        .bind(i64::try_from(request.max_fee).map_err(Error::ConversionDatabaseInt)?)
        .bind(&request.sender_address)
        .bind(i64::try_from(request.bitcoin_block_height).map_err(Error::ConversionDatabaseInt)?)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn write_deposit_signer_decision(
        &self,
        decision: &model::DepositSigner,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO sbtc_signer.deposit_signers
              ( txid
              , output_index
              , signer_pub_key
              , can_accept
              , can_sign
              )
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT DO NOTHING",
        )
        .bind(decision.txid)
        .bind(i32::try_from(decision.output_index).map_err(Error::ConversionDatabaseInt)?)
        .bind(decision.signer_pub_key)
        .bind(decision.can_accept)
        .bind(decision.can_sign)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_withdrawal_signer_decision(
        &self,
        decision: &model::WithdrawalSigner,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO sbtc_signer.withdrawal_signers
              ( request_id
              , txid
              , block_hash
              , signer_pub_key
              , is_accepted
              )
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT DO NOTHING",
        )
        .bind(i64::try_from(decision.request_id).map_err(Error::ConversionDatabaseInt)?)
        .bind(decision.txid)
        .bind(decision.block_hash)
        .bind(decision.signer_pub_key)
        .bind(decision.is_accepted)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_bitcoin_transaction(&self, tx_ref: &model::BitcoinTxRef) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO sbtc_signer.bitcoin_transactions (txid, block_hash)
            VALUES ($1, $2)
            ON CONFLICT DO NOTHING",
        )
        .bind(tx_ref.txid)
        .bind(tx_ref.block_hash)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_bitcoin_transactions(&self, txs: Vec<model::BitcoinTxRef>) -> Result<(), Error> {
        if txs.is_empty() {
            return Ok(());
        }

        let mut tx_ids = Vec::with_capacity(txs.len());
        let mut block_hashes = Vec::with_capacity(txs.len());

        for tx in txs {
            tx_ids.push(tx.txid);
            block_hashes.push(tx.block_hash)
        }

        sqlx::query(
            r#"
            WITH tx_ids AS (
                SELECT ROW_NUMBER() OVER (), txid
                FROM UNNEST($1::bytea[]) AS txid
            )
            , block_ids AS (
                SELECT ROW_NUMBER() OVER (), block_id
                FROM UNNEST($2::bytea[]) AS block_id
            )
            INSERT INTO sbtc_signer.bitcoin_transactions (txid, block_hash)
            SELECT
                txid
              , block_id
            FROM tx_ids
            JOIN block_ids USING (row_number)
            ON CONFLICT DO NOTHING"#,
        )
        .bind(&tx_ids)
        .bind(&block_hashes)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_stacks_block_headers(
        &self,
        blocks: Vec<model::StacksBlock>,
    ) -> Result<(), Error> {
        if blocks.is_empty() {
            return Ok(());
        }

        let mut block_ids = Vec::with_capacity(blocks.len());
        let mut parent_block_ids = Vec::with_capacity(blocks.len());
        let mut chain_lengths = Vec::<i64>::with_capacity(blocks.len());
        let mut bitcoin_anchors = Vec::with_capacity(blocks.len());

        for block in blocks {
            block_ids.push(block.block_hash);
            parent_block_ids.push(block.parent_hash);
            let block_height =
                i64::try_from(block.block_height).map_err(Error::ConversionDatabaseInt)?;
            chain_lengths.push(block_height);
            bitcoin_anchors.push(block.bitcoin_anchor);
        }

        sqlx::query(
            r#"
            WITH block_ids AS (
                SELECT ROW_NUMBER() OVER (), block_id
                FROM UNNEST($1::bytea[]) AS block_id
            )
            , parent_block_ids AS (
                SELECT ROW_NUMBER() OVER (), parent_block_id
                FROM UNNEST($2::bytea[]) AS parent_block_id
            )
            , chain_lengths AS (
                SELECT ROW_NUMBER() OVER (), chain_length
                FROM UNNEST($3::bigint[]) AS chain_length
            )
            , bitcoin_anchors AS (
                SELECT ROW_NUMBER() OVER (), bitcoin_anchor
                FROM UNNEST($4::bytea[]) AS bitcoin_anchor
            )
            INSERT INTO sbtc_signer.stacks_blocks (block_hash, block_height, parent_hash, bitcoin_anchor)
            SELECT
                block_id
              , chain_length
              , parent_block_id
              , bitcoin_anchor
            FROM block_ids
            JOIN parent_block_ids USING (row_number)
            JOIN chain_lengths USING (row_number)
            JOIN bitcoin_anchors USING (row_number)
            ON CONFLICT DO NOTHING"#,
        )
        .bind(&block_ids)
        .bind(&parent_block_ids)
        .bind(&chain_lengths)
        .bind(&bitcoin_anchors)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_encrypted_dkg_shares(
        &self,
        shares: &model::EncryptedDkgShares,
    ) -> Result<(), Error> {
        let started_at_bitcoin_block_height = i64::try_from(shares.started_at_bitcoin_block_height)
            .map_err(Error::ConversionDatabaseInt)?;

        sqlx::query(
            r#"
            INSERT INTO sbtc_signer.dkg_shares (
                aggregate_key
              , tweaked_aggregate_key
              , encrypted_private_shares
              , public_shares
              , script_pubkey
              , signer_set_public_keys
              , signature_share_threshold
              , dkg_shares_status
              , started_at_bitcoin_block_hash
              , started_at_bitcoin_block_height
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT DO NOTHING"#,
        )
        .bind(shares.aggregate_key)
        .bind(shares.tweaked_aggregate_key)
        .bind(&shares.encrypted_private_shares)
        .bind(&shares.public_shares)
        .bind(&shares.script_pubkey)
        .bind(&shares.signer_set_public_keys)
        .bind(i32::from(shares.signature_share_threshold))
        .bind(shares.dkg_shares_status)
        .bind(shares.started_at_bitcoin_block_hash)
        .bind(started_at_bitcoin_block_height)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_rotate_keys_transaction(
        &self,
        key_rotation: &model::KeyRotationEvent,
    ) -> Result<(), Error> {
        sqlx::query(
            r#"
            INSERT INTO sbtc_signer.rotate_keys_transactions (
                  txid
                , block_hash
                , address
                , aggregate_key
                , signer_set
                , signatures_required)
            VALUES
                ($1, $2, $3, $4, $5, $6)
            ON CONFLICT DO NOTHING"#,
        )
        .bind(key_rotation.txid)
        .bind(key_rotation.block_hash)
        .bind(&key_rotation.address)
        .bind(key_rotation.aggregate_key)
        .bind(&key_rotation.signer_set)
        .bind(i32::from(key_rotation.signatures_required))
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_completed_deposit_event(
        &self,
        event: &CompletedDepositEvent,
    ) -> Result<(), Error> {
        sqlx::query(
            "
        INSERT INTO sbtc_signer.completed_deposit_events (
            txid
          , block_hash
          , amount
          , bitcoin_txid
          , output_index
          , sweep_block_hash
          , sweep_block_height
          , sweep_txid
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(event.txid)
        .bind(event.block_id)
        .bind(i64::try_from(event.amount).map_err(Error::ConversionDatabaseInt)?)
        .bind(event.outpoint.txid.to_byte_array())
        .bind(i64::from(event.outpoint.vout))
        .bind(event.sweep_block_hash.to_byte_array())
        .bind(i64::try_from(event.sweep_block_height).map_err(Error::ConversionDatabaseInt)?)
        .bind(event.sweep_txid.to_byte_array())
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_withdrawal_accept_event(
        &self,
        event: &WithdrawalAcceptEvent,
    ) -> Result<(), Error> {
        sqlx::query(
            "
        INSERT INTO sbtc_signer.withdrawal_accept_events (
            txid
          , block_hash
          , request_id
          , signer_bitmap
          , bitcoin_txid
          , output_index
          , fee
          , sweep_block_hash
          , sweep_block_height
          , sweep_txid
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(event.txid)
        .bind(event.block_id)
        .bind(i64::try_from(event.request_id).map_err(Error::ConversionDatabaseInt)?)
        .bind(event.signer_bitmap.into_inner())
        .bind(event.outpoint.txid.to_byte_array())
        .bind(i64::from(event.outpoint.vout))
        .bind(i64::try_from(event.fee).map_err(Error::ConversionDatabaseInt)?)
        .bind(event.sweep_block_hash.to_byte_array())
        .bind(i64::try_from(event.sweep_block_height).map_err(Error::ConversionDatabaseInt)?)
        .bind(event.sweep_txid.to_byte_array())
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_withdrawal_reject_event(
        &self,
        event: &WithdrawalRejectEvent,
    ) -> Result<(), Error> {
        sqlx::query(
            "
        INSERT INTO sbtc_signer.withdrawal_reject_events (
            txid
          , block_hash
          , request_id
          , signer_bitmap
        )
        VALUES ($1, $2, $3, $4)",
        )
        .bind(event.txid)
        .bind(event.block_id)
        .bind(i64::try_from(event.request_id).map_err(Error::ConversionDatabaseInt)?)
        .bind(event.signer_bitmap.into_inner())
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_tx_output(&self, output: &model::TxOutput) -> Result<(), Error> {
        sqlx::query(
            r#"
            INSERT INTO bitcoin_tx_outputs (
                txid
              , output_index
              , amount
              , script_pubkey
              , output_type
            )
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT DO NOTHING;
            "#,
        )
        .bind(output.txid)
        .bind(i32::try_from(output.output_index).map_err(Error::ConversionDatabaseInt)?)
        .bind(i64::try_from(output.amount).map_err(Error::ConversionDatabaseInt)?)
        .bind(&output.script_pubkey)
        .bind(output.output_type)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_withdrawal_tx_output(
        &self,
        output: &model::WithdrawalTxOutput,
    ) -> Result<(), Error> {
        sqlx::query(
            r#"
            INSERT INTO bitcoin_withdrawal_tx_outputs (
                txid
              , output_index
              , request_id
            )
            VALUES ($1, $2, $3)
            ON CONFLICT DO NOTHING;
            "#,
        )
        .bind(output.txid)
        .bind(i32::try_from(output.output_index).map_err(Error::ConversionDatabaseInt)?)
        .bind(i64::try_from(output.request_id).map_err(Error::ConversionDatabaseInt)?)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_tx_prevout(&self, prevout: &model::TxPrevout) -> Result<(), Error> {
        sqlx::query(
            r#"
            INSERT INTO bitcoin_tx_inputs (
                txid
              , prevout_txid
              , prevout_output_index
              , amount
              , script_pubkey
              , prevout_type
            )
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT DO NOTHING;
            "#,
        )
        .bind(prevout.txid)
        .bind(prevout.prevout_txid)
        .bind(i32::try_from(prevout.prevout_output_index).map_err(Error::ConversionDatabaseInt)?)
        .bind(i64::try_from(prevout.amount).map_err(Error::ConversionDatabaseInt)?)
        .bind(&prevout.script_pubkey)
        .bind(prevout.prevout_type)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_bitcoin_txs_sighashes(
        &self,
        sighashes: &[model::BitcoinTxSigHash],
    ) -> Result<(), Error> {
        if sighashes.is_empty() {
            return Ok(());
        }

        let mut txid = Vec::with_capacity(sighashes.len());
        let mut chain_tip = Vec::with_capacity(sighashes.len());
        let mut prevout_txid = Vec::with_capacity(sighashes.len());
        let mut prevout_output_index = Vec::with_capacity(sighashes.len());
        let mut sighash = Vec::with_capacity(sighashes.len());
        let mut prevout_type = Vec::with_capacity(sighashes.len());
        let mut validation_result = Vec::with_capacity(sighashes.len());
        let mut is_valid_tx = Vec::with_capacity(sighashes.len());
        let mut will_sign = Vec::with_capacity(sighashes.len());
        let mut aggregate_key = Vec::with_capacity(sighashes.len());

        for tx_sighash in sighashes {
            txid.push(tx_sighash.txid);
            chain_tip.push(tx_sighash.chain_tip);
            prevout_txid.push(tx_sighash.prevout_txid);
            prevout_output_index.push(
                i32::try_from(tx_sighash.prevout_output_index)
                    .map_err(Error::ConversionDatabaseInt)?,
            );
            sighash.push(tx_sighash.sighash);
            prevout_type.push(tx_sighash.prevout_type);
            validation_result.push(tx_sighash.validation_result);
            is_valid_tx.push(tx_sighash.is_valid_tx);
            will_sign.push(tx_sighash.will_sign);
            aggregate_key.push(tx_sighash.aggregate_key);
        }

        sqlx::query(
            r#"
            WITH tx_ids             AS (SELECT ROW_NUMBER() OVER (), txid FROM UNNEST($1::BYTEA[]) AS txid)
            , chain_tip             AS (SELECT ROW_NUMBER() OVER (), chain_tip FROM UNNEST($2::BYTEA[]) AS chain_tip)
            , prevout_txid          AS (SELECT ROW_NUMBER() OVER (), prevout_txid FROM UNNEST($3::BYTEA[]) AS prevout_txid)
            , prevout_output_index  AS (SELECT ROW_NUMBER() OVER (), prevout_output_index FROM UNNEST($4::INTEGER[]) AS prevout_output_index)
            , sighash               AS (SELECT ROW_NUMBER() OVER (), sighash FROM UNNEST($5::BYTEA[]) AS sighash)
            , prevout_type          AS (SELECT ROW_NUMBER() OVER (), prevout_type FROM UNNEST($6::sbtc_signer.prevout_type[]) AS prevout_type)
            , validation_result     AS (SELECT ROW_NUMBER() OVER (), validation_result FROM UNNEST($7::TEXT[]) AS validation_result)
            , is_valid_tx           AS (SELECT ROW_NUMBER() OVER (), is_valid_tx FROM UNNEST($8::BOOLEAN[]) AS is_valid_tx)
            , will_sign             AS (SELECT ROW_NUMBER() OVER (), will_sign FROM UNNEST($9::BOOLEAN[]) AS will_sign)
            , x_only_public_key     AS (SELECT ROW_NUMBER() OVER (), x_only_public_key FROM UNNEST($10::BYTEA[]) AS x_only_public_key)
            INSERT INTO sbtc_signer.bitcoin_tx_sighashes (
                  txid
                , chain_tip
                , prevout_txid
                , prevout_output_index
                , sighash
                , prevout_type
                , validation_result
                , is_valid_tx
                , will_sign
                , x_only_public_key
            )
            SELECT
                txid
              , chain_tip
              , prevout_txid
              , prevout_output_index
              , sighash
              , prevout_type
              , validation_result
              , is_valid_tx
              , will_sign
              , x_only_public_key
            FROM tx_ids
            JOIN chain_tip USING (row_number)
            JOIN prevout_txid USING (row_number)
            JOIN prevout_output_index USING (row_number)
            JOIN sighash USING (row_number)
            JOIN prevout_type USING (row_number)
            JOIN validation_result USING (row_number)
            JOIN is_valid_tx USING (row_number)
            JOIN will_sign USING (row_number)
            JOIN x_only_public_key USING (row_number)
            ON CONFLICT DO NOTHING"#,
        )
        .bind(txid)
        .bind(chain_tip)
        .bind(prevout_txid)
        .bind(prevout_output_index)
        .bind(sighash)
        .bind(prevout_type)
        .bind(validation_result)
        .bind(is_valid_tx)
        .bind(will_sign)
        .bind(aggregate_key)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn write_bitcoin_withdrawals_outputs(
        &self,
        withdrawal_outputs: &[model::BitcoinWithdrawalOutput],
    ) -> Result<(), Error> {
        if withdrawal_outputs.is_empty() {
            return Ok(());
        }

        let mut bitcoin_txid = Vec::with_capacity(withdrawal_outputs.len());
        let mut bitcoin_chain_tip = Vec::with_capacity(withdrawal_outputs.len());
        let mut request_id = Vec::with_capacity(withdrawal_outputs.len());
        let mut output_index = Vec::with_capacity(withdrawal_outputs.len());
        let mut stacks_txid = Vec::with_capacity(withdrawal_outputs.len());
        let mut stacks_block_hash = Vec::with_capacity(withdrawal_outputs.len());
        let mut validation_result = Vec::with_capacity(withdrawal_outputs.len());
        let mut is_valid_tx = Vec::with_capacity(withdrawal_outputs.len());

        for withdrawal_output in withdrawal_outputs {
            bitcoin_txid.push(withdrawal_output.bitcoin_txid);
            bitcoin_chain_tip.push(withdrawal_output.bitcoin_chain_tip);
            output_index.push(
                i32::try_from(withdrawal_output.output_index)
                    .map_err(Error::ConversionDatabaseInt)?,
            );
            request_id.push(
                i64::try_from(withdrawal_output.request_id)
                    .map_err(Error::ConversionDatabaseInt)?,
            );
            stacks_txid.push(withdrawal_output.stacks_txid);
            stacks_block_hash.push(withdrawal_output.stacks_block_hash);
            validation_result.push(withdrawal_output.validation_result);
            is_valid_tx.push(withdrawal_output.is_valid_tx);
        }

        sqlx::query(
            r#"
            WITH bitcoin_tx_ids     AS (SELECT ROW_NUMBER() OVER (), bitcoin_txid FROM UNNEST($1::BYTEA[]) AS bitcoin_txid)
            , bitcoin_chain_tip     AS (SELECT ROW_NUMBER() OVER (), bitcoin_chain_tip FROM UNNEST($2::BYTEA[]) AS bitcoin_chain_tip)
            , output_index          AS (SELECT ROW_NUMBER() OVER (), output_index FROM UNNEST($3::INTEGER[]) AS output_index)
            , request_id            AS (SELECT ROW_NUMBER() OVER (), request_id FROM UNNEST($4::BIGINT[]) AS request_id)
            , stacks_txid           AS (SELECT ROW_NUMBER() OVER (), stacks_txid FROM UNNEST($5::BYTEA[]) AS stacks_txid)
            , stacks_block_hash     AS (SELECT ROW_NUMBER() OVER (), stacks_block_hash FROM UNNEST($6::BYTEA[]) AS stacks_block_hash)
            , validation_result     AS (SELECT ROW_NUMBER() OVER (), validation_result FROM UNNEST($7::TEXT[]) AS validation_result)
            , is_valid_tx           AS (SELECT ROW_NUMBER() OVER (), is_valid_tx FROM UNNEST($8::BOOLEAN[]) AS is_valid_tx)
            INSERT INTO sbtc_signer.bitcoin_withdrawals_outputs (
                  bitcoin_txid
                , bitcoin_chain_tip
                , output_index
                , request_id
                , stacks_txid
                , stacks_block_hash
                , validation_result
                , is_valid_tx)
            SELECT
                bitcoin_txid
              , bitcoin_chain_tip
              , output_index
              , request_id
              , stacks_txid
              , stacks_block_hash
              , validation_result
              , is_valid_tx
            FROM bitcoin_tx_ids
            JOIN bitcoin_chain_tip USING (row_number)
            JOIN output_index USING (row_number)
            JOIN request_id USING (row_number)
            JOIN stacks_txid USING (row_number)
            JOIN stacks_block_hash USING (row_number)
            JOIN validation_result USING (row_number)
            JOIN is_valid_tx USING (row_number)
            ON CONFLICT DO NOTHING"#,
        )
        .bind(bitcoin_txid)
        .bind(bitcoin_chain_tip)
        .bind(output_index)
        .bind(request_id)
        .bind(stacks_txid)
        .bind(stacks_block_hash)
        .bind(validation_result)
        .bind(is_valid_tx)
        .execute(&self.0)
        .await
        .map_err(Error::SqlxQuery)?;

        Ok(())
    }

    async fn revoke_dkg_shares<X>(&self, aggregate_key: X) -> Result<bool, Error>
    where
        X: Into<PublicKeyXOnly> + Send,
    {
        sqlx::query(
            r#"
            UPDATE sbtc_signer.dkg_shares
            SET dkg_shares_status = 'failed'
            WHERE substring(aggregate_key FROM 2) = $1
              AND dkg_shares_status = 'unverified'; -- only allow failing pending entries
            "#,
        )
        .bind(aggregate_key.into())
        .execute(&self.0)
        .await
        .map(|res| res.rows_affected() > 0)
        .map_err(Error::SqlxQuery)
    }

    async fn verify_dkg_shares<X>(&self, aggregate_key: X) -> Result<bool, Error>
    where
        X: Into<PublicKeyXOnly> + Send,
    {
        sqlx::query(
            r#"
            UPDATE sbtc_signer.dkg_shares
            SET dkg_shares_status = 'verified'
            WHERE substring(aggregate_key FROM 2) = $1
              AND dkg_shares_status = 'unverified'; -- only allow verifying pending entries
            "#,
        )
        .bind(aggregate_key.into())
        .execute(&self.0)
        .await
        .map(|res| res.rows_affected() > 0)
        .map_err(Error::SqlxQuery)
    }
}
