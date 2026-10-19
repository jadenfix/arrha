import {BrowserProvider,Contract,formatUnits} from '/ethers.js';
import {EvmSettlement} from '/sdk.mjs';
import {json} from '/protocol.mjs';
const $=id=>document.getElementById(id);
let config,sdk,provider,signer;
const terms=()=>JSON.parse($('terms').value);
const status=s=>$('status').textContent=s;
const key=()=>`arrha-pending:${config.manifest.chainId}:${config.manifest.escrow}`;
async function refresh(){
  const r=await fetch('/api/state'), state=await r.json();
  $('activity').textContent=json(state); $('freshness').textContent=r.ok?`Observed ${state.observedAt}; block ${state.block}; policy: ${state.finalityPolicy}.`:'Chain state unavailable. No success can be confirmed.';
  if(!r.ok) throw Error(state.error);
  return state;
}
async function connected(){
  if(!window.ethereum) throw Error('Install or connect an Ethereum wallet.');
  provider=new BrowserProvider(window.ethereum,'any'); await provider.send('eth_requestAccounts',[]);
  signer=await provider.getSigner(); sdk=new EvmSettlement(config.manifest,config.abi,signer);await sdk.verify();
  status(`Connected ${await signer.getAddress()}. Verify terms before authorizing payment.`);
  window.ethereum.on?.('chainChanged',()=>{sdk=null;status('Network changed. Reconnect before acting.');});
  window.ethereum.on?.('accountsChanged',()=>{sdk=null;status('Account changed. Reconnect before acting.');});
}
async function send(request){
  if(!sdk) throw Error('Connect the correct wallet first.'); await sdk.verify();
  // An unknown wallet response is never automatically retried. Recover by run ID on chain.
  const pending=localStorage.getItem(key());
  if(pending) throw Error('A wallet operation is unresolved. Refresh canonical activity and reconnect after checking your wallet history.');
  request={...request,nonce:await signer.getNonce('pending')};
  const from=await signer.getAddress(),startBlock=await provider.getBlockNumber();
  localStorage.setItem(key(),json({request,from,startBlock,createdAt:new Date().toISOString(),state:'Awaiting wallet'}));
  let tx;
  try {tx=await signer.sendTransaction(request);}
  catch(e){if(e.code==='ACTION_REJECTED')localStorage.removeItem(key());throw e;}
  localStorage.setItem(key(),json({hash:tx.hash,state:'Submitted'}));status(`Submitted ${tx.hash}. Awaiting inclusion.`);
  const a=document.createElement('a');a.textContent=tx.hash;
  if(config.manifest.explorer){a.href=`${config.manifest.explorer}/tx/${tx.hash}`;a.target='_blank';a.rel='noopener';}
  $('transactions').replaceChildren(a);
  const receipt=await tx.wait();
  if(receipt.status!==1) {localStorage.removeItem(key());throw Error('Transaction failed');}
  status(`Included ${tx.hash}. Refreshing canonical state under ${config.manifest.finality} policy.`);
  await refresh();localStorage.removeItem(key());
}
async function recoverPending(){
  const pending=JSON.parse(localStorage.getItem(key()) ?? 'null');if(!pending||!provider||!sdk)return;
  await sdk.verify();
  let hash=pending.hash;
  if(!hash&&pending.from&&pending.request?.nonce!==undefined){
    const latest=await provider.getBlockNumber();if(latest-pending.startBlock>1000)throw Error('Recovery range exceeded. Inspect wallet history before any retry.');
    for(let number=pending.startBlock;number<=latest;number++){
      const block=await provider.send('eth_getBlockByNumber',[`0x${number.toString(16)}`,true]);
      const tx=block?.transactions.find(tx=>tx.from.toLowerCase()===pending.from.toLowerCase()&&BigInt(tx.nonce)===BigInt(pending.request.nonce));
      if(tx){
        if(tx.to?.toLowerCase()!==pending.request.to?.toLowerCase()||tx.input!==pending.request.data||BigInt(tx.value)!==BigInt(pending.request.value ?? 0))throw Error('Nonce consumed by a different transaction. Inspect wallet history; no automatic retry.');
        hash=tx.hash;localStorage.setItem(key(),json({...pending,hash,state:'Recovered inclusion'}));break;
      }
    }
  }
  if(!hash)throw Error('Wallet response still unresolved. No matching canonical transaction found; sending remains blocked.');
  const receipt=await provider.getTransactionReceipt(hash);
  if(receipt){
    const block=await provider.getBlock(receipt.blockNumber);if(block?.hash!==receipt.blockHash)throw Error('Recovered receipt is not canonical.');
    localStorage.removeItem(key());status(`Recovered transaction ${hash}: ${receipt.status===1?'included; see canonical state':'failed'}.`);
  }
}
function review(){
  const a=terms();$('run').value=a.runId;
  $('summary').textContent=`Provider: ${a.provider}\nPayer: ${a.payer}\nAsset: ${a.asset}\nAmount: ${formatUnits(a.amount,config.manifest.decimals)} (${a.amount} units)\nArbitrator: ${a.arbitrator}\nReceipt signer: ${a.receiptSigner}\n${['fundBy','acceptBy','deliverBy','reviewUntil','resolveBy'].map(k=>`${k}: ${new Date(Number(a[k])*1000).toISOString()}`).join('\n')}\nWork hash: ${a.workHash}\nEvidence policy: ${a.evidencePolicy}`;
}
const actions={connect:connected,refresh:async()=>{
  await refresh();await recoverPending();
},review,sign:async()=>{review();$('signature').value=await sdk.signAgreement(terms());},approve:async()=>{review();await send(await sdk.approveRequest(terms().amount));},fund:async()=>send(await sdk.request('fund',terms(),$('signature').value.trim())),receipt:async()=>{$('receipt-signature').value=await sdk.signReceipt(terms(),$('evidence').value.trim());}};
for(const name of ['accept','settle','dispute','refund'])actions[name]=async()=>send(await sdk.request(name,$('run').value.trim()));
actions.deliver=async()=>send(await sdk.request('deliver',$('run').value.trim(),$('evidence').value.trim(),$('receipt-signature').value.trim()));
actions.resolve=async()=>send(await sdk.request('resolve',$('run').value.trim(),BigInt($('allocation').value)));
for(const [id,fn] of Object.entries(actions))$(id).addEventListener('click',async()=>{const b=$(id);b.disabled=true;try{if(!sdk&&!['connect','refresh','review'].includes(id))throw Error('Connect the correct wallet first.');await fn();}catch(e){status(e.shortMessage ?? e.message);}finally{b.disabled=false;}});
try {config=await (await fetch('/api/config')).json();$('network').textContent=`Chain ${config.manifest.chainId} · ${config.manifest.assetName} · ${config.manifest.qualification}`;await refresh();}
catch(e){status(e.message);}
