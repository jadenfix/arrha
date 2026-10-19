// SPDX-License-Identifier: MIT
pragma solidity 0.8.37;
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
/// @dev Adversarial E2E fixture, never a supported asset.
contract AttackToken is ERC20 {
    bool public blocked;
    uint256 public fee;
    bool public feeOnTop;
    address public target;
    bytes public attack;
    uint256 public blockedCalls;
    constructor() ERC20("Adversarial Test", "ATTACK") { _mint(msg.sender, 1000000000000); }
    function decimals() public pure override returns(uint8) { return 6; }
    function configure(bool b, uint256 f, address t, bytes calldata a) external { blocked=b;fee=f;target=t;attack=a; }
    function setFeeOnTop(bool value) external { feeOnTop=value; }
    function _update(address from,address to,uint256 amount) internal override {
        require(!blocked,"frozen token");
        if(from==target && target!=address(0)) {
            (bool ok,bytes memory reason)=target.call(attack);
            require(!ok && reason.length>=4 && bytes4(reason)==bytes4(keccak256("ReentrancyGuardReentrantCall()")),"reentry not rejected by guard");
            blockedCalls++;
        }
        if(from!=address(0) && to!=address(0) && fee>0) {super._update(from,address(0),fee);super._update(from,to,feeOnTop?amount:amount-fee);}
        else super._update(from,to,amount);
    }
}
