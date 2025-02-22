use std::sync::Arc;
use std::time::Duration;

use bitcoin_hashes::sha256;
use fedimint_client::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_client::transaction::ClientInput;
use fedimint_client::DynGlobalClientContext;
use fedimint_core::api::GlobalFederationApi;
use fedimint_core::config::FederationId;
use fedimint_core::core::{Decoder, OperationId};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::task::sleep;
use fedimint_core::{Amount, OutPoint, TransactionId};
use fedimint_ln_common::api::LnFederationApi;
use fedimint_ln_common::contracts::outgoing::OutgoingContractData;
use fedimint_ln_common::contracts::ContractId;
use fedimint_ln_common::{
    LightningClientContext, LightningGateway, LightningInput, LightningOutputOutcome,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::error;

use crate::{set_payment_result, LightningClientStateMachines, PayType};

#[cfg_attr(doc, aquamarine::aquamarine)]
/// State machine that requests the lightning gateway to pay an invoice on
/// behalf of a federation client.
///
/// ```mermaid
/// graph LR
/// classDef virtual fill:#fff,stroke-dasharray: 5 5
///
///  CreatedOutgoingLnContract -- await transaction failed --> Canceled
///  CreatedOutgoingLnContract -- await transaction acceptance --> Funded    
///  Funded -- await gateway payment success  --> Success
///  Funded -- await gateway payment failed --> Refundable
///  Refundable -- gateway issued refunded --> Refund
///  Refundable -- transaction timeout --> Refund
///  Refund -- await transaction acceptance --> Refunded
///  Refund -- await transaction rejected --> Failure
/// ```
#[derive(Debug, Clone, Eq, PartialEq, Decodable, Encodable)]
pub enum LightningPayStates {
    CreatedOutgoingLnContract(LightningPayCreatedOutgoingLnContract),
    Canceled,
    Funded(LightningPayFunded),
    Success(String),
    Refundable(LightningPayRefundable),
    Refund(LightningPayRefund),
    Refunded(Vec<OutPoint>),
    Failure(String),
}

#[derive(Debug, Clone, Eq, PartialEq, Decodable, Encodable)]
pub struct LightningPayCommon {
    pub operation_id: OperationId,
    pub federation_id: FederationId,
    pub contract: OutgoingContractData,
    pub gateway_fee: Amount,
}

#[derive(Debug, Clone, Eq, PartialEq, Decodable, Encodable)]
pub struct LightningPayStateMachine {
    pub common: LightningPayCommon,
    pub state: LightningPayStates,
}

impl State for LightningPayStateMachine {
    type ModuleContext = LightningClientContext;
    type GlobalContext = DynGlobalClientContext;

    fn transitions(
        &self,
        context: &Self::ModuleContext,
        global_context: &Self::GlobalContext,
    ) -> Vec<StateTransition<Self>> {
        match &self.state {
            LightningPayStates::CreatedOutgoingLnContract(created_outgoing_ln_contract) => {
                created_outgoing_ln_contract.transitions(&self.common, context, global_context)
            }
            LightningPayStates::Canceled => {
                vec![]
            }
            LightningPayStates::Funded(funded) => funded.transitions(self.common.clone()),
            LightningPayStates::Success(_) => {
                vec![]
            }
            LightningPayStates::Refundable(refundable) => {
                refundable.transitions(self.common.clone(), global_context.clone())
            }
            LightningPayStates::Refund(refund) => refund.transitions(&self.common, global_context),
            LightningPayStates::Refunded(_) => {
                vec![]
            }
            LightningPayStates::Failure(_) => {
                vec![]
            }
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Decodable, Encodable)]
pub struct LightningPayCreatedOutgoingLnContract {
    pub funding_txid: TransactionId,
    pub contract_id: ContractId,
    pub gateway: LightningGateway,
}

impl LightningPayCreatedOutgoingLnContract {
    fn transitions(
        &self,
        common: &LightningPayCommon,
        context: &LightningClientContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<LightningPayStateMachine>> {
        let txid = self.funding_txid;
        let contract_id = self.contract_id;
        let funded_common = common.clone();
        let success_context = global_context.clone();
        let gateway = self.gateway.clone();
        vec![StateTransition::new(
            Self::await_outgoing_contract_funded(
                context.ln_decoder.clone(),
                success_context,
                txid,
                contract_id,
            ),
            move |_dbtx, result, old_state| {
                Box::pin(Self::transition_outgoing_contract_funded(
                    result,
                    old_state,
                    funded_common.clone(),
                    contract_id,
                    gateway.clone(),
                ))
            },
        )]
    }

    async fn await_outgoing_contract_funded(
        module_decoder: Decoder,
        global_context: DynGlobalClientContext,
        txid: TransactionId,
        contract_id: ContractId,
    ) -> Result<u32, GatewayPayError> {
        let out_point = OutPoint { txid, out_idx: 0 };
        global_context
            .api()
            .await_output_outcome::<LightningOutputOutcome>(
                out_point,
                Duration::from_millis(i32::MAX as u64),
                &module_decoder,
            )
            .await
            .map_err(|_| GatewayPayError::OutgoingContractError)?;

        let contract = global_context
            .module_api()
            .get_outgoing_contract(contract_id)
            .await
            .map_err(|_| GatewayPayError::OutgoingContractError)?;
        Ok(contract.contract.timelock)
    }

    async fn transition_outgoing_contract_funded(
        result: Result<u32, GatewayPayError>,
        old_state: LightningPayStateMachine,
        common: LightningPayCommon,
        contract_id: ContractId,
        gateway: LightningGateway,
    ) -> LightningPayStateMachine {
        assert!(matches!(
            old_state.state,
            LightningPayStates::CreatedOutgoingLnContract(_)
        ));

        match result {
            Ok(timelock) => {
                // Success case: funding transaction is accepted
                let payload = PayInvoicePayload::new(common.federation_id, contract_id);
                LightningPayStateMachine {
                    common: old_state.common,
                    state: LightningPayStates::Funded(LightningPayFunded {
                        payload,
                        gateway,
                        timelock,
                        payment_hash: *common
                            .contract
                            .contract_account
                            .contract
                            .invoice
                            .payment_hash(),
                    }),
                }
            }
            Err(_) => {
                // Failure case: funding transaction is rejected
                LightningPayStateMachine {
                    common: old_state.common,
                    state: LightningPayStates::Canceled,
                }
            }
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Decodable, Encodable)]
pub struct LightningPayFunded {
    payload: PayInvoicePayload,
    gateway: LightningGateway,
    timelock: u32,
    payment_hash: sha256::Hash,
}

#[derive(Error, Debug, Serialize, Deserialize, Encodable, Decodable, Clone, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayPayError {
    #[error("Lightning Gateway failed to pay invoice. ErrorCode: {error_code:?} ErrorMessage: {error_message}")]
    GatewayInternalError {
        error_code: Option<u16>,
        error_message: String,
    },
    #[error("OutgoingContract was not created in the federation")]
    OutgoingContractError,
}

impl LightningPayFunded {
    fn transitions(
        &self,
        common: LightningPayCommon,
    ) -> Vec<StateTransition<LightningPayStateMachine>> {
        let gateway = self.gateway.clone();
        let payload = self.payload.clone();
        let contract_id = self.payload.contract_id;
        let timelock = self.timelock;
        let payment_hash = self.payment_hash;
        vec![StateTransition::new(
            Self::gateway_pay_invoice(gateway, payload),
            move |dbtx, result, old_state| {
                Box::pin(Self::transition_outgoing_contract_execution(
                    result,
                    old_state,
                    contract_id,
                    timelock,
                    dbtx,
                    payment_hash,
                    common.clone(),
                ))
            },
        )]
    }

    async fn gateway_pay_invoice(
        gateway: LightningGateway,
        payload: PayInvoicePayload,
    ) -> Result<String, GatewayPayError> {
        let response = reqwest::Client::new()
            .post(
                gateway
                    .api
                    .join("pay_invoice")
                    .expect("'pay_invoice' contains no invalid characters for a URL")
                    .as_str(),
            )
            .json(&payload)
            .send()
            .await
            .map_err(|e| GatewayPayError::GatewayInternalError {
                error_code: None,
                error_message: e.to_string(),
            })?;

        if !response.status().is_success() {
            return Err(GatewayPayError::GatewayInternalError {
                error_code: Some(response.status().as_u16()),
                error_message: response
                    .text()
                    .await
                    .expect("Could not retrieve text from response"),
            });
        }

        let preimage =
            response
                .text()
                .await
                .map_err(|_| GatewayPayError::GatewayInternalError {
                    error_code: None,
                    error_message: "Error retrieving preimage from response".to_string(),
                })?;
        let length = preimage.len();
        Ok(preimage[1..length - 1].to_string())
    }

    async fn transition_outgoing_contract_execution(
        result: Result<String, GatewayPayError>,
        old_state: LightningPayStateMachine,
        contract_id: ContractId,
        timelock: u32,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        payment_hash: sha256::Hash,
        common: LightningPayCommon,
    ) -> LightningPayStateMachine {
        match result {
            Ok(preimage) => {
                set_payment_result(
                    &mut dbtx.module_tx(),
                    payment_hash,
                    PayType::Lightning(old_state.common.operation_id),
                    contract_id,
                    common.gateway_fee,
                )
                .await;
                LightningPayStateMachine {
                    common: old_state.common,
                    state: LightningPayStates::Success(preimage),
                }
            }
            Err(e) => LightningPayStateMachine {
                common: old_state.common,
                state: LightningPayStates::Refundable(LightningPayRefundable {
                    contract_id,
                    block_timelock: timelock,
                    error: e,
                }),
            },
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Decodable, Encodable)]
pub struct LightningPayRefundable {
    contract_id: ContractId,
    pub block_timelock: u32,
    pub error: GatewayPayError,
}

impl LightningPayRefundable {
    fn transitions(
        &self,
        common: LightningPayCommon,
        global_context: DynGlobalClientContext,
    ) -> Vec<StateTransition<LightningPayStateMachine>> {
        let contract_id = self.contract_id;
        let timeout_global_context = global_context.clone();
        let timeout_common = common.clone();
        let timelock = self.block_timelock;
        vec![
            StateTransition::new(
                Self::await_contract_cancellable(contract_id, global_context.clone()),
                move |dbtx, (), old_state| {
                    Box::pin(Self::try_refund_outgoing_contract(
                        old_state,
                        common.clone(),
                        dbtx,
                        global_context.clone(),
                    ))
                },
            ),
            StateTransition::new(
                Self::await_contract_timeout(timeout_global_context.clone(), timelock),
                move |dbtx, (), old_state| {
                    Box::pin(Self::try_refund_outgoing_contract(
                        old_state,
                        timeout_common.clone(),
                        dbtx,
                        timeout_global_context.clone(),
                    ))
                },
            ),
        ]
    }

    /// Claims a refund for an expired or cancelled outgoing contract
    ///
    /// This can be necessary when the Lightning gateway cannot route the
    /// payment, is malicious or offline. The function returns the out point
    /// of the e-cash output generated as change.
    async fn try_refund_outgoing_contract(
        old_state: LightningPayStateMachine,
        common: LightningPayCommon,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        global_context: DynGlobalClientContext,
    ) -> LightningPayStateMachine {
        let contract_data = common.contract;
        let (refund_key, refund_input) = (
            contract_data.recovery_key,
            contract_data.contract_account.refund(),
        );

        let refund_client_input = ClientInput::<LightningInput, LightningClientStateMachines> {
            input: refund_input,
            keys: vec![refund_key],
            // The input of the refund tx is managed by this state machine, so no new state machines
            // need to be created
            state_machines: Arc::new(|_, _| vec![]),
        };

        let (txid, out_points) = global_context.claim_input(dbtx, refund_client_input).await;

        LightningPayStateMachine {
            common: old_state.common,
            state: LightningPayStates::Refund(LightningPayRefund { txid, out_points }),
        }
    }

    async fn await_contract_cancellable(
        contract_id: ContractId,
        global_context: DynGlobalClientContext,
    ) {
        loop {
            // If we fail to get the contract from the federation, we need to keep retrying
            // until we successfully do.
            match global_context
                .module_api()
                .wait_outgoing_contract_cancelled(contract_id)
                .await
            {
                Ok(_) => return,
                Err(error) => {
                    error!("Error waiting for outgoing contract to be cancelled: {error:?}");
                }
            }

            sleep(Duration::from_secs(1)).await;
        }
    }

    async fn await_contract_timeout(global_context: DynGlobalClientContext, timelock: u32) {
        loop {
            match global_context
                .module_api()
                .wait_block_height(timelock as u64)
                .await
            {
                Ok(_) => return,
                Err(error) => error!("Error waiting for block height: {timelock} {error:?}"),
            }

            sleep(Duration::from_secs(1)).await;
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Decodable, Encodable)]
pub struct LightningPayRefund {
    txid: TransactionId,
    out_points: Vec<OutPoint>,
}

impl LightningPayRefund {
    fn transitions(
        &self,
        common: &LightningPayCommon,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<LightningPayStateMachine>> {
        let refund_out_points = self.out_points.clone();
        vec![StateTransition::new(
            Self::await_refund_success(common.clone(), global_context.clone(), self.txid),
            move |_dbtx, result, old_state| {
                let refund_out_points = refund_out_points.clone();
                Box::pin(Self::transition_refund_success(
                    result,
                    old_state,
                    refund_out_points,
                ))
            },
        )]
    }

    async fn await_refund_success(
        common: LightningPayCommon,
        global_context: DynGlobalClientContext,
        refund_txid: TransactionId,
    ) -> Result<(), String> {
        global_context
            .await_tx_accepted(common.operation_id, refund_txid)
            .await
    }

    async fn transition_refund_success(
        result: Result<(), String>,
        old_state: LightningPayStateMachine,
        refund_out_points: Vec<OutPoint>,
    ) -> LightningPayStateMachine {
        match result {
            Ok(_) => {
                // Refund successful
                LightningPayStateMachine {
                    common: old_state.common,
                    state: LightningPayStates::Refunded(refund_out_points),
                }
            }
            Err(_) => {
                // Refund failure
                LightningPayStateMachine {
                    common: old_state.common,
                    state: LightningPayStates::Failure(
                        "Refund Transaction was rejected.".to_string(),
                    ),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Decodable, Encodable)]
#[serde(rename_all = "snake_case")]
pub struct PayInvoicePayload {
    pub federation_id: FederationId,
    pub contract_id: ContractId,
}

impl PayInvoicePayload {
    pub fn new(federation_id: FederationId, contract_id: ContractId) -> Self {
        Self {
            contract_id,
            federation_id,
        }
    }
}
