from __future__ import annotations

import hashlib
import re
import secrets
import struct
from dataclasses import dataclass
from typing import Optional

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

U64_MAX = (1 << 64) - 1
U128_MAX = (1 << 128) - 1
TRANSFER_PROGRAM_ID = b"\x01" * 32
TRANSFER_GAS_LIMIT = 21_000


@dataclass(frozen=True)
class Keypair:
    secret_key: bytes
    public_key: bytes
    address: bytes

    @classmethod
    def from_seed(cls, seed: bytes) -> "Keypair":
        if len(seed) != 32:
            raise ValueError("Ed25519 seed must be 32 bytes")
        private_key = Ed25519PrivateKey.from_private_bytes(seed)
        public_key = private_key.public_key().public_bytes(
            serialization.Encoding.Raw,
            serialization.PublicFormat.Raw,
        )
        return cls(secret_key=bytes(seed), public_key=public_key, address=hashlib.sha256(public_key).digest()[:20])

    @classmethod
    def generate(cls) -> "Keypair":
        return cls.from_seed(secrets.token_bytes(32))

    @classmethod
    def devnet(cls, index: int) -> "Keypair":
        if not 0 <= index <= 0xFFFF_FFFF:
            raise ValueError("devnet account index must fit in u32")
        seed = hashlib.sha256(b"aether-devnet-account" + struct.pack("<I", index)).digest()
        return cls.from_seed(seed)

    @property
    def address_hex(self) -> str:
        return "0x" + self.address.hex()


@dataclass(frozen=True)
class FeeParameters:
    a: int
    b: int
    c: int
    min_base_fee: int
    chain_id: int

    @classmethod
    def from_dict(cls, value: dict) -> "FeeParameters":
        if not isinstance(value, dict):
            raise ValueError("fee parameter response is incomplete or invalid")
        numbers = {}
        for key in ("a", "b", "c", "minBaseFee"):
            raw = value.get(key)
            if not isinstance(raw, str) or re.fullmatch(r"(?:0|[1-9][0-9]*)", raw) is None:
                raise ValueError(f"fee parameter {key} must be a decimal integer string")
            numbers[key] = int(raw)
        chain_id = value.get("chainId")
        if not isinstance(chain_id, int) or isinstance(chain_id, bool) or chain_id <= 0:
            raise ValueError("fee parameter chainId must be a positive integer")
        numbers["chainId"] = chain_id
        return cls(
            a=numbers["a"],
            b=numbers["b"],
            c=numbers["c"],
            min_base_fee=numbers["minBaseFee"],
            chain_id=numbers["chainId"],
        )


@dataclass(frozen=True)
class Transaction:
    nonce: int
    chain_id: int
    sender: bytes
    public_key: bytes
    recipient: bytes
    amount: int
    fee: int
    gas_limit: int
    memo: Optional[str]
    signature: bytes

    def hash(self) -> str:
        return "0x" + hashlib.sha256(self._encode(signature=b"", fee=self.fee)).hexdigest()

    def to_raw_bytes(self) -> bytes:
        return self._encode(signature=self.signature, fee=self.fee)

    def to_raw_hex(self) -> str:
        return "0x" + self.to_raw_bytes().hex()

    def _encode(self, *, signature: bytes, fee: int) -> bytes:
        payload = encode_transfer_payload(self.recipient, self.amount, self.memo)
        reads = [self.sender]
        writes = sorted({self.sender, self.recipient})
        if self.sender == self.recipient:
            writes = [self.sender]
        return b"".join((
            u64(self.nonce),
            u64(self.chain_id),
            self.sender,
            byte_vec(self.public_key),
            u64(0),  # Vec<UtxoId>
            u64(0),  # Vec<UtxoOutput>
            address_set(reads),
            address_set(writes),
            b"\x01" + TRANSFER_PROGRAM_ID,  # Some(TRANSFER_PROGRAM_ID)
            byte_vec(payload),
            u64(self.gas_limit),
            u128(fee),
            byte_vec(signature),
        ))

    @classmethod
    def sign_transfer(
        cls,
        *,
        keypair: Keypair,
        recipient: bytes,
        amount: int,
        nonce: int,
        chain_id: int,
        fee: int,
        gas_limit: int = TRANSFER_GAS_LIMIT,
        memo: Optional[str] = None,
    ) -> "Transaction":
        validate_transfer(keypair, recipient, amount, nonce, chain_id, fee, gas_limit)
        unsigned = cls(
            nonce=nonce,
            chain_id=chain_id,
            sender=keypair.address,
            public_key=keypair.public_key,
            recipient=recipient,
            amount=amount,
            fee=fee,
            gas_limit=gas_limit,
            memo=memo,
            signature=b"",
        )
        message = hashlib.sha256(unsigned._encode(signature=b"", fee=fee)).digest()
        signature = Ed25519PrivateKey.from_private_bytes(keypair.secret_key).sign(message)
        return cls(**{**unsigned.__dict__, "signature": signature})


def calculate_transfer_fee(
    *,
    keypair: Keypair,
    recipient: bytes,
    amount: int,
    nonce: int,
    chain_id: int,
    parameters: FeeParameters,
    gas_limit: int = TRANSFER_GAS_LIMIT,
    memo: Optional[str] = None,
) -> int:
    if parameters.chain_id != chain_id:
        raise ValueError("fee parameters belong to a different chain")
    unsigned = Transaction(
        nonce=nonce,
        chain_id=chain_id,
        sender=keypair.address,
        public_key=keypair.public_key,
        recipient=recipient,
        amount=amount,
        fee=0,
        gas_limit=gas_limit,
        memo=memo,
        signature=b"",
    )
    validate_transfer(keypair, recipient, amount, nonce, chain_id, 0, gas_limit)
    # Rust's fee checker measures the complete serialized transaction, including
    # the fixed 64-byte Ed25519 signature attached after fee selection.
    sized_bytes = unsigned._encode(signature=b"\x00" * 64, fee=0)
    minimum = parameters.a + parameters.b * len(sized_bytes) + parameters.c * gas_limit
    if minimum > U128_MAX:
        raise ValueError("minimum fee exceeds u128")
    recommended = minimum * 2
    if recommended > U128_MAX:
        raise ValueError("recommended fee exceeds u128")
    return recommended


def encode_transfer_payload(recipient: bytes, amount: int, memo: Optional[str]) -> bytes:
    if len(recipient) != 20:
        raise ValueError("recipient must be exactly 20 bytes")
    u128(amount)
    encoded_memo = b"\x00" if memo is None else b"\x01" + byte_vec(memo.encode("utf-8"))
    return recipient + u128(amount) + encoded_memo


def validate_transfer(
    keypair: Keypair,
    recipient: bytes,
    amount: int,
    nonce: int,
    chain_id: int,
    fee: int,
    gas_limit: int,
) -> None:
    if len(keypair.public_key) != 32 or len(keypair.secret_key) != 32 or len(keypair.address) != 20:
        raise ValueError("invalid Ed25519 keypair")
    if hashlib.sha256(keypair.public_key).digest()[:20] != keypair.address:
        raise ValueError("sender address does not match public key")
    if len(recipient) != 20:
        raise ValueError("recipient must be exactly 20 bytes")
    if amount <= 0:
        raise ValueError("amount must be positive")
    u64(nonce)
    u64(chain_id)
    u64(gas_limit)
    u128(amount)
    u128(fee)
    if gas_limit == 0:
        raise ValueError("gas limit must be positive")


def address_bytes(value: str) -> bytes:
    normalized = value[2:] if value.startswith("0x") else value
    if len(normalized) != 40:
        raise ValueError("address must be a 20-byte hex string")
    try:
        return bytes.fromhex(normalized)
    except ValueError as exc:
        raise ValueError("address must be valid hex") from exc


def u64(value: int) -> bytes:
    if not 0 <= value <= U64_MAX:
        raise ValueError("integer must fit in u64")
    return struct.pack("<Q", value)


def u128(value: int) -> bytes:
    if not 0 <= value <= U128_MAX:
        raise ValueError("integer must fit in u128")
    return value.to_bytes(16, "little")


def byte_vec(value: bytes) -> bytes:
    return u64(len(value)) + value


def address_set(values: list[bytes]) -> bytes:
    ordered = sorted(values)
    return u64(len(ordered)) + b"".join(ordered)
