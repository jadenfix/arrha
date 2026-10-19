import { Contract, keccak256 } from 'ethers';
import { domain, agreementTypes, receiptTypes, agreementHash } from './protocol.mjs';
export const tokenAbi = ['function approve(address,uint256) returns(bool)','function allowance(address,address) view returns(uint256)','function balanceOf(address) view returns(uint256)','function decimals() view returns(uint8)'];
export async function verifyDeployment(provider,m,abi) {
  if (BigInt(await provider.send('eth_chainId',[])) !== BigInt(m.chainId)) throw Error('Wrong network');
  if (keccak256(await provider.getCode(m.escrow)) !== m.runtimeHash) throw Error('Wrong deployment bytecode');
  if(m.assetCodeHash && keccak256(await provider.getCode(m.asset))!==m.assetCodeHash)throw Error('Asset bytecode changed');
  const c = new Contract(m.escrow,abi,provider);
  if ((await c.token()).toLowerCase() !== m.asset.toLowerCase() || Number(await c.decimals()) !== m.decimals) throw Error('Wrong asset or decimals');
  return c;
}
/** No Arrha node/consensus dependency. Wallets remain caller-owned. */
export class EvmSettlement {
  constructor(manifest,abi,runner) { this.manifest=manifest; this.abi=abi; this.runner=runner; this.contract=new Contract(manifest.escrow,abi,runner); }
  async verify() { return verifyDeployment(this.runner.provider ?? this.runner,this.manifest,this.abi); }
  async signAgreement(terms) { await this.verify(); return this.runner.signTypedData(domain(this.manifest),agreementTypes,terms); }
  async signReceipt(terms,evidenceHash) { await this.verify(); return this.runner.signTypedData(domain(this.manifest),receiptTypes,{runId:terms.runId,agreementHash:agreementHash(terms),evidenceHash}); }
  async request(action,...args) { await this.verify(); return this.contract[action].populateTransaction(...args); }
  async approveRequest(amount) { await this.verify(); return new Contract(this.manifest.asset,tokenAbi,this.runner).approve.populateTransaction(this.manifest.escrow,amount); }
  async inspect(id) { await this.verify(); return this.contract.get(id); }
}
