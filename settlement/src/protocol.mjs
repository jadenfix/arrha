import { TypedDataEncoder, getAddress } from 'ethers';
export const agreementTypes = { Agreement: [
  ['runId','bytes32'],['payer','address'],['provider','address'],['arbitrator','address'],
  ['receiptSigner','address'],['asset','address'],['amount','uint256'],['workHash','bytes32'],
  ['evidencePolicy','bytes32'],['fundBy','uint256'],['acceptBy','uint256'],['deliverBy','uint256'],
  ['reviewUntil','uint256'],['resolveBy','uint256'],['nonce','uint256']
].map(([name,type])=>({name,type})) };
export const receiptTypes = {Receipt:[{name:'runId',type:'bytes32'},{name:'agreementHash',type:'bytes32'},{name:'evidenceHash',type:'bytes32'}]};
export const states = ['None','Funded','Accepted','Delivered','Disputed','Settled','Refunded','Resolved'];
export const domain = m => ({name:'ArrhaEscrow',version:'1',chainId:BigInt(m.chainId),verifyingContract:getAddress(m.escrow)});
export const agreementHash = a => TypedDataEncoder.hashStruct('Agreement',agreementTypes,a);
export const json = x => JSON.stringify(x,(_,v)=>typeof v === 'bigint' ? v.toString() : v,2);
export const capabilities = Object.freeze({
  evm:{payments:true,escrow:true,disputes:true,refunds:true,scopedAuthorizations:true,receiptAnchoring:true,qualification:'local contract/SDK/service/browser E2E; finalized strict-debit Arbitrum Sepolia SDK journeys; wallet-extension/mainnet unqualified'},
  solana:{payments:true,escrow:true,disputes:true,refunds:true,scopedAuthorizations:true,receiptAnchoring:true,qualification:'local-validator and public-devnet SDK E2E; native transaction authorization; native browser/mainnet unqualified'},
  bitcoin:{payments:true,escrow:false,disputes:false,refunds:false,scopedAuthorizations:false,receiptAnchoring:false,qualification:'Bitcoin Core regtest SDK; no escrow; public payments unqualified'},
  lightning:{payments:true,escrow:false,disputes:false,refunds:false,scopedAuthorizations:false,receiptAnchoring:false,qualification:'two LND nodes on regtest; no escrow; public payments unqualified'}
});
