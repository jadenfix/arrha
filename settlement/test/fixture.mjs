import {spawn} from 'node:child_process';
import {createServer} from 'node:net';
import {mkdtempSync,writeFileSync,readFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {resolve} from 'node:path';
import {JsonRpcProvider,HDNodeWallet,id} from 'ethers';
import {compile,root} from '../src/compile.mjs';
import {deployTest} from '../src/deploy.mjs';
import {Store} from '../src/store.mjs';
import {EvmSettlement} from '../src/sdk.mjs';
import {json} from '../src/protocol.mjs';
export async function fixture() {
  const port=await new Promise(r=>{const s=createServer();s.listen(0,'127.0.0.1',()=>{const p=s.address().port;s.close(()=>r(p));});});
  const child=spawn(resolve(root,'node_modules/.bin/anvil'),['--port',String(port),'--host','127.0.0.1','--silent','--hardfork','cancun'],{stdio:['ignore','ignore','pipe']});
  let err='';child.stderr.on('data',d=>err+=d);
  const provider=new JsonRpcProvider(`http://127.0.0.1:${port}`,undefined,{cacheTimeout:-1});provider.pollingInterval=20;
  let ready=false;
  for(let i=0;i<100;i++){try{await provider.send('eth_chainId',[]);ready=true;break;}catch{await new Promise(r=>setTimeout(r,50));}}
  if(!ready){child.kill();provider.destroy();throw Error(`Anvil failed: ${err}`);}
  const wallets=Array.from({length:5},(_,i)=>HDNodeWallet.fromPhrase('test test test test test test test test test test test junk',undefined,`m/44'/60'/0'/0/${i}`).connect(provider));
  const artifacts=compile(), manifest=await deployTest(wallets[0],artifacts);
  const dir=mkdtempSync(resolve(root,'.local-test-')); // cleaned by close; never export test keys
  const dbPath=resolve(dir,'journal.sqlite');let store=new Store(dbPath);
  const sdks=wallets.map(w=>new EvmSettlement(manifest,artifacts.ArrhaEscrow.abi,w));
  let seq=0;
  async function terms(patch={}){
    const now=(await provider.getBlock('latest')).timestamp;
    seq++;
    return {runId:id(`run-${seq}`),payer:wallets[0].address,provider:wallets[1].address,arbitrator:wallets[2].address,receiptSigner:wallets[1].address,asset:manifest.asset,amount:1000000n,workHash:id('work'),evidencePolicy:id('retain-for-arbitrator-30-days'),fundBy:now+60,acceptBy:now+120,deliverBy:now+180,reviewUntil:now+240,resolveBy:now+300,nonce:seq,...patch};
  }
  async function send(sdk,action,...args){
    const request=await sdk.request(action,...args),opid=`operation-${++seq}`;
    const op=await store.prepare(opid,sdk.runner,request);await store.broadcast(opid,provider);
    const receipt=await provider.waitForTransaction(op.hash,1,10000);
    if(receipt.status!==1)throw Error(`Reverted ${action}`);
    return receipt;
  }
  async function fund(a){return send(sdks[0],'fund',a,await sdks[0].signAgreement(a));}
  async function jump(seconds){await provider.send('evm_increaseTime',[seconds]);await provider.send('evm_mine',[]);}
  return {provider,wallets,artifacts,manifest,dir,dbPath,sdks,terms,send,fund,jump,get store(){return store;},restartStore(){store.close();store=new Store(dbPath);return store;},async close(){store.close();provider.destroy();child.kill('SIGTERM');await new Promise(r=>child.once('exit',r));const {rmSync}=await import('node:fs');rmSync(dir,{recursive:true,force:true});}};
}
