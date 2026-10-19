from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Literal, Optional, Union


@dataclass(frozen=True)
class ClientConfig:
    default_gas_limit: int = 21_000


@dataclass
class SubmitResponse:
    tx_hash: str
    accepted: bool


@dataclass(frozen=True)
class AirdropResponse:
    success: bool


@dataclass
class JobRequest:
    job_id: str
    model_hash: str
    input_hash: str
    max_fee: int
    expires_at: int
    metadata: Optional[Dict[str, object]] = None


@dataclass
class JobSubmission:
    url: str
    method: str
    headers: Dict[str, str]
    body: JobRequest


@dataclass
class RpcBlockHeader:
    slot: int
    timestamp: int
    proposer: Optional[Any] = None


@dataclass
class RpcBlock:
    header: RpcBlockHeader
    transactions: List[Any]

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "RpcBlock":
        header_data = data.get("header", {})
        header = RpcBlockHeader(
            slot=header_data.get("slot", 0),
            timestamp=header_data.get("timestamp", 0),
            proposer=header_data.get("proposer"),
        )
        return cls(header=header, transactions=data.get("transactions", []))


@dataclass
class RpcReceipt:
    tx_hash: Any
    block_hash: Any
    slot: int
    status: Any

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "RpcReceipt":
        return cls(
            tx_hash=data.get("tx_hash"),
            block_hash=data.get("block_hash"),
            slot=data.get("slot", 0),
            status=data.get("status"),
        )


# Account state is an open-ended map of fields returned by the node.
RpcAccountState = Dict[str, Any]


@dataclass
class NodeSyncStatus:
    syncing: bool
    from_slot: Optional[int] = None
    target_slot: Optional[int] = None

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "NodeSyncStatus":
        return cls(
            syncing=data.get("syncing", False),
            from_slot=data.get("fromSlot"),
            target_slot=data.get("targetSlot"),
        )


@dataclass
class NodeHealth:
    status: Literal["ok", "syncing", "error"]
    version: str
    latest_slot: int
    finalized_slot: int
    peer_count: int
    sync: NodeSyncStatus

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "NodeHealth":
        return cls(
            status=data.get("status", "ok"),
            version=data.get("version", ""),
            latest_slot=data.get("latestSlot", 0),
            finalized_slot=data.get("finalizedSlot", 0),
            peer_count=data.get("peerCount", 0),
            sync=NodeSyncStatus.from_dict(data.get("sync", {})),
        )


def ensure_hex(value: str, *, field: str) -> None:
    if not value.startswith("0x"):
        raise ValueError(f"{field} must be a hex string")
    if len(value) == 2:
        raise ValueError(f"{field} must not be empty")
    try:
        int(value[2:], 16)
    except ValueError as exc:
        raise ValueError(f"{field} must be valid hex") from exc


def ensure_positive_int(value: int, *, field: str) -> None:
    if value <= 0:
        raise ValueError(f"{field} must be positive")
