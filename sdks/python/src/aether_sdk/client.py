from __future__ import annotations

import json
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any, Dict, Optional, Union

from .builders import JobBuilder, TransferBuilder
from .transaction import Transaction
from .transaction import FeeParameters
from .types import (
    ClientConfig,
    AirdropResponse,
    JobRequest,
    JobSubmission,
    NodeHealth,
    RpcAccountState,
    RpcBlock,
    RpcReceipt,
    SubmitResponse,
)


def _normalize_endpoint(endpoint: str) -> str:
    if not endpoint:
        raise ValueError("endpoint must be provided")
    return endpoint.rstrip("/")


def _rpc_payload(method: str, params: list[object], request_id: int) -> bytes:
    return json.dumps(
        {
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": request_id,
        }
    ).encode("utf-8")


@dataclass
class AetherClient:
    endpoint: str
    config: ClientConfig = ClientConfig()
    _request_id: int = 1

    def __post_init__(self) -> None:
        self.endpoint = _normalize_endpoint(self.endpoint)

    def transfer(self) -> TransferBuilder:
        return TransferBuilder(self.config)

    def job(self) -> JobBuilder:
        return JobBuilder(self.endpoint)

    def submit(self, transaction: Transaction) -> SubmitResponse:
        tx_hash = self._rpc_call("aeth_sendRawTransaction", [transaction.to_raw_hex()])
        if not isinstance(tx_hash, str):
            raise ValueError("rpc response did not include a transaction hash")
        if tx_hash.lower() != transaction.hash().lower():
            raise ValueError(f"node returned transaction hash {tx_hash}, expected {transaction.hash()}")
        return SubmitResponse(tx_hash=tx_hash, accepted=True)

    def get_chain_id(self) -> int:
        value = self._rpc_call("aeth_chainId", [])
        if not isinstance(value, str) or not value.startswith("0x"):
            raise ValueError("aeth_chainId returned invalid hex")
        try:
            return int(value, 16)
        except ValueError as exc:
            raise ValueError("aeth_chainId returned invalid hex") from exc

    def get_fee_parameters(self) -> FeeParameters:
        value = self._rpc_call("aeth_getFeeParameters", [])
        if not isinstance(value, dict):
            raise ValueError("aeth_getFeeParameters returned an invalid response")
        return FeeParameters.from_dict(value)

    def get_pending_account(self, address: str) -> Optional[RpcAccountState]:
        return self._rpc_call("aeth_getPendingAccount", [address])

    def request_airdrop(self, address: str, amount: int) -> AirdropResponse:
        if amount <= 0:
            raise ValueError("airdrop amount must be positive")
        result = self._rpc_call("aeth_requestAirdrop", [address, str(amount)])
        if not isinstance(result, dict) or result.get("success") is not True:
            raise ValueError("aeth_requestAirdrop returned an invalid acknowledgement")
        return AirdropResponse(success=True)

    def get_transaction_status(self, tx_hash: str) -> Dict[str, Any]:
        result = self._rpc_call("aeth_getTransactionStatus", [tx_hash])
        if not isinstance(result, dict):
            raise ValueError("transaction status response was invalid")
        return result

    def wait_for_finality(
        self,
        tx_hash: str,
        *,
        timeout_seconds: float = 60.0,
        poll_seconds: float = 0.5,
    ) -> RpcReceipt:
        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            status = self.get_transaction_status(tx_hash)
            if status.get("status") == "finalized":
                if status.get("success") is not True:
                    raise ValueError(f"transaction {tx_hash} finalized with failure: {status.get('error')}")
                receipt = self.get_transaction_receipt(tx_hash)
                if receipt is None:
                    raise ValueError(f"finalized transaction {tx_hash} has no receipt")
                return receipt
            time.sleep(poll_seconds)
        raise TimeoutError(f"transaction {tx_hash} did not finalize within {timeout_seconds} seconds")

    def get_slot_number(self) -> int:
        slot = self._rpc_call("aeth_getSlotNumber", [])
        if not isinstance(slot, int):
            raise ValueError("rpc response did not include a slot number")
        return slot

    def get_finalized_slot(self) -> int:
        slot = self._rpc_call("aeth_getFinalizedSlot", [])
        if not isinstance(slot, int):
            raise ValueError("rpc response did not include a finalized slot")
        return slot

    def get_block_by_number(
        self,
        block_ref: Union[int, str] = "latest",
        full_tx: bool = True,
    ) -> Optional[RpcBlock]:
        result = self._rpc_call("aeth_getBlockByNumber", [str(block_ref), full_tx])
        if result is None:
            return None
        return RpcBlock.from_dict(result)

    def get_block_by_hash(
        self,
        block_hash: str,
        full_tx: bool = True,
    ) -> Optional[RpcBlock]:
        result = self._rpc_call("aeth_getBlockByHash", [block_hash, full_tx])
        if result is None:
            return None
        return RpcBlock.from_dict(result)

    def get_transaction_receipt(self, tx_hash: str) -> Optional[RpcReceipt]:
        result = self._rpc_call("aeth_getTransactionReceipt", [tx_hash])
        if result is None:
            return None
        return RpcReceipt.from_dict(result)

    def get_account(
        self,
        address: str,
        block_ref: Optional[str] = None,
    ) -> Optional[RpcAccountState]:
        params: list[object] = [address] if block_ref is None else [address, block_ref]
        return self._rpc_call("aeth_getAccount", params)

    def get_state_root(self, block_ref: Optional[str] = None) -> str:
        params: list[object] = [] if block_ref is None else [block_ref]
        result = self._rpc_call("aeth_getStateRoot", params)
        if not isinstance(result, str):
            raise ValueError("rpc response did not include a state root string")
        return result

    def get_health(self) -> NodeHealth:
        """Fetch node health from the HTTP /health endpoint (not a JSON-RPC call)."""
        request = urllib.request.Request(
            f"{self.endpoint}/health",
            headers={"accept": "application/json"},
            method="GET",
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                body = response.read().decode("utf-8")
        except urllib.error.URLError as exc:
            raise ConnectionError(
                f"failed to reach health endpoint {self.endpoint}/health"
            ) from exc
        data: Dict[str, Any] = json.loads(body)
        return NodeHealth.from_dict(data)

    def prepare_job_submission(self, job: JobRequest) -> JobSubmission:
        return JobSubmission(
            url=f"{self.endpoint}/v1/jobs",
            method="POST",
            headers={"content-type": "application/json"},
            body=job,
        )

    def _rpc_call(self, method: str, params: list[object]) -> Any:
        request_id = self._request_id
        self._request_id += 1
        request = urllib.request.Request(
            self.endpoint,
            data=_rpc_payload(method, params, request_id),
            headers={"content-type": "application/json"},
            method="POST",
        )

        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                body = response.read().decode("utf-8")
        except urllib.error.URLError as exc:
            raise ConnectionError(f"failed to reach rpc endpoint {self.endpoint}") from exc

        payload: Dict[str, Any] = json.loads(body)
        error = payload.get("error")
        if error is not None:
            code = error.get("code", "unknown")
            message = error.get("message", "unknown rpc error")
            raise ValueError(f"rpc error {code}: {message}")
        if "result" not in payload:
            raise ValueError("rpc response missing result")
        return payload["result"]
