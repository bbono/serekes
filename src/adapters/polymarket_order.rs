use crate::domain::OrderIntent;
use crate::ports::exchange::{ExchangePort, OrderResult};
use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use log::{debug, error};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer as _};
use polymarket_client_sdk::clob::types::response::PostOrderResponse;
use polymarket_client_sdk::clob::types::{Amount, OrderStatusType, SignatureType, Side};
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::types::{Decimal, U256};
use polymarket_client_sdk::POLYGON;
use std::str::FromStr;

/// Polymarket CLOB order adapter implementing ExchangePort.
pub struct PolymarketOrderAdapter {
    client: Option<ClobClient<Authenticated<Normal>>>,
    signer_instance: Option<PrivateKeySigner>,
    paper_mode: bool,
}

impl PolymarketOrderAdapter {
    /// Create a new adapter. If `private_key` is None, runs in paper mode.
    pub async fn new(private_key: Option<&str>) -> Self {
        match private_key {
            Some(pk) => {
                let signer: PrivateKeySigner = LocalSigner::from_str(pk)
                    .unwrap()
                    .with_chain_id(Some(POLYGON));
                let client_builder =
                    ClobClient::new("https://clob.polymarket.com", ClobConfig::builder().build())
                        .unwrap();

                match client_builder
                    .authentication_builder(&signer)
                    .signature_type(SignatureType::Proxy)
                    .authenticate()
                    .await
                {
                    Ok(client) => {
                        debug!("SDK authenticated");
                        Self {
                            client: Some(client),
                            signer_instance: Some(signer),
                            paper_mode: false,
                        }
                    }
                    Err(e) => {
                        error!("SDK auth failed: {e}");
                        Self {
                            client: None,
                            signer_instance: None,
                            paper_mode: true,
                        }
                    }
                }
            }
            None => Self {
                client: None,
                signer_instance: None,
                paper_mode: true,
            },
        }
    }

    async fn sign_and_submit(
        &self,
        token_id: &str,
        intent: &OrderIntent,
    ) -> Result<PostOrderResponse, String> {
        let (Some(ref client), Some(ref signer)) = (&self.client, &self.signer_instance) else {
            return Err("client not initialized".into());
        };

        let token_u256 =
            U256::from_str(token_id).map_err(|e| format!("Invalid token_id: {:?}", e))?;

        let order = match intent {
            OrderIntent::Limit {
                side,
                price,
                size,
                order_type,
            } => client
                .limit_order()
                .order_type(order_type.clone())
                .token_id(token_u256)
                .side(*side)
                .price(*price)
                .size(*size)
                .build()
                .await
                .map_err(|e| format!("Order build failed: {:?}", e))?,
            OrderIntent::Market {
                side,
                amount,
                order_type,
            } => {
                let amt = if *side == Side::Buy {
                    Amount::usdc(*amount)
                } else {
                    Amount::shares(*amount)
                }
                .map_err(|e| format!("Invalid amount: {:?}", e))?;

                client
                    .market_order()
                    .order_type(order_type.clone())
                    .token_id(token_u256)
                    .side(*side)
                    .amount(amt)
                    .build()
                    .await
                    .map_err(|e| format!("Order build failed: {:?}", e))?
            }
        };

        let signed_order = client
            .sign(signer, order)
            .await
            .map_err(|e| format!("Order sign failed: {:?}", e))?;
        client
            .post_order(signed_order)
            .await
            .map_err(|e| format!("Order post failed: {:?}", e))
    }
}

#[async_trait::async_trait]
impl ExchangePort for PolymarketOrderAdapter {
    async fn submit_order(
        &self,
        token_id: &str,
        intent: &OrderIntent,
    ) -> Result<OrderResult, String> {
        if self.paper_mode {
            return Ok(OrderResult {
                order_id: "PAPERTRADE-1".to_string(),
                status: OrderStatusType::Matched,
                making_amount: Decimal::default(),
                taking_amount: Decimal::default(),
            });
        }

        let resp = self.sign_and_submit(token_id, intent).await?;
        Ok(OrderResult {
            order_id: resp.order_id,
            status: resp.status,
            making_amount: resp.making_amount,
            taking_amount: resp.taking_amount,
        })
    }

    fn is_paper_mode(&self) -> bool {
        self.paper_mode
    }
}
