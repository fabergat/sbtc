/*
 * emily-openapi-spec
 *
 * No description provided (generated by Openapi Generator https://github.com/openapitools/openapi-generator)
 *
 * The version of the OpenAPI document: 0.1.0
 *
 * Generated by: https://openapi-generator.tech
 */

use crate::models;
use serde::{Deserialize, Serialize};

/// WithdrawalParameters : Withdrawal parameters.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct WithdrawalParameters {
    /// Maximum fee the signers are allowed to take from the withdrawal to facilitate the inclusion of the transaction onto the Bitcoin blockchain.
    #[serde(rename = "maxFee")]
    pub max_fee: u64,
}

impl WithdrawalParameters {
    /// Withdrawal parameters.
    pub fn new(max_fee: u64) -> WithdrawalParameters {
        WithdrawalParameters { max_fee }
    }
}
