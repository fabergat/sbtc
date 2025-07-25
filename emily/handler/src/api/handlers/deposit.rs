//! Handlers for Deposit endpoints.
use bitcoin::ScriptBuf;
use bitcoin::opcodes::all::{self as opcodes};
use sbtc::deposits::ReclaimScriptInputs;
use sha2::{Digest, Sha256};
use stacks_common::codec::StacksMessageCodec as _;
use tracing::instrument;
use warp::http::StatusCode;
use warp::reply::{Reply, json, with_status};

use crate::api::models::common::DepositStatus;
use crate::api::models::common::requests::BasicPaginationQuery;
use crate::api::models::deposit::responses::{
    DepositWithStatus, GetDepositsForTransactionResponse, UpdateDepositsResponse,
};
use crate::api::models::deposit::{Deposit, DepositInfo};
use crate::api::models::{
    deposit::requests::{
        CreateDepositRequestBody, GetDepositsForTransactionQuery, GetDepositsQuery,
        UpdateDepositsRequestBody,
    },
    deposit::responses::GetDepositsResponse,
};
use crate::common::error::Error;
use crate::context::EmilyContext;
use crate::database::accessors;
use crate::database::entries::DepositStatusEntry;
use crate::database::entries::chainstate::ApiStateEntry;
use crate::database::entries::deposit::{
    DepositEntry, DepositEntryKey, DepositEvent, DepositParametersEntry,
    ValidatedUpdateDepositsRequest,
};

/// Get deposit handler.
#[utoipa::path(
    get,
    operation_id = "getDeposit",
    path = "/deposit/{txid}/{index}",
    params(
        ("txid" = String, Path, description = "txid associated with the Deposit."),
        ("index" = String, Path, description = "output index associated with the Deposit."),
    ),
    tag = "deposit",
    responses(
        (status = 200, description = "Deposit retrieved successfully", body = Deposit),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 404, description = "Address not found", body = ErrorResponse),
        (status = 405, description = "Method not allowed", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
#[instrument(skip(context))]
pub async fn get_deposit(
    context: EmilyContext,
    bitcoin_txid: String,
    bitcoin_tx_output_index: u32,
) -> impl warp::reply::Reply {
    tracing::debug!("in get deposit");
    // Internal handler so `?` can be used correctly while still returning a reply.
    async fn handler(
        context: EmilyContext,
        bitcoin_txid: String,
        bitcoin_tx_output_index: u32,
    ) -> Result<impl warp::reply::Reply, Error> {
        // Make key.
        let key = DepositEntryKey {
            bitcoin_txid,
            bitcoin_tx_output_index,
        };
        // Get deposit.
        let deposit: Deposit = accessors::get_deposit_entry(&context, &key)
            .await?
            .try_into()?;

        // Respond.
        Ok(with_status(json(&deposit), StatusCode::OK))
    }

    // Handle and respond.
    handler(context, bitcoin_txid, bitcoin_tx_output_index)
        .await
        .map_or_else(Reply::into_response, Reply::into_response)
}

/// Get deposits for transaction handler.
#[utoipa::path(
    get,
    operation_id = "getDepositsForTransaction",
    path = "/deposit/{txid}",
    params(
        ("txid" = String, Path, description = "txid associated with the Deposit."),
        ("nextToken" = Option<String>, Query, description = "the next token value from the previous return of this api call."),
        ("pageSize" = Option<u16>, Query, description = "the maximum number of items in the response list.")
    ),
    tag = "deposit",
    responses(
        (status = 200, description = "Deposits retrieved successfully", body = GetDepositsForTransactionResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 404, description = "Address not found", body = ErrorResponse),
        (status = 405, description = "Method not allowed", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
#[instrument(skip(context))]
pub async fn get_deposits_for_transaction(
    context: EmilyContext,
    bitcoin_txid: String,
    query: GetDepositsForTransactionQuery,
) -> impl warp::reply::Reply {
    tracing::debug!("in get deposits for transaction");
    // Internal handler so `?` can be used correctly while still returning a reply.
    async fn handler(
        context: EmilyContext,
        bitcoin_txid: String,
        query: GetDepositsForTransactionQuery,
    ) -> Result<impl warp::reply::Reply, Error> {
        // TODO(506): Reverse this order of deposits so that the transactions are returned
        // in ascending index order.
        let (entries, next_token) = accessors::get_deposit_entries_for_transaction(
            &context,
            &bitcoin_txid,
            query.next_token,
            query.page_size,
        )
        .await?;
        // Get deposits as the right type.
        let deposits: Vec<Deposit> = entries
            .into_iter()
            .map(|entry| entry.try_into())
            .collect::<Result<_, _>>()?;
        // Create response.
        let response = GetDepositsForTransactionResponse { deposits, next_token };
        // Respond.
        Ok(with_status(json(&response), StatusCode::OK))
    }
    // Handle and respond.
    handler(context, bitcoin_txid, query)
        .await
        .map_or_else(Reply::into_response, Reply::into_response)
}

/// Get deposits handler.
#[utoipa::path(
    get,
    operation_id = "getDeposits",
    path = "/deposit",
    params(
        ("status" = DepositStatus, Query, description = "the status to search by when getting all deposits."),
        ("nextToken" = Option<String>, Query, description = "the next token value from the previous return of this api call."),
        ("pageSize" = Option<u16>, Query, description = "the maximum number of items in the response list.")
    ),
    tag = "deposit",
    responses(
        (status = 200, description = "Deposits retrieved successfully", body = GetDepositsResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 404, description = "Address not found", body = ErrorResponse),
        (status = 405, description = "Method not allowed", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
#[instrument(skip(context))]
pub async fn get_deposits(
    context: EmilyContext,
    query: GetDepositsQuery,
) -> impl warp::reply::Reply {
    tracing::debug!("in get deposits");
    // Internal handler so `?` can be used correctly while still returning a reply.
    async fn handler(
        context: EmilyContext,
        query: GetDepositsQuery,
    ) -> Result<impl warp::reply::Reply, Error> {
        // Deserialize next token into the exclusive start key if present/
        let (entries, next_token) = accessors::get_deposit_entries(
            &context,
            &query.status,
            query.next_token,
            query.page_size,
        )
        .await?;
        // Convert data into resource types.
        let deposits: Vec<DepositInfo> = entries.into_iter().map(|entry| entry.into()).collect();
        // Create response.
        let response = GetDepositsResponse { deposits, next_token };
        // Respond.
        Ok(with_status(json(&response), StatusCode::OK))
    }
    // Handle and respond.
    handler(context, query)
        .await
        .map_or_else(Reply::into_response, Reply::into_response)
}

/// Get deposits by recipient handler.
#[utoipa::path(
    get,
    operation_id = "getDepositsForRecipient",
    path = "/deposit/recipient/{recipient}",
    params(
        ("recipient" = String, Path, description = "the recipient to search by when getting all deposits."),
        ("nextToken" = Option<String>, Query, description = "the next token value from the previous return of this api call."),
        ("pageSize" = Option<u16>, Query, description = "the maximum number of items in the response list.")
    ),
    tag = "deposit",
    responses(
        (status = 200, description = "Deposits retrieved successfully", body = GetDepositsResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 404, description = "Address not found", body = ErrorResponse),
        (status = 405, description = "Method not allowed", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
#[instrument(skip(context))]
pub async fn get_deposits_for_recipient(
    context: EmilyContext,
    recipient: String,
    query: BasicPaginationQuery,
) -> impl warp::reply::Reply {
    tracing::debug!("in get deposits for recipient: {recipient}");
    // Internal handler so `?` can be used correctly while still returning a reply.
    async fn handler(
        context: EmilyContext,
        recipient: String,
        query: BasicPaginationQuery,
    ) -> Result<impl warp::reply::Reply, Error> {
        let (entries, next_token) = accessors::get_deposit_entries_by_recipient(
            &context,
            &recipient,
            query.next_token,
            query.page_size,
        )
        .await?;
        // Convert data into resource types.
        let deposits: Vec<DepositInfo> = entries.into_iter().map(|entry| entry.into()).collect();
        // Create response.
        let response = GetDepositsResponse { deposits, next_token };
        // Respond.
        Ok(with_status(json(&response), StatusCode::OK))
    }
    // Handle and respond.
    handler(context, recipient, query)
        .await
        .map_or_else(Reply::into_response, Reply::into_response)
}

/// Get deposits by recipient handler.
#[utoipa::path(
    get,
    operation_id = "getDepositsForReclaimPubkeys",
    path = "/deposit/reclaim-pubkeys/{reclaimPubkeys}",
    params(
        ("reclaimPubkeys" = String, Path, description = "The dash-separated list of hex-encoded x-only pubkeys used to generate the reclaim_script."),
        ("nextToken" = Option<String>, Query, description = "the next token value from the previous return of this api call."),
        ("pageSize" = Option<u16>, Query, description = "the maximum number of items in the response list.")
    ),
    tag = "deposit",
    responses(
        (status = 200, description = "Deposits retrieved successfully", body = GetDepositsResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 404, description = "Address not found", body = ErrorResponse),
        (status = 405, description = "Method not allowed", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
#[instrument(skip(context))]
pub async fn get_deposits_for_reclaim_pubkeys(
    context: EmilyContext,
    reclaim_pubkeys: String,
    query: BasicPaginationQuery,
) -> impl warp::reply::Reply {
    tracing::debug!("in get deposits for reclaim pubkey: {reclaim_pubkeys}");
    // Internal handler so `?` can be used correctly while still returning a reply.
    async fn handler(
        context: EmilyContext,
        reclaim_pubkeys: String,
        query: BasicPaginationQuery,
    ) -> Result<impl warp::reply::Reply, Error> {
        let reclaim_pubkeys_bytes = validate_reclaim_pubkeys(&reclaim_pubkeys)?;
        let reclaim_pubkeys_hash = sorted_sha256(reclaim_pubkeys_bytes);
        let (entries, next_token) = accessors::get_deposit_entries_by_reclaim_pubkeys_hash(
            &context,
            &reclaim_pubkeys_hash,
            query.next_token,
            query.page_size,
        )
        .await?;
        // Convert data into resource types.
        let deposits: Vec<DepositInfo> = entries.into_iter().map(|entry| entry.into()).collect();
        // Create response.
        let response = GetDepositsResponse { deposits, next_token };
        // Respond.
        Ok(with_status(json(&response), StatusCode::OK))
    }
    // Handle and respond.
    handler(context, reclaim_pubkeys, query)
        .await
        .map_or_else(Reply::into_response, Reply::into_response)
}

/// Create deposit handler.
#[utoipa::path(
    post,
    operation_id = "createDeposit",
    path = "/deposit",
    tag = "deposit",
    request_body = CreateDepositRequestBody,
    responses(
        (status = 200, description = "Deposit already exists", body = Deposit),
        (status = 201, description = "Deposit created successfully", body = Deposit),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 404, description = "Address not found", body = ErrorResponse),
        (status = 405, description = "Method not allowed", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
#[instrument(skip(context))]
pub async fn create_deposit(
    context: EmilyContext,
    body: CreateDepositRequestBody,
) -> impl warp::reply::Reply {
    tracing::debug!(
        bitcoin_txid = %body.bitcoin_txid,
        bitcoin_tx_output_index = body.bitcoin_tx_output_index,
        "creating deposit"
    );
    // Internal handler so `?` can be used correctly while still returning a reply.
    async fn handler(
        context: EmilyContext,
        body: CreateDepositRequestBody,
    ) -> Result<impl warp::reply::Reply, Error> {
        // Set variables.
        let api_state = accessors::get_api_state(&context).await?;
        api_state.error_if_reorganizing()?;

        let chaintip = api_state.chaintip();
        let stacks_block_hash = chaintip.key.hash;
        let stacks_block_height = chaintip.key.height;

        let deposit_info = body.validate(context.settings.is_mainnet)?;

        // Check if deposit with such txid and outindex already exists.
        let entry = accessors::get_deposit_entry(
            &context,
            &DepositEntryKey {
                bitcoin_txid: body.bitcoin_txid.clone(),
                bitcoin_tx_output_index: body.bitcoin_tx_output_index,
            },
        )
        .await;

        match entry {
            Ok(deposit_entry) => {
                // The deposit already exists, return it.
                let response: Deposit = deposit_entry.try_into()?;
                return Ok(with_status(json(&response), StatusCode::OK));
            }
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        let reclaim_pubkeys_hash = extract_reclaim_pubkeys_hash(&deposit_info.reclaim_script);
        if reclaim_pubkeys_hash.is_none() {
            tracing::warn!(
                bitcoin_txid = %body.bitcoin_txid,
                bitcoin_tx_output_index = %body.bitcoin_tx_output_index,
                "unknown reclaim script"
            );
        }
        // Make table entry.
        let deposit_entry: DepositEntry = DepositEntry {
            key: DepositEntryKey {
                bitcoin_txid: body.bitcoin_txid,
                bitcoin_tx_output_index: body.bitcoin_tx_output_index,
            },
            recipient: hex::encode(deposit_info.recipient.serialize_to_vec()),
            parameters: DepositParametersEntry {
                max_fee: deposit_info.max_fee,
                lock_time: deposit_info.lock_time.to_consensus_u32(),
            },
            history: vec![DepositEvent {
                status: DepositStatusEntry::Pending,
                message: "Just received deposit".to_string(),
                stacks_block_hash: stacks_block_hash.clone(),
                stacks_block_height,
            }],
            status: DepositStatus::Pending,
            last_update_block_hash: stacks_block_hash,
            last_update_height: stacks_block_height,
            amount: deposit_info.amount,
            reclaim_script: body.reclaim_script,
            deposit_script: body.deposit_script,
            reclaim_pubkeys_hash,
            ..Default::default()
        };
        // Validate deposit entry.
        deposit_entry.validate()?;
        // Add entry to the table.
        accessors::add_deposit_entry(&context, &deposit_entry).await?;
        // Respond.
        let response: Deposit = deposit_entry.try_into()?;
        Ok(with_status(json(&response), StatusCode::CREATED))
    }
    // Handle and respond.
    handler(context, body)
        .await
        .map_or_else(Reply::into_response, Reply::into_response)
}

/// Update deposits handler.
#[utoipa::path(
    put,
    operation_id = "updateDepositsSigner",
    path = "/deposit",
    tag = "deposit",
    request_body = UpdateDepositsRequestBody,
    responses(
        (status = 200, description = "Deposits updated successfully", body = UpdateDepositsResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Address not found", body = ErrorResponse),
        (status = 405, description = "Method not allowed", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("ApiGatewayKey" = []))
)]
#[instrument(skip(context))]
pub async fn update_deposits_signer(
    context: EmilyContext,
    body: UpdateDepositsRequestBody,
) -> impl warp::reply::Reply {
    tracing::debug!("in update deposits");
    // Internal handler so `?` can be used correctly while still returning a reply.
    async fn handler(
        context: EmilyContext,
        body: UpdateDepositsRequestBody,
    ) -> Result<impl warp::reply::Reply, Error> {
        // Get the api state and error if the api state is claimed by a reorg.
        //
        // Note: This may not be necessary due to the implied order of events
        // that the API can receive from stacks nodes, but it's being added here
        // in order to enforce added stability to the API during a reorg.
        let api_state = accessors::get_api_state(&context).await?;
        api_state.error_if_reorganizing()?;

        update_deposits(api_state, context, body, false).await
    }
    // Handle and respond.
    handler(context, body)
        .await
        .map_or_else(Reply::into_response, Reply::into_response)
}

/// Update deposits handler.
#[utoipa::path(
    put,
    operation_id = "updateDepositsSidecar",
    path = "/deposit_private",
    tag = "deposit",
    request_body = UpdateDepositsRequestBody,
    responses(
        (status = 200, description = "Deposits updated successfully", body = UpdateDepositsResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Address not found", body = ErrorResponse),
        (status = 405, description = "Method not allowed", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("ApiGatewayKey" = []))
)]
#[instrument(skip(context))]
pub async fn update_deposits_sidecar(
    context: EmilyContext,
    body: UpdateDepositsRequestBody,
) -> impl warp::reply::Reply {
    tracing::debug!("in update deposits");
    // Internal handler so `?` can be used correctly while still returning a reply.
    async fn handler(
        context: EmilyContext,
        body: UpdateDepositsRequestBody,
    ) -> Result<impl warp::reply::Reply, Error> {
        // Get the api state and error if the api state is claimed by a reorg.
        //
        // Note: This may not be necessary due to the implied order of events
        // that the API can receive from stacks nodes, but it's being added here
        // in order to enforce added stability to the API during a reorg.
        let api_state = accessors::get_api_state(&context).await?;
        api_state.error_if_reorganizing()?;

        update_deposits(api_state, context, body, true).await
    }
    // Handle and respond.
    handler(context, body)
        .await
        .map_or_else(Reply::into_response, Reply::into_response)
}

async fn update_deposits(
    api_state: ApiStateEntry,
    context: EmilyContext,
    body: UpdateDepositsRequestBody,
    is_from_trusted_source: bool,
) -> Result<impl warp::reply::Reply, Error> {
    // Validate request.
    let validated_request: ValidatedUpdateDepositsRequest =
        body.into_validated_update_request(api_state.chaintip().into());

    // Create aggregator.
    let mut updated_deposits: Vec<(usize, DepositWithStatus)> =
        Vec::with_capacity(validated_request.deposits.len());

    // Loop through all updates and execute.
    for (index, update) in validated_request.deposits {
        if let Err(error) = update {
            // This error is a ValidationError: it shouldn't contain any
            // sensitive information.
            updated_deposits.push((
                index,
                DepositWithStatus {
                    deposit: None,
                    error: Some(error.to_string()),
                    status: StatusCode::BAD_REQUEST.as_u16(),
                },
            ));
            continue;
        }
        let update = update.unwrap();

        let bitcoin_txid = update.key.bitcoin_txid.clone();
        let bitcoin_tx_output_index = update.key.bitcoin_tx_output_index;

        tracing::debug!(
            %bitcoin_txid,
            bitcoin_tx_output_index,
            "updating deposit"
        );

        let updated_deposit = match accessors::pull_and_update_deposit_with_retry(
            &context,
            update,
            15,
            is_from_trusted_source,
        )
        .await
        {
            Ok(updated_deposit) => updated_deposit,
            Err(Error::NotFound) => {
                tracing::warn!(
                    %bitcoin_txid,
                    bitcoin_tx_output_index,
                    "failed to update deposit. Deposit not found in the database"
                );
                updated_deposits.push((
                    index,
                    DepositWithStatus {
                        deposit: None,
                        error: Some(Error::NotFound.to_string()),
                        status: StatusCode::NOT_FOUND.as_u16(),
                    },
                ));
                continue;
            }
            Err(Error::Forbidden) => {
                tracing::warn!(
                    %bitcoin_txid,
                    bitcoin_tx_output_index,
                    "failed to update deposit. Such type of update is not allowed for the caller"
                );
                updated_deposits.push((
                    index,
                    DepositWithStatus {
                        deposit: None,
                        error: Some(Error::Forbidden.to_string()),
                        status: StatusCode::FORBIDDEN.as_u16(),
                    },
                ));
                continue;
            }
            Err(error) => {
                tracing::error!(
                    %bitcoin_txid,
                    bitcoin_tx_output_index,
                    %error,
                    "failed to update deposit"
                );
                updated_deposits.push((
                    index,
                    DepositWithStatus {
                        deposit: None,
                        error: Some(error.into_production_error().to_string()),
                        status: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    },
                ));
                continue;
            }
        };

        let deposit: Deposit = updated_deposit.try_into().inspect_err(|error| {
            // This should never happen, because the deposit was
            // validated before being updated.
            tracing::error!(
                %bitcoin_txid,
                bitcoin_tx_output_index,
                %error,
                "failed to convert deposit"
            );
        })?;
        updated_deposits.push((
            index,
            DepositWithStatus {
                error: None,
                deposit: Some(deposit),
                status: StatusCode::OK.as_u16(),
            },
        ));
    }

    updated_deposits.sort_by_key(|(index, _)| *index);
    let deposits: Vec<_> = updated_deposits
        .into_iter()
        .map(|(_, deposit)| deposit)
        .collect();
    let response = UpdateDepositsResponse { deposits };
    Ok(with_status(json(&response), StatusCode::OK))
}

const OP_DROP: u8 = opcodes::OP_DROP.to_u8();
const OP_CHECKSIG: u8 = opcodes::OP_CHECKSIG.to_u8();
const OP_CHECKSIGADD: u8 = opcodes::OP_CHECKSIGADD.to_u8();
const OP_NUMEQUAL: u8 = opcodes::OP_NUMEQUAL.to_u8();
const OP_PUSHBYTES_32: u8 = opcodes::OP_PUSHBYTES_32.to_u8();
const OP_PUSHNUM_1: u8 = opcodes::OP_PUSHNUM_1.to_u8();
const OP_PUSHNUM_16: u8 = opcodes::OP_PUSHNUM_16.to_u8();

/// Sort the pubkeys and hash them with sha256.
fn sorted_sha256(mut pubkeys: Vec<[u8; 32]>) -> String {
    pubkeys.sort();

    let mut hasher = Sha256::new();
    for pubkey in pubkeys {
        hasher.update(pubkey);
    }

    hex::encode(hasher.finalize())
}

/// Parse the reclaim script to extract the pubkeys and hash them with sha256 in
/// an order-independent way.
/// Currently supports the sBTC Bridge, Leather and Asigna reclaim scripts.
fn extract_reclaim_pubkeys_hash(reclaim_script: &ScriptBuf) -> Option<String> {
    let reclaim = ReclaimScriptInputs::parse(reclaim_script).ok()?;

    match reclaim.user_script().as_bytes() {
        // The reclaim script used by sBTC Bridge and Leather.
        [OP_DROP, OP_PUSHBYTES_32, pubkey @ .., OP_CHECKSIG] => Some(vec![pubkey.try_into().ok()?]),
        // The multi-sig reclaim script used by Asigna.
        [OP_DROP, keys_data @ .., OP_NUMEQUAL] => {
            // keys_data is a composed like below:
            // [OP_PUSHBYTES_32, pubkey1, OP_CHECKSIG,
            //  OP_PUSHBYTES_32, pubkey2, OP_CHECKSIGADD,
            //  ...
            //  OP_PUSHBYTES_32, pubkeyN, OP_CHECKSIGADD,
            //  OP_PUSHNUM_N]
            let mut data_iter = keys_data.iter();
            let mut pubkeys = Vec::new();
            while let Some(&opcode) = data_iter.next() {
                match opcode {
                    OP_PUSHBYTES_32 => {
                        // Collect the next 32 bytes
                        let pubkey_bytes: Vec<u8> = data_iter.by_ref().take(32).cloned().collect();
                        let pubkey_result: Result<[u8; 32], _> = pubkey_bytes.try_into();

                        match pubkey_result {
                            Ok(pubkey) => pubkeys.push(pubkey),
                            Err(_) => return None, // Malformed pubkey
                        }
                    }
                    OP_CHECKSIG | OP_CHECKSIGADD => continue, // Skip sig verification opcodes
                    OP_PUSHNUM_1..=OP_PUSHNUM_16 => break,    // End of pubkeys
                    _ => return None,                         // Unexpected opcode
                }
            }
            Some(pubkeys)
        }
        _ => None,
    }
    .map(sorted_sha256)
}

/// Parse a dash-separated list of hex-encoded pubkeys into a Vec<[u8; 32]>.
fn validate_reclaim_pubkeys(reclaim_pubkeys: &str) -> Result<Vec<[u8; 32]>, Error> {
    reclaim_pubkeys
        .split('-')
        .map(|s| {
            hex::decode(s)
                .map_err(|_| {
                    Error::HttpRequest(StatusCode::BAD_REQUEST, "invalid pubkey".to_string())
                })
                .and_then(|bytes| {
                    bytes.try_into().map_err(|_| {
                        Error::HttpRequest(StatusCode::BAD_REQUEST, "invalid pubkey".to_string())
                    })
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        key::rand::rngs::OsRng,
        secp256k1::{SECP256K1, SecretKey},
    };
    use test_case::test_case;

    fn make_reclaim_script(pubkey: &[u8; 32]) -> ScriptBuf {
        ScriptBuf::builder()
            .push_opcode(opcodes::OP_DROP)
            .push_slice(pubkey)
            .push_opcode(opcodes::OP_CHECKSIG)
            .into_script()
    }

    fn make_asigna_reclaim_script(pubkeys: &[[u8; 32]]) -> ScriptBuf {
        let mut pubkeys_iter = pubkeys.iter();
        let mut script = ScriptBuf::builder().push_opcode(opcodes::OP_DROP);
        script = script
            .push_slice(pubkeys_iter.next().unwrap())
            .push_opcode(opcodes::OP_CHECKSIG);

        for pubkey in pubkeys_iter {
            script = script
                .push_slice(pubkey)
                .push_opcode(opcodes::OP_CHECKSIGADD);
        }

        script = script
            .push_int(pubkeys.len() as i64)
            .push_opcode(opcodes::OP_NUMEQUAL);

        script.into_script()
    }

    #[tokio::test]
    async fn test_parse_bridge_reclaim_pubkey_two_ways() {
        let secret_key = SecretKey::new(&mut OsRng);
        let pubkey = secret_key.x_only_public_key(SECP256K1).0.serialize();
        let reclaim_script = ReclaimScriptInputs::try_new(14, make_reclaim_script(&pubkey))
            .unwrap()
            .reclaim_script();
        let pubkey_from_script = extract_reclaim_pubkeys_hash(&reclaim_script).unwrap();
        assert_eq!(pubkey_from_script, hex::encode(Sha256::digest(pubkey)));
    }

    #[tokio::test]
    async fn test_parse_asigna_reclaim_pubkey_two_ways() {
        let mut pubkeys: Vec<[u8; 32]> = (0..3)
            .map(|_| {
                SecretKey::new(&mut OsRng)
                    .x_only_public_key(SECP256K1)
                    .0
                    .serialize()
            })
            .collect();

        let reclaim_script = ReclaimScriptInputs::try_new(14, make_asigna_reclaim_script(&pubkeys))
            .unwrap()
            .reclaim_script();
        let pubkey_from_script = extract_reclaim_pubkeys_hash(&reclaim_script).unwrap();

        pubkeys.sort();
        let mut hasher = Sha256::new();
        for pubkey in &pubkeys {
            hasher.update(pubkey);
        }
        let expected_hash: String = hex::encode(hasher.finalize());
        assert_eq!(expected_hash, pubkey_from_script);
    }

    #[test_case(""; "empty")]
    #[test_case("-"; "empty-dash")]
    #[test_case("invalid"; "invalid-pubkey")]
    #[test_case("5da66963a375a1b994fbf695ddfa161954ffecdf67d80397650dcb4985f6a09c-"; "trailing-dash")]
    #[test_case("a66963a375a1b994fbf695ddfa161954ffecdf67d80397650dcb4985f6a09c"; "key-too-short")]
    #[test_case("035da66963a375a1b994fbf695ddfa161954ffecdf67d80397650dcb4985f6a09c"; "key-too-long")]
    #[test_case("5da66963a375a1b994fbf695ddfa161954ffecdf67d80397650dcb4985f6a09c-invalid"; "multi-keys-one-too-long")]
    #[tokio::test]
    async fn validate_reclaim_pubkeys_errors(input: &str) {
        let result = validate_reclaim_pubkeys(input);
        assert_eq!(
            result.unwrap_err().to_string(),
            "HTTP request failed with status code 400 Bad Request: invalid pubkey",
        );
    }

    #[test_case("5da66963a375a1b994fbf695ddfa161954ffecdf67d80397650dcb4985f6a09c", 1; "single-key")]
    #[test_case("5da66963a375a1b994fbf695ddfa161954ffecdf67d80397650dcb4985f6a09c-883a1b3f430eefac5bed7aa0d428e267a558736346363cbfec6b0e321e31f453",2; "multi-keys")]
    #[tokio::test]
    async fn validate_reclaim_pubkeys_happy_path(input: &str, num_keys: usize) {
        let result = validate_reclaim_pubkeys(input);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), num_keys);
    }

    #[test_case(vec![]; "empty")]
    #[test_case(vec![[1u8; 32]]; "single-key")]
    #[test_case(vec![[1u8; 32], [2u8; 32]]; "multi-keys")]
    #[tokio::test]
    async fn test_sorted_sha256(pubkeys: Vec<[u8; 32]>) {
        let mut expected = Sha256::new();
        for pubkey in &pubkeys {
            expected.update(pubkey);
        }
        let result: String = sorted_sha256(pubkeys);
        assert_eq!(result, hex::encode(expected.finalize()));
    }

    #[tokio::test]
    async fn test_sorted_sha256_multiple_keys_order_independant() {
        let pubkeys1: Vec<[u8; 32]> = vec![[2u8; 32], [1u8; 32]];
        let pubkeys2: Vec<[u8; 32]> = vec![[1u8; 32], [2u8; 32]];
        assert_eq!(sorted_sha256(pubkeys1), sorted_sha256(pubkeys2));
    }

    #[test_case(vec![[1u8; 32]]; "single-key")]
    #[test_case(vec![[2u8; 32], [1u8; 32]]; "multi-keys")]
    #[tokio::test]
    async fn test_validate_reclaim_pubkeys_hash_matches_extract_reclaim_pubkeys_hash(
        pubkeys: Vec<[u8; 32]>,
    ) {
        let pubkeys_hex: String = pubkeys
            .iter()
            .map(hex::encode)
            .collect::<Vec<String>>()
            .join("-");
        let validated_pubkeys = validate_reclaim_pubkeys(&pubkeys_hex).unwrap();
        let query_pubkeys_hash = sorted_sha256(validated_pubkeys);

        let user_script = match pubkeys.len() {
            1 => make_reclaim_script(pubkeys.first().unwrap()),
            _ => make_asigna_reclaim_script(&pubkeys),
        };
        let reclaim_script = ReclaimScriptInputs::try_new(14, user_script)
            .unwrap()
            .reclaim_script();
        let reclaim_pubkeys_hash = extract_reclaim_pubkeys_hash(&reclaim_script).unwrap();
        assert_eq!(query_pubkeys_hash, reclaim_pubkeys_hash);
    }
}

// TODO(393): Add handler unit tests.
