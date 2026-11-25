use std::{str::FromStr, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use spr_keypair::ShieldedAddress;
use spr_transaction::{Address, Mint};

use crate::Quote;

pub const PAYMENT_REQUIRED: &str = "payment-required";
pub const PAYMENT_SIGNATURE: &str = "payment-signature";
pub const PAYMENT_RESPONSE: &str = "payment-response";
pub const PAYMENT_SESSION: &str = "payment-session";

pub const X402_VERSION: u8 = 2;
pub const SCHEME: &str = "confidential";
pub const SOLANA_MAINNET: &str = "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp";
pub const SOLANA_DEVNET: &str = "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1";

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("header is not base64 JSON")]
    Encoding,
    #[error("invalid {0}")]
    Field(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequired {
    pub x402_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub resource: Resource,
    pub accepts: Vec<Requirements>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Requirements {
    pub scheme: String,
    pub network: String,
    pub amount: String,
    pub asset: String,
    pub pay_to: String,
    pub max_timeout_seconds: u64,
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extra {
    pub quote: String,
    #[serde(default)]
    pub sessions: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentPayload {
    pub x402_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<Resource>,
    pub accepted: Requirements,
    pub payload: Proof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settlement {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
    pub transaction: String,
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Extensions>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extensions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub balance: String,
}

impl Requirements {
    pub fn offer(quote: &Quote, network: &str, timeout: Duration) -> Self {
        Self {
            scheme: SCHEME.to_string(),
            network: network.to_string(),
            amount: quote.amount.to_string(),
            asset: quote.asset.asset.to_string(),
            pay_to: quote.pay_to.to_string(),
            max_timeout_seconds: timeout.as_secs(),
            extra: Extra {
                quote: hex::encode(quote.id),
                sessions: true,
            },
        }
    }

    pub fn asset(&self) -> Result<Address, WireError> {
        Address::from_str(&self.asset).map_err(|_| WireError::Field("asset"))
    }

    pub fn price(&self) -> Result<u64, WireError> {
        self.amount.parse().map_err(|_| WireError::Field("amount"))
    }

    pub fn quote(&self, asset: Mint) -> Result<Quote, WireError> {
        if asset.asset != self.asset()? {
            return Err(WireError::Field("asset"));
        }
        let mut id = [0; 16];
        hex::decode_to_slice(&self.extra.quote, &mut id).map_err(|_| WireError::Field("quote"))?;
        Ok(Quote {
            id,
            asset,
            amount: self.price()?,
            pay_to: ShieldedAddress::from_str(&self.pay_to)
                .map_err(|_| WireError::Field("payTo"))?,
        })
    }
}

pub fn encode<T: Serialize>(value: &T) -> String {
    STANDARD.encode(serde_json::to_vec(value).expect("wire types serialize"))
}

pub fn decode<T: DeserializeOwned>(header: &str) -> Result<T, WireError> {
    let bytes = STANDARD
        .decode(header.trim())
        .map_err(|_| WireError::Encoding)?;
    serde_json::from_slice(&bytes).map_err(|_| WireError::Encoding)
}
