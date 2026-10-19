import {test} from 'node:test';
import assert from 'node:assert/strict';
import {Contract,ContractFactory,id,Wallet,keccak256} from 'ethers';
import {spawn} from 'node:child_process';
import {writeFileSync,readFileSync} from 'node:fs';
import {resolve} from 'node:path';
import {fixture} from './fixture.mjs';
import {sync} from '../src/indexer.mjs';
import {EvmSettlement,tokenAbi} from '../src/sdk.mjs';
import {agreementHash,json} from '../src/protocol.mjs';
import {createApp} from '../src/server.mjs';

test('real EVM contract, public SDK, persistent journal and canonical HTTP service', {timeout:180000}, async t=>{
 const f=await fixture();t.after(()=>f.close());
 const {provider,wallets,manifest,sdks,artifacts}=f;
 const token=new Contract(manifest.asset,tokenAbi,wallets[0]);
 await (await wallets[0].sendTransaction(await sdks[0].approveRequest(100000000n))).wait();
 const balance=address=>token.balanceOf(address);
 const state=async a=>Number((await sdks[0].inspect(a.runId)).state);
 const liabilities=async()=>assert.equal(await balance(manifest.escrow),await sdks[0].contract.outstanding());
 async function delivered(a){await f.fund(a);await f.send(sdks[1],'accept',a.runId);await f.send(sdks[1],'deliver',a.runId,id('evidence'),await sdks[1].signReceipt(a,id('evidence')));}
 await t.test('signed terms hash matches Solidity; payer settles exact actual funds once',async()=>{
   const a=await f.terms(),before=await balance(wallets[1].address);
   assert.equal(await sdks[0].contract.agreementHash(a),agreementHash(a));
   await delivered(a);await f.send(sdks[0],'settle',a.runId);
   assert.equal(await balance(wallets[1].address)-before,a.amount);assert.equal(await state(a),5);
   await assert.rejects(f.send(sdks[0],'settle',a.runId));await assert.rejects(f.fund(a));await liabilities();
 });
 await t.test('payer cancels unfollowed work and receives exact refund',async()=>{
   const a=await f.terms(),before=await balance(wallets[0].address);await f.fund(a);await f.send(sdks[0],'refund',a.runId);
   assert.equal(await balance(wallets[0].address),before);assert.equal(await state(a),6);await liabilities();
 });
 await t.test('provider never accepts; third party triggers timeout refund',async()=>{
   const a=await f.terms();await f.fund(a);await assert.rejects(f.send(sdks[3],'refund',a.runId));await f.jump(121);await f.send(sdks[3],'refund',a.runId);assert.equal(await state(a),6);await liabilities();
 });
 await t.test('accepted but not delivered; payer cannot cancel early; delivery timeout refunds',async()=>{
   const a=await f.terms();await f.fund(a);await f.send(sdks[1],'accept',a.runId);await assert.rejects(f.send(sdks[0],'refund',a.runId));await f.jump(181);await f.send(sdks[3],'refund',a.runId);assert.equal(await state(a),6);await liabilities();
 });
 await t.test('review deadline permits provider payout; concurrent refund cannot double spend',async()=>{
   const a=await f.terms();await delivered(a);await assert.rejects(f.send(sdks[1],'settle',a.runId));await f.jump(241);
   const results=await Promise.allSettled([f.send(sdks[1],'settle',a.runId),f.send(sdks[3],'refund',a.runId)]);
   assert.equal(results.filter(x=>x.status==='fulfilled').length,1);assert.equal(await state(a),5);await liabilities();
 });
 await t.test('arbitrator splits exact deposit only between agreed parties',async()=>{
   const a=await f.terms();const p=await balance(wallets[0].address),v=await balance(wallets[1].address);
   await delivered(a);await f.send(sdks[0],'dispute',a.runId);
   await assert.rejects(f.send(sdks[3],'resolve',a.runId,1n));await assert.rejects(f.send(sdks[2],'resolve',a.runId,a.amount+1n));
   await f.send(sdks[2],'resolve',a.runId,400000n);
   assert.equal(await state(a),7);assert.equal(p-await balance(wallets[0].address),400000n);assert.equal(await balance(wallets[1].address)-v,400000n);await liabilities();
 });
 await t.test('unavailable arbitrator cannot lock funds past agreed resolution deadline',async()=>{
   const a=await f.terms();await delivered(a);await f.send(sdks[0],'dispute',a.runId);await assert.rejects(f.send(sdks[0],'settle',a.runId));
   await f.jump(301);await assert.rejects(f.send(sdks[2],'resolve',a.runId,1n));await f.send(sdks[3],'refund',a.runId);assert.equal(await state(a),6);await liabilities();
 });
 await t.test('changed amount, recipient, asset and deadlines cannot use original authorization',async()=>{
   const a=await f.terms(),sig=await sdks[0].signAgreement(a);
   for(const patch of [{amount:a.amount+1n},{provider:wallets[3].address},{asset:wallets[3].address},{fundBy:a.fundBy+1}])await assert.rejects(f.send(sdks[0],'fund',{...a,...patch},sig));
   assert.equal(await state(a),0);await liabilities();
 });
 await t.test('expired, revoked and consumed permissions rejected',async()=>{
   const a=await f.terms();await f.send(sdks[0],'revoke',a.nonce);await assert.rejects(f.fund(a));
   const b=await f.terms();const sig=await sdks[0].signAgreement(b);await f.jump(61);await assert.rejects(f.send(sdks[0],'fund',b,sig));
   const c=await f.terms();await f.fund(c);await assert.rejects(f.fund({...c,runId:id('nonce-replay')}));await f.send(sdks[0],'refund',c.runId);await liabilities();
 });
 await t.test('wrong network/deployment signatures rejected and SDK fails closed',async()=>{
   const a=await f.terms();
   for(const patch of [{chainId:421614},{escrow:wallets[4].address}]){
     const bad=new EvmSettlement({...manifest,...patch},artifacts.ArrhaEscrow.abi,wallets[0]);await assert.rejects(bad.verify());
     const {domain,agreementTypes}=await import('../src/protocol.mjs');
     const sig=await wallets[0].signTypedData(domain({...manifest,...patch}),agreementTypes,a);await assert.rejects(f.send(sdks[0],'fund',a,sig));
   }
 });
 await t.test('forged receipt, evidence mutation, wrong sender and repeated delivery rejected',async()=>{
   const a=await f.terms();await f.fund(a);await assert.rejects(f.send(sdks[3],'accept',a.runId));await f.send(sdks[1],'accept',a.runId);
   await assert.rejects(f.send(sdks[1],'deliver',a.runId,id('evidence'),await sdks[3].signReceipt(a,id('evidence'))));
   const sig=await sdks[1].signReceipt(a,id('evidence'));
   await assert.rejects(f.send(sdks[1],'deliver',a.runId,id('altered'),sig));await f.send(sdks[1],'deliver',a.runId,id('evidence'),sig);
   await assert.rejects(f.send(sdks[1],'deliver',a.runId,id('evidence'),sig));await f.send(sdks[0],'settle',a.runId);await liabilities();
 });
 await t.test('journal survives a process exit after broadcast; same hash recovered without repayment',async()=>{
   const a=await f.terms(),sig=await sdks[0].signAgreement(a),req=await sdks[0].request('fund',a,sig),op=await f.store.prepare('crash-after-broadcast',wallets[0],req);
   assert.equal(op.status,'Signed');f.restartStore();
   // A separate process sends the already journaled raw transaction and exits before updating status.
   const child=spawn(process.execPath,['--input-type=module','-e',`import {JsonRpcProvider} from 'ethers'; const p=new JsonRpcProvider(process.env.RPC); await p.broadcastTransaction(process.env.RAW); process.exit(0);`],{cwd:resolve(import.meta.dirname,'..'),env:{...process.env,RPC:provider._getConnection().url,RAW:op.raw},stdio:'pipe'});
   assert.equal(await new Promise(r=>child.on('exit',r)),0);f.restartStore();
   assert.equal((await f.store.reconcile(op.id,provider,manifest.finality)).status,'Included');
   await provider.send('anvil_mine',['0x40','0x0']);
   const recovered=await f.store.reconcile(op.id,provider,manifest.finality);assert.equal(recovered.hash,op.hash);assert.equal(recovered.status,'Finalized');
   await f.store.broadcast(op.id,provider);assert.equal(await state(a),1);assert.equal(await balance(manifest.escrow),a.amount);
   await assert.rejects(f.store.prepare(op.id,wallets[0],{...req,data:'0x1234'}));
   await f.send(sdks[0],'refund',a.runId);await liabilities();
 });
 await t.test('duplicate logs, restart and a real chain reorganization rebuild canonical projection',async()=>{
   const snap=await provider.send('evm_snapshot',[]);const a=await f.terms();await f.fund(a);
   f.store.saveEvidence(id('retained'),{privateReference:'local-only'});
   await provider.send('anvil_mine',['0x40','0x0']);
   const first=await sync(f.store,provider,manifest,artifacts.ArrhaEscrow.abi);assert(first.runs.some(r=>r.runId===a.runId));
   const duplicateProvider=new Proxy(provider,{get(target,key){if(key==='getLogs')return async filter=>{const l=await target.getLogs(filter);return [...l,...l];};const v=Reflect.get(target,key);return typeof v==='function'?v.bind(target):v;}});
   assert.equal((await sync(f.store,duplicateProvider,manifest,artifacts.ArrhaEscrow.abi)).eventCount,first.eventCount);
   f.restartStore();await provider.send('evm_revert',[snap]);await provider.send('anvil_mine',['0x40','0x0']);
   const rebuilt=await sync(f.store,provider,manifest,artifacts.ArrhaEscrow.abi);assert(!rebuilt.runs.some(r=>r.runId===a.runId));assert.equal(rebuilt.outstanding,'0');assert(f.store.db.prepare('SELECT * FROM evidence WHERE hash=?').get(id('retained')));await liabilities();
 });
 await t.test('fee tokens rejected, transfer failure rolls back liabilities, reentrancy blocked and donations cannot be swept',async()=>{
   const attack=await new ContractFactory(artifacts.AttackToken.abi,artifacts.AttackToken.evm.bytecode.object,wallets[0]).deploy();await attack.waitForDeployment();
   const escrow=await new ContractFactory(artifacts.ArrhaEscrow.abi,artifacts.ArrhaEscrow.evm.bytecode.object,wallets[0]).deploy(await attack.getAddress(),6,1000000n,2000000n);await escrow.waitForDeployment();
   const m={...manifest,escrow:await escrow.getAddress(),asset:await attack.getAddress(),assetCodeHash:keccak256(await provider.getCode(await attack.getAddress()))};m.runtimeHash=keccak256(await provider.getCode(m.escrow));
   const sdk=new EvmSettlement(m,artifacts.ArrhaEscrow.abi,wallets[0]);await(await attack.approve(m.escrow,100000000n)).wait();
   const a=await f.terms({asset:m.asset}),sig=await sdk.signAgreement(a);
   await(await attack.configure(false,1n,'0x0000000000000000000000000000000000000000','0x')).wait();
   await assert.rejects(f.send(sdk,'fund',a,sig));assert.equal(await escrow.outstanding(),0n);assert.equal(await escrow.usedNonces(a.payer,a.nonce),false);
   await(await attack.configure(false,0n,'0x0000000000000000000000000000000000000000','0x')).wait();
   await f.send(sdk,'fund',a,sig);
   const tooBig=await f.terms({asset:m.asset,amount:1000001n});await assert.rejects(f.send(sdk,'fund',tooBig,await sdk.signAgreement(tooBig)));
   await(await attack.configure(false,1n,'0x0000000000000000000000000000000000000000','0x')).wait();
   await assert.rejects(f.send(sdk,'refund',a.runId));assert.equal(await escrow.outstanding(),a.amount);
   await(await attack.configure(true,0n,'0x0000000000000000000000000000000000000000','0x')).wait();
   await assert.rejects(f.send(sdk,'refund',a.runId));assert.equal(Number((await sdk.inspect(a.runId)).state),1);assert.equal(await escrow.outstanding(),a.amount);
   await(await attack.configure(false,0n,m.escrow,escrow.interface.encodeFunctionData('refund',[a.runId]))).wait();
   await f.send(sdk,'refund',a.runId);assert.equal(await attack.blockedCalls(),1n);assert.equal(await escrow.outstanding(),0n);
   await(await attack.configure(false,0n,'0x0000000000000000000000000000000000000000','0x')).wait();
   await(await attack.transfer(m.escrow,1n)).wait();assert.equal(await attack.balanceOf(m.escrow),1n);assert.equal(await escrow.outstanding(),0n);
 });
 await t.test('HTTP service reports RPC outage as unavailable; restarts reconstruct actual balances',async()=>{
   const server=createApp({store:f.store,provider,manifest,abi:artifacts.ArrhaEscrow.abi});await new Promise(r=>server.listen(0,'127.0.0.1',r));t.after(()=>new Promise(r=>server.close(r)));
   const url=`http://127.0.0.1:${server.address().port}`;
   await provider.send('anvil_mine',['0x40','0x0']);
   assert.equal((await (await fetch(`${url}/api/state`)).json()).outstanding,'0');assert.equal((await fetch(`${url}/`)).status,200);
   const broken=new Proxy(provider,{get(target,key){if(key==='send')return async()=>{throw Error('RPC outage');};const v=Reflect.get(target,key);return typeof v==='function'?v.bind(target):v;}});
   const bad=createApp({store:f.store,provider:broken,manifest,abi:artifacts.ArrhaEscrow.abi});await new Promise(r=>bad.listen(0,'127.0.0.1',r));
   const response=await fetch(`http://127.0.0.1:${bad.address().port}/api/state`);assert.equal(response.status,503);assert.equal((await response.json()).availability,'unavailable');await new Promise(r=>bad.close(r));
 });
});
