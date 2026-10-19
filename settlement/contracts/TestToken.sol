// SPDX-License-Identifier: MIT
pragma solidity 0.8.37;
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
/// @dev Test-only stablecoin substitute. Never a real stablecoin or production asset.
contract TestToken is ERC20 {
    constructor() ERC20("Arrha Test USD", "aTEST") { _mint(msg.sender, 1000000000000); }
    function decimals() public pure override returns (uint8) { return 6; }
}
