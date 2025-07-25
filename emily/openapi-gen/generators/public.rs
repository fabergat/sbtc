use emily_handler::api;
use emily_handler::common;

use super::AwsApiKey;
use super::AwsLambdaIntegration;
use super::CorsSupport;

#[derive(utoipa::OpenApi)]
#[openapi(
    // Add API key security scheme.
    modifiers(&CorsSupport, &AwsApiKey, &AwsLambdaIntegration),
    // Paths to be included in the OpenAPI specification.
    paths(
        // Health check endpoints.
        api::handlers::health::get_health,
        // Deposit endpoints.
        api::handlers::deposit::get_deposit,
        api::handlers::deposit::get_deposits_for_transaction,
        api::handlers::deposit::get_deposits_for_recipient,
        api::handlers::deposit::get_deposits_for_reclaim_pubkeys,
        api::handlers::deposit::get_deposits,
        api::handlers::deposit::create_deposit,
        api::handlers::deposit::update_deposits_signer,
        // Withdrawal endpoints.
        api::handlers::withdrawal::get_withdrawal,
        api::handlers::withdrawal::get_withdrawals,
        api::handlers::withdrawal::get_withdrawals_for_recipient,
        api::handlers::withdrawal::get_withdrawals_for_sender,
        api::handlers::withdrawal::update_withdrawals_signer,
        // Chainstate endpoints.
        api::handlers::chainstate::get_chain_tip,
        api::handlers::chainstate::get_chainstate_at_height,
        // Limits endpoints.
        api::handlers::limits::get_limits,
        api::handlers::limits::get_limits_for_account,
    ),
    // Components to be included in the OpenAPI specification.
    components(schemas(
        // Chainstate models.
        api::models::chainstate::Chainstate,
        // Deposit models.
        api::models::deposit::Deposit,
        api::models::deposit::responses::DepositWithStatus,
        api::models::deposit::DepositParameters,
        api::models::deposit::DepositInfo,
        api::models::deposit::requests::CreateDepositRequestBody,
        api::models::deposit::requests::DepositUpdate, // signers may update the state of deposits to Accepted.
        api::models::deposit::requests::UpdateDepositsRequestBody, // signers may update the state of deposits to Accepted.
        api::models::deposit::responses::GetDepositsForTransactionResponse,
        api::models::deposit::responses::GetDepositsResponse,
        api::models::deposit::responses::UpdateDepositsResponse, // signers may update the state of deposits to Accepted.
        // Withdrawal Models.
        api::models::withdrawal::Withdrawal,
        api::models::withdrawal::responses::WithdrawalWithStatus,
        api::models::withdrawal::WithdrawalInfo,
        api::models::withdrawal::WithdrawalParameters,
        api::models::withdrawal::requests::WithdrawalUpdate, // signers may update the state of withdrawals to Accepted.
        api::models::withdrawal::requests::UpdateWithdrawalsRequestBody, // signers may update the state of withdrawals to Accepted.
        api::models::withdrawal::responses::GetWithdrawalsResponse,
        api::models::withdrawal::responses::UpdateWithdrawalsResponse, // signers may update the state of withdrawals to Accepted.
        // Health check datatypes.
        api::models::health::responses::HealthData,
        // Common models.
        api::models::common::DepositStatus,
        api::models::common::WithdrawalStatus,
        api::models::common::Fulfillment,
        // Limits models
        api::models::limits::Limits,
        api::models::limits::AccountLimits,
        // Errors.
        common::error::ErrorResponse,
    ))
)]
pub struct ApiDoc;
