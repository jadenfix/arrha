use std::collections::BTreeMap;

use aether_agent_schema::{PaymentEnvelope, PaymentToken, SideEffect};
use aether_types::{Address, H256};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AetherSdkError;

pub const AETHER_PAYMENT_SCHEME: &str = "aether-agent-payment-v1";
pub const AETHER_PAYMENT_HEADER: &str = "X-PAYMENT";
pub const AETHER_PAYMENT_HASH_HEADER: &str = "X-AETHER-PAYMENT-HASH";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequiredOptions {
    pub network: String,
    pub resource: String,
    pub recipient: Address,
    pub token: PaymentToken,
    pub amount: u128,
    pub quote_hash: H256,
    pub request_hash: H256,
    pub side_effect: SideEffect,
    pub chain_id: u64,
    pub expires_at_slot: u64,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRequiredResponse {
    pub accepts: Vec<PaymentAcceptOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentAcceptOption {
    pub scheme: String,
    pub network: String,
    pub resource: String,
    #[serde(with = "wire::address")]
    pub pay_to: Address,
    pub token: PaymentToken,
    pub max_amount_required: String,
    pub extra: PaymentAcceptExtra,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentAcceptExtra {
    #[serde(with = "wire::h256")]
    pub quote_hash: H256,
    #[serde(with = "wire::h256")]
    pub request_hash: H256,
    pub side_effect: SideEffect,
    pub chain_id: u64,
    pub expires_at_slot: u64,
}

pub fn build_payment_required_response(
    options: PaymentRequiredOptions,
) -> Result<PaymentRequiredResponse, AetherSdkError> {
    if options.amount == 0 {
        return Err(AetherSdkError::build("amount must be a positive integer"));
    }
    if options.network.trim().is_empty() {
        return Err(AetherSdkError::build("network must not be empty"));
    }
    if options.resource.trim().is_empty() {
        return Err(AetherSdkError::build("resource must not be empty"));
    }
    if options.chain_id == 0 {
        return Err(AetherSdkError::build("chain_id must be positive"));
    }
    if options.expires_at_slot == 0 {
        return Err(AetherSdkError::build("expires_at_slot must be positive"));
    }

    Ok(PaymentRequiredResponse {
        accepts: vec![PaymentAcceptOption {
            scheme: AETHER_PAYMENT_SCHEME.to_string(),
            network: options.network,
            resource: options.resource,
            pay_to: options.recipient,
            token: options.token,
            max_amount_required: options.amount.to_string(),
            extra: PaymentAcceptExtra {
                quote_hash: options.quote_hash,
                request_hash: options.request_hash,
                side_effect: options.side_effect,
                chain_id: options.chain_id,
                expires_at_slot: options.expires_at_slot,
            },
        }],
        error: Some("payment_required".to_string()),
        description: options.description,
    })
}

pub fn validate_payment_envelope(
    envelope: &PaymentEnvelope,
    current_slot: u64,
) -> Result<(), AetherSdkError> {
    envelope
        .validate(current_slot)
        .map_err(|err| AetherSdkError::build(err.to_string()))?;
    envelope
        .validate_signature_binding()
        .map_err(|err| AetherSdkError::build(err.to_string()))?;
    Ok(())
}

pub fn payment_envelope_hash(envelope: &PaymentEnvelope) -> Result<String, AetherSdkError> {
    validate_payment_envelope(envelope, 0)?;
    envelope
        .payment_hash()
        .map(|hash| format!("{hash:?}"))
        .map_err(|err| AetherSdkError::build(err.to_string()))
}

pub fn encode_payment_header(envelope: &PaymentEnvelope) -> Result<String, AetherSdkError> {
    validate_payment_envelope(envelope, 0)?;
    let canonical = canonical_json(envelope)?;
    Ok(base64url_encode(canonical.as_bytes()))
}

pub fn decode_payment_header(header: &str) -> Result<PaymentEnvelope, AetherSdkError> {
    decode_payment_header_at(header, 0)
}

pub fn decode_payment_header_at(
    header: &str,
    current_slot: u64,
) -> Result<PaymentEnvelope, AetherSdkError> {
    let bytes = base64url_decode(header)?;
    let decoded = std::str::from_utf8(&bytes).map_err(|err| {
        AetherSdkError::invalid_response(format!("payment header is not utf-8: {err}"))
    })?;
    let envelope: PaymentEnvelope = serde_json::from_str(decoded).map_err(|err| {
        AetherSdkError::invalid_response(format!("payment header is not a payment envelope: {err}"))
    })?;
    validate_payment_envelope(&envelope, current_slot)?;
    Ok(envelope)
}

pub fn decode_payment_header_with_hash(
    header: &str,
    expected_hash: &str,
) -> Result<PaymentEnvelope, AetherSdkError> {
    let envelope = decode_payment_header(header)?;
    let actual_hash = payment_envelope_hash(&envelope)?;
    let expected = parse_h256_hex(expected_hash)?;
    if actual_hash != format!("{expected:?}") {
        return Err(AetherSdkError::invalid_response(format!(
            "payment hash mismatch: expected {expected:?}, got {actual_hash}"
        )));
    }
    Ok(envelope)
}

pub fn payment_headers(
    envelope: &PaymentEnvelope,
) -> Result<BTreeMap<String, String>, AetherSdkError> {
    let mut headers = BTreeMap::new();
    headers.insert(
        AETHER_PAYMENT_HEADER.to_string(),
        encode_payment_header(envelope)?,
    );
    headers.insert(
        AETHER_PAYMENT_HASH_HEADER.to_string(),
        payment_envelope_hash(envelope)?,
    );
    Ok(headers)
}

fn canonical_json<T: Serialize>(value: &T) -> Result<String, AetherSdkError> {
    let value = serde_json::to_value(value).map_err(AetherSdkError::serialization)?;
    serde_json::to_string(&canonicalize(value)).map_err(AetherSdkError::serialization)
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        Value::Object(object) => {
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut sorted = serde_json::Map::new();
            for (key, value) in entries {
                sorted.insert(key, canonicalize(value));
            }
            Value::Object(sorted)
        }
        scalar => scalar,
    }
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        }
    }
    out
}

fn base64url_decode(encoded: &str) -> Result<Vec<u8>, AetherSdkError> {
    if encoded.len() % 4 == 1 {
        return Err(AetherSdkError::invalid_response(
            "payment header has invalid base64url length",
        ));
    }

    let mut out = Vec::with_capacity(encoded.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in encoded.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => {
                return Err(AetherSdkError::invalid_response(
                    "payment header must use unpadded base64url",
                ))
            }
            _ => {
                return Err(AetherSdkError::invalid_response(format!(
                    "payment header contains invalid base64url byte: {byte}"
                )))
            }
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    if bits > 0 && buffer != 0 {
        return Err(AetherSdkError::invalid_response(
            "payment header has non-zero trailing base64url bits",
        ));
    }
    Ok(out)
}

fn parse_h256_hex(encoded: &str) -> Result<H256, AetherSdkError> {
    let body = encoded
        .strip_prefix("0x")
        .ok_or_else(|| AetherSdkError::invalid_response("payment hash must start with 0x"))?;
    let bytes = hex::decode(body)
        .map_err(|err| AetherSdkError::invalid_response(format!("invalid payment hash: {err}")))?;
    H256::from_slice(&bytes)
        .map_err(|err| AetherSdkError::invalid_response(format!("invalid payment hash: {err}")))
}

mod wire {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) mod h256 {
        use super::*;

        pub fn serialize<S>(value: &H256, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            serializer.serialize_str(&format!("{value:?}"))
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<H256, D::Error>
        where
            D: Deserializer<'de>,
        {
            let encoded = String::deserialize(deserializer)?;
            let body = encoded
                .strip_prefix("0x")
                .ok_or_else(|| serde::de::Error::custom("hex value must start with 0x"))?;
            let bytes = hex::decode(body).map_err(serde::de::Error::custom)?;
            H256::from_slice(&bytes).map_err(serde::de::Error::custom)
        }
    }

    pub(super) mod address {
        use super::*;

        pub fn serialize<S>(value: &Address, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            serializer.serialize_str(&format!("{value:?}"))
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Address, D::Error>
        where
            D: Deserializer<'de>,
        {
            let encoded = String::deserialize(deserializer)?;
            let body = encoded
                .strip_prefix("0x")
                .ok_or_else(|| serde::de::Error::custom("hex value must start with 0x"))?;
            let bytes = hex::decode(body).map_err(serde::de::Error::custom)?;
            Address::from_slice(&bytes).map_err(serde::de::Error::custom)
        }
    }
}
