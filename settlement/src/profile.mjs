import {JsonRpcProvider,Contract,keccak256} from 'ethers';
import {writeFileSync} from 'node:fs';
import {json} from './protocol.mjs';
// Read-only preflight: does not approve an asset for production or spend any funds.
const provider=new JsonRpcProvider(process.env.ARRHA_RPC_URL,undefined,{cacheTimeout:-1});
try {
 const chainId=Number(await provider.send('eth_chainId',[]));
 if(chainId!==421614)throw Error('This profile is for Arbitrum Sepolia only');
 const asset='0x75faf114eafb1BDbe2F0316DF893fd58CE46AA4d';
 const token=new Contract(asset,['function decimals() view returns(uint8)'],provider);
 const profile={schemaVersion:1,chainId,asset,assetName:'Circle testnet USDC - no monetary value',decimals:Number(await token.decimals()),assetCodeHash:keccak256(await provider.getCode(asset)),maxPerRun:'1000000',maxOutstanding:'10000000',assetSource:'https://developers.circle.com/stablecoins/usdc-contract-addresses',qualification:'testnet preflight only; issuer proxy/implementation authority requires independent review'};
 if(profile.decimals!==6)throw Error('Unexpected USDC decimals');
 writeFileSync(process.argv[2],json(profile),{flag:'wx'});console.log(json(profile));
} finally {provider.destroy();}
