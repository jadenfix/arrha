from __future__ import annotations

import time
from dataclasses import dataclass
from typing import Dict, Optional

from .transaction import (
    FeeParameters,
    Keypair,
    Transaction,
    address_bytes,
    calculate_transfer_fee,
)
from .types import (
    ClientConfig,
    JobRequest,
    JobSubmission,
    ensure_hex,
    ensure_positive_int,
)


@dataclass
class TransferBuilder:
    config: ClientConfig
    _recipient: Optional[str] = None
    _amount: Optional[int] = None
    _memo: Optional[str] = None
    _fee: Optional[int] = None
    _gas_limit: int = 0

    def __post_init__(self) -> None:
        self._gas_limit = self.config.default_gas_limit

    def to(self, recipient: str) -> "TransferBuilder":
        ensure_hex(recipient, field="recipient")
        self._recipient = recipient
        return self

    def amount(self, amount: int) -> "TransferBuilder":
        ensure_positive_int(amount, field="amount")
        self._amount = amount
        return self

    def memo(self, memo: str) -> "TransferBuilder":
        self._memo = memo
        return self

    def fee(self, fee: int) -> "TransferBuilder":
        ensure_positive_int(fee, field="fee")
        self._fee = fee
        return self

    def gas_limit(self, gas_limit: int) -> "TransferBuilder":
        ensure_positive_int(gas_limit, field="gas_limit")
        self._gas_limit = gas_limit
        return self

    def build(
        self,
        *,
        keypair: Keypair,
        chain_id: int,
        nonce: int,
        fee_parameters: Optional[FeeParameters] = None,
    ) -> Transaction:
        if self._recipient is None:
            raise ValueError("recipient not set")
        if self._amount is None:
            raise ValueError("amount not set")

        recipient = address_bytes(self._recipient)
        fee = self._fee
        if fee is None:
            if fee_parameters is None:
                raise ValueError("live RPC fee parameters are required when no explicit fee is set")
            fee = calculate_transfer_fee(
                keypair=keypair,
                recipient=recipient,
                amount=self._amount,
                nonce=nonce,
                chain_id=chain_id,
                parameters=fee_parameters,
                gas_limit=self._gas_limit,
                memo=self._memo,
            )
        return Transaction.sign_transfer(
            keypair=keypair,
            recipient=recipient,
            amount=self._amount,
            nonce=nonce,
            chain_id=chain_id,
            fee=fee,
            gas_limit=self._gas_limit,
            memo=self._memo,
        )


class JobBuilder:
    def __init__(self, endpoint: str):
        self._endpoint = endpoint.rstrip("/")
        self._job_id: Optional[str] = None
        self._model_hash: Optional[str] = None
        self._input_hash: Optional[str] = None
        self._max_fee: int = 1_000_000
        self._expires_at: Optional[int] = None
        self._metadata: Optional[Dict[str, object]] = None

    def id(self, job_id: str) -> "JobBuilder":
        if not job_id.strip():
            raise ValueError("job_id must not be empty")
        self._job_id = job_id
        return self

    def model(self, model_hash: str) -> "JobBuilder":
        ensure_hex(model_hash, field="model_hash")
        self._model_hash = model_hash
        return self

    def input(self, input_hash: str) -> "JobBuilder":
        ensure_hex(input_hash, field="input_hash")
        self._input_hash = input_hash
        return self

    def max_fee(self, fee: int) -> "JobBuilder":
        ensure_positive_int(fee, field="max_fee")
        self._max_fee = fee
        return self

    def expires_at(self, expires_at: int) -> "JobBuilder":
        ensure_positive_int(expires_at, field="expires_at")
        if expires_at <= int(time.time()):
            raise ValueError("expiry must be in the future")
        self._expires_at = expires_at
        return self

    def with_metadata(self, metadata: Dict[str, object]) -> "JobBuilder":
        self._metadata = metadata
        return self

    def build(self) -> JobRequest:
        if self._job_id is None:
            raise ValueError("job_id not set")
        if self._model_hash is None:
            raise ValueError("model_hash not set")
        if self._input_hash is None:
            raise ValueError("input_hash not set")
        if self._expires_at is None:
            raise ValueError("expires_at not set")

        return JobRequest(
            job_id=self._job_id,
            model_hash=self._model_hash,
            input_hash=self._input_hash,
            max_fee=self._max_fee,
            expires_at=self._expires_at,
            metadata=self._metadata,
        )

    def to_submission(self) -> JobSubmission:
        job = self.build()
        return JobSubmission(
            url=f"{self._endpoint}/v1/jobs",
            method="POST",
            headers={"content-type": "application/json"},
            body=job,
        )
