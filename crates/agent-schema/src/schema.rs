use serde_json::{json, Value};

/// JSON Schema for the language-neutral agent settlement wire contract.
///
/// The Rust structs remain the source implementation, but this schema is the
/// cross-repo artifact consumed by beater.js, tempo, beatbox, and other SDKs.
#[must_use]
pub fn agent_contract_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://aether.dev/schema/agent-settlement/v1",
        "title": "Aether Agent Settlement Schema",
        "type": "object",
        "additionalProperties": false,
        "required": [
            "signatureEnvelope",
            "agentAuthorization",
            "stepReceipt",
            "runStatus",
            "settlementPolicy",
            "paymentEnvelope"
        ],
        "properties": {
            "signatureEnvelope": { "$ref": "#/$defs/SignatureEnvelope" },
            "agentAuthorization": { "$ref": "#/$defs/AgentAuthorization" },
            "stepReceiptSigningPayload": { "$ref": "#/$defs/StepReceiptSigningPayload" },
            "stepReceipt": { "$ref": "#/$defs/StepReceipt" },
            "journalProof": { "$ref": "#/$defs/JournalProof" },
            "runStatus": { "$ref": "#/$defs/RunStatus" },
            "settlementPolicy": { "$ref": "#/$defs/SettlementPolicy" },
            "paymentEnvelope": { "$ref": "#/$defs/PaymentEnvelope" }
        },
        "$defs": {
            "H160": {
                "type": "string",
                "pattern": "^0x[0-9a-fA-F]{40}$",
                "description": "20-byte Aether address encoded as 0x-prefixed hex"
            },
            "H256": {
                "type": "string",
                "pattern": "^0x[0-9a-fA-F]{64}$",
                "description": "32-byte hash encoded as 0x-prefixed hex"
            },
            "Bytes": {
                "type": "string",
                "pattern": "^0x[0-9a-fA-F]*$",
                "description": "Arbitrary bytes encoded as 0x-prefixed hex"
            },
            "SigningAlgorithm": {
                "type": "string",
                "enum": [
                    "ed25519",
                    "bls12381",
                    "frost_ristretto255",
                    "ed25519_ml_dsa87",
                    "ml_dsa87",
                    "slh_dsa_sha2256f"
                ]
            },
            "SideEffect": {
                "type": "string",
                "enum": ["read", "draft", "write", "send", "purchase", "delete"]
            },
            "StepKind": {
                "type": "string",
                "enum": ["llm_call", "tool_call", "browse_act", "sandbox_exec", "mcp_call"]
            },
            "PaymentToken": {
                "type": "string",
                "enum": ["aic", "swr"]
            },
            "RunStatus": {
                "type": "string",
                "enum": ["running", "completed", "failed", "needs_review", "disputed"]
            },
            "MerkleSide": {
                "type": "string",
                "enum": ["left", "right"]
            },
            "AgentRunId": {
                "$ref": "#/$defs/H256"
            },
            "JournalRoot": {
                "allOf": [{ "$ref": "#/$defs/H256" }],
                "description": "32-byte journal commitment H(aether/agent_journal_root/v1 || leaf_count || raw padded Merkle root), encoded as 0x-prefixed hex"
            },
            "SignatureEnvelope": {
                "type": "object",
                "additionalProperties": false,
                "required": ["alg", "domain", "chain_id", "key_id", "payload_hash", "signature"],
                "properties": {
                    "alg": { "$ref": "#/$defs/SigningAlgorithm" },
                    "domain": {
                        "type": "string",
                        "pattern": "^aether/.+",
                        "description": "Domain-separated signing context. Agent settlement envelopes use exact object domains: aether/agent_authorization/v1, aether/agent_payment_authorization/v1, or aether/agent_step_receipt/v1."
                    },
                    "chain_id": { "type": "integer", "minimum": 1 },
                    "key_id": { "type": "string", "minLength": 1 },
                    "payload_hash": { "$ref": "#/$defs/H256" },
                    "signature": { "$ref": "#/$defs/Bytes" },
                    "pq_signature": {
                        "anyOf": [{ "$ref": "#/$defs/Bytes" }, { "type": "null" }]
                    }
                }
            },
            "AgentAuthorization": {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "agent_account",
                    "session_public_key",
                    "delegated_by",
                    "valid_from_slot",
                    "valid_until_slot",
                    "max_aic",
                    "max_per_call_aic",
                    "allowed_side_effects",
                    "policy_hash",
                    "signature"
                ],
                "properties": {
                    "agent_account": { "$ref": "#/$defs/H160" },
                    "session_public_key": { "$ref": "#/$defs/Bytes" },
                    "delegated_by": { "$ref": "#/$defs/H160" },
                    "valid_from_slot": { "type": "integer", "minimum": 0 },
                    "valid_until_slot": { "type": "integer", "minimum": 1 },
                    "max_aic": { "type": "integer", "minimum": 0 },
                    "max_per_call_aic": { "type": "integer", "minimum": 0 },
                    "allowed_side_effects": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "$ref": "#/$defs/SideEffect" }
                    },
                    "allowed_tools": {
                        "type": "array",
                        "default": [],
                        "items": { "type": "string" }
                    },
                    "allowed_recipients": {
                        "type": "array",
                        "default": [],
                        "items": { "$ref": "#/$defs/H160" }
                    },
                    "policy_hash": { "$ref": "#/$defs/H256" },
                    "guardian_threshold": {
                        "anyOf": [{ "type": "integer", "minimum": 1 }, { "type": "null" }]
                    },
                    "guardian_public_key": {
                        "anyOf": [{ "$ref": "#/$defs/Bytes" }, { "type": "null" }]
                    },
                    "signature": {
                        "allOf": [{ "$ref": "#/$defs/SignatureEnvelope" }],
                        "description": "Guardian approval envelope. domain must equal aether/agent_authorization/v1 and payload_hash must equal the operation policy hash verified by FROST."
                    }
                }
            },
            "SettlementPolicy": {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "min_escrow_aic",
                    "challenge_slots"
                ],
                "properties": {
                    "min_escrow_aic": { "type": "integer", "minimum": 0 },
                    "challenge_slots": { "type": "integer", "minimum": 0 },
                    "requires_human_confirm": {
                        "type": "boolean",
                        "default": false
                    }
                }
            },
            "StepReceipt": {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "run_id",
                    "seq",
                    "kind",
                    "side_effect",
                    "request_hash",
                    "result_hash",
                    "tool_use_id",
                    "signer",
                    "signature"
                ],
                "properties": {
                    "run_id": { "$ref": "#/$defs/AgentRunId" },
                    "seq": { "type": "integer", "minimum": 1 },
                    "prev_receipt_hash": {
                        "anyOf": [{ "$ref": "#/$defs/H256" }, { "type": "null" }]
                    },
                    "kind": { "$ref": "#/$defs/StepKind" },
                    "side_effect": { "$ref": "#/$defs/SideEffect" },
                    "request_hash": { "$ref": "#/$defs/H256" },
                    "result_hash": { "$ref": "#/$defs/H256" },
                    "evidence_uri_hash": {
                        "anyOf": [{ "$ref": "#/$defs/H256" }, { "type": "null" }]
                    },
                    "tool_use_id": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Stable tool-use identifier from the agent runtime journal"
                    },
                    "signer": { "$ref": "#/$defs/H160" },
                    "signature": { "$ref": "#/$defs/SignatureEnvelope" }
                }
            },
            "StepReceiptSigningPayload": {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "run_id",
                    "seq",
                    "kind",
                    "side_effect",
                    "request_hash",
                    "result_hash",
                    "tool_use_id",
                    "signer"
                ],
                "properties": {
                    "run_id": { "$ref": "#/$defs/AgentRunId" },
                    "seq": { "type": "integer", "minimum": 1 },
                    "prev_receipt_hash": {
                        "anyOf": [{ "$ref": "#/$defs/H256" }, { "type": "null" }]
                    },
                    "kind": { "$ref": "#/$defs/StepKind" },
                    "side_effect": { "$ref": "#/$defs/SideEffect" },
                    "request_hash": { "$ref": "#/$defs/H256" },
                    "result_hash": { "$ref": "#/$defs/H256" },
                    "evidence_uri_hash": {
                        "anyOf": [{ "$ref": "#/$defs/H256" }, { "type": "null" }]
                    },
                    "tool_use_id": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Stable tool-use identifier from the agent runtime journal"
                    },
                    "signer": { "$ref": "#/$defs/H160" }
                }
            },
            "JournalProofNode": {
                "type": "object",
                "additionalProperties": false,
                "required": ["side", "hash"],
                "properties": {
                    "side": { "$ref": "#/$defs/MerkleSide" },
                    "hash": { "$ref": "#/$defs/H256" }
                }
            },
            "JournalProof": {
                "type": "object",
                "additionalProperties": false,
                "required": ["leaf_hash", "leaf_index", "leaf_count", "siblings"],
                "properties": {
                    "leaf_hash": { "$ref": "#/$defs/H256" },
                    "leaf_index": { "type": "integer", "minimum": 0 },
                    "leaf_count": { "type": "integer", "minimum": 1 },
                    "siblings": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/JournalProofNode" }
                    }
                }
            },
            "PaymentEnvelope": {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "token",
                    "amount",
                    "recipient",
                    "quote_hash",
                    "request_hash",
                    "nonce",
                    "expires_at_slot",
                    "chain_id",
                    "side_effect",
                    "signature"
                ],
                "properties": {
                    "token": { "$ref": "#/$defs/PaymentToken" },
                    "amount": {
                        "type": "string",
                        "pattern": "^[1-9][0-9]*$",
                        "description": "AIC/SWR base-unit amount encoded as a canonical decimal string for JavaScript-safe u128 compatibility"
                    },
                    "recipient": { "$ref": "#/$defs/H160" },
                    "quote_hash": { "$ref": "#/$defs/H256" },
                    "request_hash": { "$ref": "#/$defs/H256" },
                    "result_hash": {
                        "anyOf": [{ "$ref": "#/$defs/H256" }, { "type": "null" }]
                    },
                    "nonce": { "$ref": "#/$defs/H256" },
                    "expires_at_slot": { "type": "integer", "minimum": 1 },
                    "chain_id": { "type": "integer", "minimum": 1 },
                    "side_effect": { "$ref": "#/$defs/SideEffect" },
                    "max_replays": { "type": "integer", "minimum": 1, "default": 1 },
                    "signature": {
                        "allOf": [{ "$ref": "#/$defs/SignatureEnvelope" }],
                        "description": "Payment authorization envelope. domain must equal aether/agent_payment_authorization/v1, chain_id must equal the payment chain_id, and payload_hash must equal PaymentEnvelope.signing_payload_hash."
                    }
                }
            }
        }
    })
}
