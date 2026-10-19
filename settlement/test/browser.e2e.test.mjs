import {test} from 'node:test';
import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {createApp} from '../src/server.mjs';
import {fixture} from './fixture.mjs';
import {id,Contract} from 'ethers';
import {tokenAbi} from '../src/sdk.mjs';
const exec=promisify(execFile);
test('browser: independent payer/provider/arbitrator wallets fund, deliver, settle, refund and dispute', {timeout:300000},async t=>{
 const f=await fixture();t.after(()=>f.close());
 const session=`arrha-browser-${process.pid}`;
 const browser=async(...args)=>{const {stdout}=await exec('npx',['--yes','agent-browser@0.39.1','--session',session,...args],{timeout:30000,maxBuffer:4*1024*1024});return stdout;};
 t.after(async()=>{await browser('close').catch(()=>{});});
 const server=createApp({store:f.store,provider:f.provider,manifest:f.manifest,abi:f.artifacts.ArrhaEscrow.abi});
 await new Promise(r=>server.listen(0,'127.0.0.1',r));t.after(()=>new Promise(r=>server.close(r)));
 const url=`http://127.0.0.1:${server.address().port}`;
 await f.provider.send('anvil_mine',['0x40','0x0']);
 await browser('open',url);
 // Local test-only wallet bridge. Actual eth_signTypedData_v4 and eth_sendTransaction hit Anvil.
 // There is no wallet stub or development key route in the application server.
 await browser('eval',`window.testAddress=${JSON.stringify(f.wallets[0].address)};window.ethereum={request:async({method,params=[]})=>{if(method==='eth_requestAccounts'||method==='eth_accounts')return [window.testAddress];const response=await fetch(${JSON.stringify(f.provider._getConnection().url)},{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({jsonrpc:'2.0',id:1,method,params})});const r=await response.json();if(r.error)throw Object.assign(Error(r.error.message),r.error);return r.result;},on:()=>{}};true`);
 async function click(selector){await browser('click',selector);await browser('wait','--fn',`!document.querySelector(${JSON.stringify(selector)}).disabled`);}
 async function account(i){await browser('eval',`window.testAddress=${JSON.stringify(f.wallets[i].address)}`);await click('#connect');assert.match(await browser('get','text','#status'),/Connected/);}
 async function fund(a){await account(0);await browser('fill','#terms',JSON.stringify(a,(_,v)=>typeof v==='bigint'?String(v):v));await click('#review');await click('#sign');assert.match(await browser('get','value','#signature'),/0x[0-9a-f]+/i);await click('#approve');await click('#fund');assert.equal(Number((await f.sdks[0].inspect(a.runId)).state),1);}
 async function delivery(a){await account(1);await click('#accept');await browser('fill','#evidence',id('browser evidence'));await click('#receipt');await click('#deliver');assert.equal(Number((await f.sdks[0].inspect(a.runId)).state),3);}
 const token=new Contract(f.manifest.asset,tokenAbi,f.provider);
 await t.test('successful work pays provider actual token balance',async()=>{
   const a=await f.terms({fundBy:(await f.provider.getBlock('latest')).timestamp+3600,acceptBy:(await f.provider.getBlock('latest')).timestamp+7200,deliverBy:(await f.provider.getBlock('latest')).timestamp+10800,reviewUntil:(await f.provider.getBlock('latest')).timestamp+14400,resolveBy:(await f.provider.getBlock('latest')).timestamp+18000});
   const before=await token.balanceOf(f.wallets[1].address);await fund(a);await delivery(a);await account(0);await click('#settle');assert.equal((await token.balanceOf(f.wallets[1].address))-before,a.amount);
 });
 await t.test('payer funds then cancels and receives exact refund',async()=>{
   const a=await f.terms({fundBy:(await f.provider.getBlock('latest')).timestamp+3600,acceptBy:(await f.provider.getBlock('latest')).timestamp+7200,deliverBy:(await f.provider.getBlock('latest')).timestamp+10800,reviewUntil:(await f.provider.getBlock('latest')).timestamp+14400,resolveBy:(await f.provider.getBlock('latest')).timestamp+18000});
   const before=await token.balanceOf(f.wallets[0].address);await fund(a);await click('#refund');assert.equal(await token.balanceOf(f.wallets[0].address),before);
 });
 await t.test('payer disputes, agreed arbitrator divides funds, service shows canonical resolution',async()=>{
   const now=(await f.provider.getBlock('latest')).timestamp;
   const a=await f.terms({fundBy:now+3600,acceptBy:now+7200,deliverBy:now+10800,reviewUntil:now+14400,resolveBy:now+18000});
   await fund(a);await delivery(a);await account(0);await click('#dispute');await account(2);await browser('fill','#allocation','250000');await click('#resolve');
   assert.equal(Number((await f.sdks[0].inspect(a.runId)).state),7);await f.provider.send('anvil_mine',['0x40','0x0']);await click('#refresh');
   assert.match(await browser('get','text','#activity'),/Resolved/);
   assert.equal(await f.sdks[0].contract.outstanding(),0n);assert.equal(await token.balanceOf(f.manifest.escrow),0n);
 });
 await t.test('lost wallet response recovers by sender nonce and exact calldata without a duplicate payment',async()=>{
   const now=(await f.provider.getBlock('latest')).timestamp;
   const a=await f.terms({fundBy:now+3600,acceptBy:now+7200,deliverBy:now+10800,reviewUntil:now+14400,resolveBy:now+18000});
   await account(0);await browser('fill','#terms',JSON.stringify(a,(_,v)=>typeof v==='bigint'?String(v):v));await click('#review');await click('#sign');await click('#approve');
   await browser('eval',`window.originalRequest=window.ethereum.request;window.ethereum.request=async x=>{const r=await window.originalRequest(x);if(x.method==='eth_sendTransaction'){window.ethereum.request=window.originalRequest;throw Error('Lost response after real broadcast');}return r;};true`);
   await click('#fund');assert.equal(Number((await f.sdks[0].inspect(a.runId)).state),1);
   await f.provider.send('anvil_mine',['0x40','0x0']);await click('#refresh');assert.match(await browser('get','text','#status'),/Recovered transaction/);
   assert.equal(await token.balanceOf(f.manifest.escrow),a.amount);await click('#refund');assert.equal(await token.balanceOf(f.manifest.escrow),0n);
 });
 assert.equal((await exec('npx',['--yes','agent-browser@0.39.1','--session',session,'errors'],{timeout:30000})).stdout.trim(),'');
 const screenshot=process.env.ARRHA_SCREENSHOT;if(screenshot)await browser('screenshot',screenshot,'--full');
});
