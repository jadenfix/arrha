// SPDX-License-Identifier: MIT
pragma solidity 0.8.37;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IERC20Metadata} from "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {EIP712} from "@openzeppelin/contracts/utils/cryptography/EIP712.sol";
import {SignatureChecker} from "@openzeppelin/contracts/utils/cryptography/SignatureChecker.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

/// @notice Versioned, non-upgradeable escrow for ONE reviewed, fixed-balance ERC20.
/// No owner, sweep, arbitrary call or destination override. New versions need migration by users.
contract ArrhaEscrow is EIP712, ReentrancyGuard {
    using SafeERC20 for IERC20;
    enum State { None, Funded, Accepted, Delivered, Disputed, Settled, Refunded, Resolved }
    struct Agreement {
        bytes32 runId;
        address payer;
        address provider;
        address arbitrator;
        address receiptSigner;
        address asset;
        uint256 amount;
        bytes32 workHash;
        bytes32 evidencePolicy;
        uint256 fundBy;
        uint256 acceptBy;
        uint256 deliverBy;
        uint256 reviewUntil;
        uint256 resolveBy;
        uint256 nonce;
    }
    struct Escrow { Agreement terms; State state; bytes32 evidence; }
    bytes32 public constant AGREEMENT_TYPEHASH = keccak256("Agreement(bytes32 runId,address payer,address provider,address arbitrator,address receiptSigner,address asset,uint256 amount,bytes32 workHash,bytes32 evidencePolicy,uint256 fundBy,uint256 acceptBy,uint256 deliverBy,uint256 reviewUntil,uint256 resolveBy,uint256 nonce)");
    bytes32 public constant RECEIPT_TYPEHASH = keccak256("Receipt(bytes32 runId,bytes32 agreementHash,bytes32 evidenceHash)");
    IERC20 public immutable token;
    uint8 public immutable decimals;
    uint256 public immutable maxPerRun;
    uint256 public immutable maxOutstanding;
    uint256 public outstanding;
    mapping(bytes32 => Escrow) private escrows;
    mapping(address => mapping(uint256 => bool)) public usedNonces;
    event Funded(bytes32 indexed runId, address indexed payer, address indexed provider, uint256 amount);
    event Transition(bytes32 indexed runId, State state, bytes32 evidence, uint256 providerAmount, uint256 payerAmount);
    event NonceRevoked(address indexed payer, uint256 nonce);
    error InvalidTerms();
    error Unauthorized();
    error InvalidState();
    error Deadline();
    error InvalidSignature();
    error UnsupportedToken();

    constructor(address asset, uint8 assetDecimals, uint256 perRun, uint256 total)
        EIP712("ArrhaEscrow", "1") {
        if (asset.code.length == 0 || perRun == 0 || total < perRun) revert InvalidTerms();
        if (IERC20Metadata(asset).decimals() != assetDecimals) revert UnsupportedToken();
        token = IERC20(asset); decimals = assetDecimals; maxPerRun = perRun; maxOutstanding = total;
    }
    function agreementHash(Agreement memory a) public pure returns (bytes32) {
        // All fields are static ABI words; exactly the EIP-712 encodeData order.
        return keccak256(abi.encode(AGREEMENT_TYPEHASH, a));
    }
    function get(bytes32 runId) external view returns (Escrow memory) { return escrows[runId]; }
    function revoke(uint256 nonce) external { usedNonces[msg.sender][nonce] = true; emit NonceRevoked(msg.sender, nonce); }
    function fund(Agreement calldata a, bytes calldata signature) external nonReentrant {
        if (a.runId == 0 || a.payer == address(0) || a.provider == address(0) ||
            a.provider == a.payer || a.arbitrator == address(0) || a.arbitrator == a.payer ||
            a.arbitrator == a.provider || a.receiptSigner == address(0) || a.asset != address(token) ||
            a.amount == 0 || a.amount > maxPerRun || outstanding + a.amount > maxOutstanding ||
            a.workHash == 0 || a.evidencePolicy == 0 ||
            !(a.fundBy < a.acceptBy && a.acceptBy < a.deliverBy &&
              a.deliverBy < a.reviewUntil && a.reviewUntil < a.resolveBy)) revert InvalidTerms();
        if (block.timestamp > a.fundBy) revert Deadline();
        if (escrows[a.runId].state != State.None || usedNonces[a.payer][a.nonce]) revert InvalidState();
        if (!SignatureChecker.isValidSignatureNow(a.payer, _hashTypedDataV4(agreementHash(a)), signature)) revert InvalidSignature();
        usedNonces[a.payer][a.nonce] = true;
        escrows[a.runId] = Escrow(a, State.Funded, bytes32(0));
        outstanding += a.amount;
        uint256 beforeBalance = token.balanceOf(address(this));
        uint256 payerBefore = token.balanceOf(a.payer);
        token.safeTransferFrom(a.payer, address(this), a.amount);
        if (token.balanceOf(address(this)) != beforeBalance + a.amount ||
            token.balanceOf(a.payer) != payerBefore - a.amount) revert UnsupportedToken();
        emit Funded(a.runId, a.payer, a.provider, a.amount);
    }
    function accept(bytes32 id) external {
        Escrow storage e = escrows[id];
        if (msg.sender != e.terms.provider) revert Unauthorized();
        if (e.state != State.Funded) revert InvalidState();
        if (block.timestamp > e.terms.acceptBy) revert Deadline();
        e.state = State.Accepted; emit Transition(id, e.state, 0, 0, 0);
    }
    function deliver(bytes32 id, bytes32 evidence, bytes calldata signature) external nonReentrant {
        Escrow storage e = escrows[id];
        if (msg.sender != e.terms.provider) revert Unauthorized();
        if (e.state != State.Accepted) revert InvalidState();
        if (block.timestamp > e.terms.deliverBy) revert Deadline();
        if (evidence == 0) revert InvalidTerms();
        bytes32 receipt = keccak256(abi.encode(RECEIPT_TYPEHASH, id, agreementHash(e.terms), evidence));
        if (!SignatureChecker.isValidSignatureNow(e.terms.receiptSigner, _hashTypedDataV4(receipt), signature)) revert InvalidSignature();
        e.evidence = evidence; e.state = State.Delivered;
        emit Transition(id, e.state, evidence, 0, 0);
    }
    function settle(bytes32 id) external nonReentrant {
        Escrow storage e = escrows[id];
        if (e.state != State.Delivered) revert InvalidState();
        if (msg.sender != e.terms.payer && block.timestamp <= e.terms.reviewUntil) revert Unauthorized();
        _pay(id, e, State.Settled, e.terms.amount);
    }
    function dispute(bytes32 id) external {
        Escrow storage e = escrows[id];
        if (msg.sender != e.terms.payer) revert Unauthorized();
        if (e.state != State.Delivered) revert InvalidState();
        if (block.timestamp > e.terms.reviewUntil) revert Deadline();
        e.state = State.Disputed; emit Transition(id, e.state, e.evidence, 0, 0);
    }
    function resolve(bytes32 id, uint256 providerAmount) external nonReentrant {
        Escrow storage e = escrows[id];
        if (msg.sender != e.terms.arbitrator) revert Unauthorized();
        if (e.state != State.Disputed) revert InvalidState();
        if (block.timestamp > e.terms.resolveBy) revert Deadline();
        if (providerAmount > e.terms.amount) revert InvalidTerms();
        _pay(id, e, State.Resolved, providerAmount);
    }
    function refund(bytes32 id) external nonReentrant {
        Escrow storage e = escrows[id];
        bool cancel = e.state == State.Funded && msg.sender == e.terms.payer;
        bool timeout = (e.state == State.Funded && block.timestamp > e.terms.acceptBy) ||
            (e.state == State.Accepted && block.timestamp > e.terms.deliverBy) ||
            (e.state == State.Disputed && block.timestamp > e.terms.resolveBy);
        if (!cancel && !timeout) revert InvalidState();
        _pay(id, e, State.Refunded, 0);
    }
    function _pay(bytes32 id, Escrow storage e, State terminal, uint256 providerAmount) private {
        e.state = terminal;
        uint256 payerAmount = e.terms.amount - providerAmount;
        outstanding -= e.terms.amount;
        if (providerAmount > 0) _transferExact(e.terms.provider, providerAmount);
        if (payerAmount > 0) _transferExact(e.terms.payer, payerAmount);
        emit Transition(id, terminal, e.evidence, providerAmount, payerAmount);
    }
    function _transferExact(address recipient, uint256 amount) private {
        uint256 escrowBefore = token.balanceOf(address(this));
        uint256 recipientBefore = token.balanceOf(recipient);
        token.safeTransfer(recipient, amount);
        if (token.balanceOf(address(this)) != escrowBefore - amount ||
            token.balanceOf(recipient) != recipientBefore + amount) revert UnsupportedToken();
    }

}
