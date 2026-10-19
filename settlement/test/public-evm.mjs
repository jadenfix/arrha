// Explicit opt-in: real Arbitrum Sepolia transactions, never mainnet. Not part of npm test.
import assert from 'node:assert/strict';
import {readFileSync,writeFileSync,mkdirSync} from 'node:fs';
import {resolve} from 'node:path';
import {JsonRpcProvider,Wallet,Contract,id,sha256,toUtf8Bytes} from 'ethers';
import {EvmSettlement,tokenAbi} from '../src/sdk.mjs';
import {Store} from '../src/store.mjs';
import {json,states} from '../src/protocol.mjs';
import {sync} from '../src/indexer.mjs';

if(process.env.ARRHA_PUBLIC_TESTNET!=='421614')throw Error('Explicit ARRHA_PUBLIC_TESTNET=421614 required');
const dir=resolve(process.env.ARRHA_PUBLIC_WORK ?? '.local/public');mkdirSync(dir,{recursive:true});
const read=p=>JSON.parse(readFileSync(p,'utf8'));
const m=read(new URL('../deployments/arbitrum-sepolia-usdc.json',import.meta.url));
assert.equal(m.chainId,421614);assert.equal(m.asset.toLowerCase(),'0x75faf114eafb1bdbe2f0316df893fd58ce46aa4d');
const p=new JsonRpcProvider(process.env.ARRHA_RPC_URL,undefined,{cacheTimeout:-1});p.pollingInterval=1000;
const db=new Store(`${dir}/journal.sqlite`);
try {
  assert.equal(Number(await p.send('eth_chainId',[])),421614);
  const keydir=process.env.ARRHA_TEST_KEY_DIR;if(!keydir)throw Error('Dedicated test key directory required');
  const [payer,provider,arbitrator]=['deployer','provider','arbitrator'].map(n=>new Wallet(read(`${keydir}/${n}.json`).privateKey,p));
  assert.equal(payer.address,'0x551fB2a948b56Ab1Da7ca76e829a2bf3b267648D');
  const abi=read(new URL('../artifacts/ArrhaEscrow.json',import.meta.url)).abi;
  const sdk=new EvmSettlement(m,abi,payer), worker=new EvmSettlement(m,abi,provider), judge=new EvmSettlement(m,abi,arbitrator);
  await sdk.verify();const c=sdk.contract,t=new Contract(m.asset,tokenAbi,p);
  assert.equal(await c.maxPerRun(),1000000n);assert.equal(await c.maxOutstanding(),10000000n);
  const balances=async()=>Object.fromEntries(await Promise.all([payer,provider,{address:m.escrow}].map(async w=>[w.address,String(await t.balanceOf(w.address))])));
  const snapshotKey=`public-journeys-v1:${m.escrow}`;
  let plan=db.db.prepare('SELECT document FROM snapshots WHERE identity=?').get(snapshotKey);
  if(plan)plan=JSON.parse(plan.document);
  else {
    assert.ok(await t.balanceOf(payer.address)>=3000000n,'Need three test USDC');
    assert.ok(await p.getBalance(payer.address)<=1000000000000000n,'Dedicated gas budget exceeded');
    const now=(await p.getBlock('latest')).timestamp;
    plan={createdAt:new Date().toISOString(),baseline:await balances(),runs:{}};
    for(const name of ['payout','refund','dispute']) {
      const terms={runId:id(`${snapshotKey}:${name}`),payer:payer.address,provider:provider.address,arbitrator:arbitrator.address,receiptSigner:provider.address,asset:m.asset,amount:'1000000',workHash:sha256(toUtf8Bytes('Return SHA256 of Arrha public test v1')),evidencePolicy:id('Public deterministic fixture; signature attests bytes, not AI correctness'),fundBy:now+7200,acceptBy:now+10800,deliverBy:now+14400,reviewUntil:now+18000,resolveBy:now+21600,nonce:id(`${snapshotKey}:${name}:nonce`)};
      const evidence={task:'SHA256',input:'Arrha public test v1',output:sha256(toUtf8Bytes('Arrha public test v1'))};
      const evidenceHash=sha256(toUtf8Bytes(json(evidence)));
      plan.runs[name]={terms,signature:await sdk.signAgreement(terms),evidence,evidenceHash,receipt:await worker.signReceipt(terms,evidenceHash)};
      db.saveEvidence(evidenceHash,evidence);
    }
    db.db.prepare('INSERT INTO snapshots VALUES(?,?)').run(snapshotKey,json(plan));
  }
  const opKey=name=>`${m.escrow.toLowerCase()}:${name}`;
  async function send(name,wallet,request) {
    name=opKey(name);
    const op=await db.prepare(name,wallet,request);await db.broadcast(name,p);
    const receipt=await p.waitForTransaction(op.hash,1,60000);assert.equal(receipt.status,1,`${name} reverted`);
    const status=await db.reconcile(name,p,m.finality);console.log(json({operation:name,hash:op.hash,block:receipt.blockNumber,status:status.status}));
  }
  const active=await Promise.all(Object.values(plan.runs).map(async r=>Number((await sdk.inspect(r.terms.runId)).state)));
  // Completed plans never create another allowance or gas top-up on restart.
  if(active.some(state=>state<5)){
   // Fixed, once-only top-ups remain within the original 0.001 test ETH.
   for(const [name,w,value] of [['provider-gas',provider,50000000000000n],['arbitrator-gas',arbitrator,25000000000000n]]) {
    if(db.operation(opKey(name))||await p.getBalance(w.address)<value/2n)await send(name,payer,{to:w.address,value});
   }
   const approval='approve-three-test-usdc';
   if(db.operation(opKey(approval))||await t.allowance(payer.address,m.escrow)<3000000n)await send(approval,payer,await sdk.approveRequest(3000000n));
  }
  for(const [name,r] of Object.entries(plan.runs)) {
    const run=r.terms.runId;
    const state=async()=>Number((await sdk.inspect(run)).state);
    if(await state()===0)await send(`${name}:fund`,payer,await sdk.request('fund',r.terms,r.signature));
    if(name==='refund') {
      if(await state()===1)await send(`${name}:refund`,payer,await sdk.request('refund',run));
      assert.equal(await state(),6);
    } else {
      if(await state()===1)await send(`${name}:accept`,provider,await worker.request('accept',run));
      if(await state()===2)await send(`${name}:deliver`,provider,await worker.request('deliver',run,r.evidenceHash,r.receipt));
      if(name==='payout') {
        if(await state()===3)await send(`${name}:settle`,payer,await sdk.request('settle',run));
        assert.equal(await state(),5);
      } else {
        if(await state()===3)await send(`${name}:dispute`,payer,await sdk.request('dispute',run));
        if(await state()===4)await send(`${name}:resolve`,arbitrator,await judge.request('resolve',run,400000n));
        assert.equal(await state(),7);
      }
    }
    console.log(json({journey:name,state:states[await state()]}));
  }
  const after=await balances();
  assert.equal(BigInt(after[payer.address]),BigInt(plan.baseline[payer.address])-1400000n);
  assert.equal(BigInt(after[provider.address]),BigInt(plan.baseline[provider.address])+1400000n);
  assert.equal(after[m.escrow],plan.baseline[m.escrow]);assert.equal(await c.outstanding(),0n);
  const operations=[];
  for(const row of db.db.prepare('SELECT id FROM operations WHERE id LIKE ? ORDER BY nonce').all(`${m.escrow.toLowerCase()}:%`)) {
    const op=await db.reconcile(row.id,p,m.finality);operations.push({id:op.id,hash:op.hash,status:op.status});
  }
  const projection=await sync(db,p,m,abi);
  const report={network:'Arbitrum Sepolia',chainId:421614,escrow:m.escrow,asset:m.asset,observedAt:new Date().toISOString(),baseline:plan.baseline,balances:after,outstanding:'0',runs:Object.fromEntries(Object.entries(plan.runs).map(([name,r])=>[name,{runId:r.terms.runId,evidenceHash:r.evidenceHash}])),operations,projection,allFinalized:operations.every(o=>o.status==='Finalized')};
  writeFileSync(`${dir}/report.json`,json(report));console.log(json(report));
} finally {db.close();p.destroy();}
