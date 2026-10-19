// Explicit public-devnet-only fixture journey. Requires an already deployed program.
import assert from 'node:assert/strict';
import {readFileSync,writeFileSync,mkdirSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {Connection,Keypair,PublicKey,SystemProgram,TransactionInstruction} from '@solana/web3.js';
import {SolanaSettlement,SolanaJournal,TOKEN_PROGRAM,ASSOCIATED_PROGRAM,ata,u64} from './sdk.mjs';
import {Store} from '../../src/store.mjs';
import {json} from '../../src/protocol.mjs';
if(process.env.ARRHA_PUBLIC_SOLANA!=='devnet')throw Error('Explicit ARRHA_PUBLIC_SOLANA=devnet required');
const read=p=>JSON.parse(readFileSync(p,'utf8')),sha=x=>createHash('sha256').update(x).digest();
const m=read(process.env.ARRHA_SOLANA_MANIFEST);
assert.equal(m.genesis,'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG');
assert.equal(m.program,'3nf8gyJnj2aSLyeERjAysJTHCSjuBcxUA2giH8ZXS4hu');
const keydir=process.env.ARRHA_TEST_KEY_DIR,dir=process.env.ARRHA_PUBLIC_WORK;
if(!keydir||!dir)throw Error('Dedicated key directory and persistent work directory required');
mkdirSync(dir,{recursive:true});
const [payer,provider,arbitrator,mint]=await Promise.all(['deployer','provider','arbitrator','mint'].map(async n=>Keypair.fromSecretKey(Uint8Array.from(read(`${keydir}/solana-${n}.json`)))));
assert.equal(payer.publicKey.toBase58(),'6oRGKsgso5kyNeP4apXWrt8yYSQDFCZ87bGBV1jj3aMX');assert.equal(mint.publicKey.toBase58(),m.mint);
const connection=new Connection(process.env.ARRHA_SOLANA_RPC ?? 'https://api.devnet.solana.com','finalized');
const sdk=new SolanaSettlement(connection,m),store=new Store(`${dir}/journal.sqlite`),journal=new SolanaJournal(store,sdk);
// Optional operator-reviewed expiry recovery; never refresh an uncertain blockhash automatically.
const recoveries=process.env.ARRHA_SOLANA_RECOVERY_FILE?read(process.env.ARRHA_SOLANA_RECOVERY_FILE):{};
const pk=(pubkey,isSigner=false,isWritable=false)=>({pubkey,isSigner,isWritable});
try {
 await sdk.verify();assert.ok(await connection.getBalance(payer.publicKey,'finalized')<=5000000000n,'Dedicated test SOL budget exceeded');
 async function send(logicalId,ix,signers=[]) {
  const recovery=recoveries[logicalId],id=recovery?.replacementId ?? logicalId;
  let original;
  if(recovery) {
   original=store.db.prepare('SELECT * FROM solana_operations WHERE id=?').get(logicalId);
   assert.ok(recovery.reason&&id!==logicalId);assert.equal(original?.signature,recovery.originalSignature);
   assert.equal((await connection.getSignatureStatuses([original.signature],{searchTransactionHistory:true})).value[0],null);
   assert.ok(await connection.getBlockHeight('finalized')>original.last_valid,'Original signature may still land');
  }
  const op=await journal.prepare(id,ix,payer,signers);
  if(original)assert.equal(store.db.prepare('SELECT intent_hash FROM solana_operations WHERE id=?').get(id).intent_hash,original.intent_hash,'Recovery must preserve the exact instruction and fee payer');
  await journal.broadcast(id);
  for(let i=0;i<90;i++) {
   const status=(await connection.getSignatureStatuses([op.signature],{searchTransactionHistory:true})).value[0];
   if(status?.err)throw Error(`${id}: ${JSON.stringify(status.err)}`);
   if(status?.confirmationStatus==='finalized'){console.log(json({operation:id,signature:op.signature,state:'finalized'}));return;}
   // Retry only the same persisted bytes while the original blockhash remains valid.
   if(i>0&&i%10===0)await journal.broadcast(id);
   await new Promise(r=>setTimeout(r,2000));
  }
  throw Error(`Finality unavailable: ${id}; retain journal and reconcile existing signature`);
 }
 // The dedicated public payer covers fees and signs every test transaction;
 // provider/arbitrator authorizations remain separate native signatures.
 await send('mint:create',SystemProgram.createAccount({fromPubkey:payer.publicKey,newAccountPubkey:mint.publicKey,lamports:await connection.getMinimumBalanceForRentExemption(82),space:82,programId:TOKEN_PROGRAM}),[mint]);
 await send('mint:initialize',new TransactionInstruction({programId:TOKEN_PROGRAM,keys:[pk(mint.publicKey,false,true)],data:Buffer.concat([Buffer.from([20,6]),payer.publicKey.toBytes(),Buffer.from([0]),Buffer.alloc(32)])}));
 for(const [name,owner] of [['payer',payer],['provider',provider]])await send(`ata:${name}`,new TransactionInstruction({programId:ASSOCIATED_PROGRAM,keys:[pk(payer.publicKey,true,true),pk(await ata(mint.publicKey,owner.publicKey),false,true),pk(owner.publicKey),pk(mint.publicKey),pk(SystemProgram.programId),pk(TOKEN_PROGRAM)],data:Buffer.from([1])}));
 const payerTokens=await ata(mint.publicKey,payer.publicKey),providerTokens=await ata(mint.publicKey,provider.publicKey);
 await send('mint:fixtures',new TransactionInstruction({programId:TOKEN_PROGRAM,keys:[pk(mint.publicKey,false,true),pk(payerTokens,false,true),pk(payer.publicKey,true)],data:Buffer.concat([Buffer.from([7]),u64(100000000n)])}));
 await send('config:initialize',await sdk.initialize(payer.publicKey,1000000n,10000000n));
 const balance=async address=>BigInt((await connection.getTokenAccountBalance(address,'finalized')).value.amount);
 const identity=`public-devnet-v1:${m.program}`;
 let plan=store.db.prepare('SELECT document FROM snapshots WHERE identity=?').get(identity);
 if(plan)plan=JSON.parse(plan.document);
 else {
  const now=Number(await connection.getBlockTime(await connection.getSlot('finalized')));
  plan={baseline:{payer:String(await balance(payerTokens)),provider:String(await balance(providerTokens))},runs:{}};
  for(const [index,name] of ['payout','refund','dispute'].entries())plan.runs[name]={runId:sha(`${identity}:${name}`).toString('hex'),nonce:index+1,fundBy:now+7200,acceptBy:now+10800,deliverBy:now+14400,reviewUntil:now+18000,resolveBy:now+21600};
  store.db.prepare('INSERT INTO snapshots VALUES(?,?)').run(identity,json(plan));
 }
 const evidence={input:'Arrha public devnet v1',output:sha('Arrha public devnet v1').toString('hex')},evidenceHash=sha(json(evidence));store.saveEvidence(evidenceHash.toString('hex'),evidence);
 const runs={};
 for(const [name,r] of Object.entries(plan.runs)) {
  const terms={...r,runId:Buffer.from(r.runId,'hex'),provider:provider.publicKey,arbitrator:arbitrator.publicKey,receiptSigner:provider.publicKey,amount:1000000n,workHash:sha('Return SHA256 of Arrha public devnet v1'),evidencePolicy:sha('Public deterministic fixture; signature is not proof of AI correctness')};
  const state=async()=>(await sdk.inspect(payer.publicKey,terms)).state;
  if(await state()===0)await send(`${name}:fund`,await sdk.fund(payer.publicKey,terms));
  if(name==='refund') {
   if(await state()===1)await send(`${name}:refund`,await sdk.payout('refund',payer.publicKey,payer.publicKey,terms));
   assert.equal(await state(),6);
  } else {
   if(await state()===1)await send(`${name}:accept`,await sdk.act('accept',provider.publicKey,payer.publicKey,terms),[provider]);
   if(await state()===2)await send(`${name}:deliver`,await sdk.deliver(provider.publicKey,provider.publicKey,payer.publicKey,terms,evidenceHash),[provider]);
   if(name==='payout') {
    if(await state()===3)await send(`${name}:settle`,await sdk.payout('settle',payer.publicKey,payer.publicKey,terms));assert.equal(await state(),5);
   } else {
    if(await state()===3)await send(`${name}:dispute`,await sdk.act('dispute',payer.publicKey,payer.publicKey,terms));
    if(await state()===4)await send(`${name}:resolve`,await sdk.payout('resolve',arbitrator.publicKey,payer.publicKey,terms,400000n),[arbitrator]);assert.equal(await state(),7);
   }
  }
  assert.equal(await balance(await ata(mint.publicKey,await sdk.run(payer.publicKey,terms.runId))),0n);runs[name]={runId:r.runId,state:await state()};
 }
 const balances={payer:String(await balance(payerTokens)),provider:String(await balance(providerTokens))};
 assert.equal(BigInt(balances.payer),BigInt(plan.baseline.payer)-1400000n);assert.equal(BigInt(balances.provider),BigInt(plan.baseline.provider)+1400000n);
 const config=await connection.getAccountInfo(sdk.config,'finalized');assert.equal(Buffer.from(config.data).readBigUInt64LE(57),0n);
 const rows=store.db.prepare('SELECT id,signature,last_valid FROM solana_operations').all(),statuses=(await connection.getSignatureStatuses(rows.map(o=>o.signature),{searchTransactionHistory:true})).value;
 const height=await connection.getBlockHeight('finalized');
 const operations=rows.map((o,i)=>{
  const recovery=recoveries[o.id];
  if(recovery){assert.equal(statuses[i],null);assert.ok(height>o.last_valid);return {id:o.id,signature:o.signature,state:'Expired',replacementId:recovery.replacementId};}
  assert.equal(statuses[i]?.err,null);assert.equal(statuses[i]?.confirmationStatus,'finalized');return {id:o.id,signature:o.signature,state:'finalized'};
 });
 const report={network:'Solana devnet',manifest:m,assetName:'aTEST fixture, not USDC; no monetary value',runs,baseline:plan.baseline,balances,outstanding:'0',allFinalized:true,finalityScope:'All applied journey actions; explicitly expired authorizations retained separately',recoveries,operations,observedAt:new Date().toISOString()};
 writeFileSync(`${dir}/report.json`,json(report));console.log(json(report));
}finally{store.close();}
