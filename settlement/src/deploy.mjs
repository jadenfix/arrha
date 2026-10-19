import {Contract,ContractFactory,keccak256} from 'ethers';
/** One immutable asset per deployment. Public test helpers refuse mainnet. */
export async function deployEscrow(wallet,artifact,profile) {
  const chainId=Number(await wallet.provider.send('eth_chainId',[]));
  if(![31337,421614].includes(chainId)||chainId!==Number(profile.chainId))throw Error('Only matching local/Arbitrum Sepolia test profiles enabled');
  const code=await wallet.provider.getCode(profile.asset);
  if(code==='0x'||keccak256(code)!==profile.assetCodeHash)throw Error('Asset bytecode profile mismatch');
  const token=new Contract(profile.asset,['function decimals() view returns(uint8)'],wallet.provider);
  if(Number(await token.decimals())!==profile.decimals)throw Error('Asset decimals profile mismatch');
  const ef=new ContractFactory(artifact.abi,artifact.evm.bytecode.object,wallet);
  const escrow=await ef.deploy(profile.asset,profile.decimals,BigInt(profile.maxPerRun),BigInt(profile.maxOutstanding));await escrow.waitForDeployment();
  const receipt=await escrow.deploymentTransaction().wait();
  return {schemaVersion:1,protocolVersion:'1',chainId,escrow:await escrow.getAddress(),asset:profile.asset,assetCodeHash:profile.assetCodeHash,decimals:profile.decimals,maxPerRun:String(profile.maxPerRun),maxOutstanding:String(profile.maxOutstanding),assetName:profile.assetName,deploymentBlock:receipt.blockNumber,deploymentTransaction:receipt.hash,runtimeHash:keccak256(await wallet.provider.getCode(await escrow.getAddress())),finality:'finalized',maxBackfillBlocks:100000,explorer:chainId===421614?'https://sepolia.arbiscan.io':'',qualification:'unreviewed test deployment; no real funds'};
}
export async function deployTest(wallet,artifacts) {
  const chainId=Number(await wallet.provider.send('eth_chainId',[]));
  if(![31337,421614].includes(chainId))throw Error('Test deployment supports local/Arbitrum Sepolia only');
  const tf=new ContractFactory(artifacts.TestToken.abi,artifacts.TestToken.evm.bytecode.object,wallet);
  const token=await tf.deploy();await token.waitForDeployment();const asset=await token.getAddress();
  return deployEscrow(wallet,artifacts.ArrhaEscrow,{chainId,asset,assetCodeHash:keccak256(await wallet.provider.getCode(asset)),decimals:6,maxPerRun:'10000000',maxOutstanding:'100000000',assetName:'aTEST - no monetary value'});
}
