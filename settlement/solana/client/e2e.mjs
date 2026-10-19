import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawn,execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {createServer} from 'node:net';
import {mkdtempSync,writeFileSync,readFileSync,rmSync} from 'node:fs';
import {resolve} from 'node:path';
import {createHash} from 'node:crypto';
import {Connection,Keypair,PublicKey,SystemProgram,Transaction,TransactionInstruction,sendAndConfirmTransaction} from '@solana/web3.js';
import {SolanaSettlement,SolanaJournal,PROGRAM_ID,TOKEN_PROGRAM,ASSOCIATED_PROGRAM,LOADER,ata,u64} from './sdk.mjs';
import {Store} from '../../src/store.mjs';
const execute=promisify(execFile),pk=(pubkey,isSigner=false,isWritable=false)=>({pubkey,isSigner,isWritable});
const freePort=()=>new Promise(r=>{const s=createServer();s.listen(0,'127.0.0.1',()=>{const p=s.address().port;s.close(()=>r(p));});});
const sha=x=>createHash('sha256').update(x).digest();
test('Solana local validator: actual SPL-token escrow with native signatures and durable submission',{timeout:360000},async t=>{
 const tools=process.env.ARRHA_SOLANA_BIN;if(!tools)throw Error('Set ARRHA_SOLANA_BIN to verified Agave bin directory');
 const nativeEnv={...process.env,...(process.env.ARRHA_SOLANA_DYLD_PATH?{DYLD_LIBRARY_PATH:process.env.ARRHA_SOLANA_DYLD_PATH}:{})};
 const dir=mkdtempSync(resolve(import.meta.dirname,'../../../../s-'));
 const rpcPort=await freePort();let faucetPort=await freePort();while(faucetPort===rpcPort+1||faucetPort===rpcPort)faucetPort=await freePort();let gossipPort=await freePort();while([rpcPort,rpcPort+1,faucetPort].includes(gossipPort))gossipPort=await freePort();
 const log=await import('node:fs').then(f=>f.openSync(`${dir}/validator.log`,'w'));
 const child=spawn(`${tools}/solana-test-validator`,['--ledger',`${dir}/ledger`,'--rpc-port',String(rpcPort),'--faucet-port',String(faucetPort),'--gossip-port',String(gossipPort),'--dynamic-port-range','23000-23030','--bind-address','127.0.0.1','--limit-ledger-size','50000','--ticks-per-slot','8','--quiet'],{stdio:['ignore',log,log],env:nativeEnv});
 let store; t.after(async()=>{store?.close();child.kill('SIGTERM');await new Promise(r=>{if(child.exitCode!==null||child.signalCode!==null)r();else child.once('exit',r);});await import('node:fs').then(f=>f.closeSync(log));rmSync(dir,{recursive:true,force:true});});
 const url=`http://127.0.0.1:${rpcPort}`,connection=new Connection(url,'finalized');
 async function confirm(signature){for(let i=0;i<200;i++){const s=(await connection.getSignatureStatuses([signature],{searchTransactionHistory:true})).value[0];if(s?.err)throw Error(JSON.stringify(s.err));if(s?.confirmationStatus==='finalized')return;await new Promise(r=>setTimeout(r,150));}throw Error(`Finality unavailable for ${signature}`);}
 async function sendMany(ixs,signers){const latest=await connection.getLatestBlockhash('finalized');const tx=new Transaction({feePayer:signers[0].publicKey,...latest}).add(...ixs);await tx.sign(...signers);const raw=await tx.serialize();const sig=await connection.sendRawTransaction(raw);await confirm(sig);return sig;}
 let ready=false;for(let i=0;i<240;i++){if(child.exitCode!==null||child.signalCode!==null)throw Error(readFileSync(`${dir}/validator.log`,'utf8'));try{await connection.getGenesisHash();ready=true;break;}catch{await new Promise(r=>setTimeout(r,250));}}
 console.log('Validator RPC ready',url);
 if(!ready)throw Error(`Validator did not start: ${readFileSync(`${dir}/validator.log`,'utf8').slice(-5000)}`);
 const wallets=await Promise.all([1,2,3,4,42].map(n=>Keypair.fromSeed(new Uint8Array(32).fill(n))));
 const [payer,provider,arbitrator,stranger,programKey]=wallets;
 assert(programKey.publicKey.equals(PROGRAM_ID));
 for(const w of [payer,provider,arbitrator,stranger]){const sig=await connection.requestAirdrop(w.publicKey,10000000000n);await confirm(sig);console.log('Airdrop finalized',w.publicKey.toBase58());}
 writeFileSync(`${dir}/payer.json`,JSON.stringify(Array.from(payer.secretKey)),{mode:0o600});writeFileSync(`${dir}/program.json`,JSON.stringify(Array.from(programKey.secretKey)),{mode:0o600});
 await execute(`${tools}/solana`,['program','deploy','--url',url,'--keypair',`${dir}/payer.json`,'--program-id',`${dir}/program.json`,'--use-rpc',resolve(import.meta.dirname,'../target/deploy/arrha_solana.so')],{timeout:180000,maxBuffer:200000,env:nativeEnv});
 console.log('Program deployed');
 const mint=await Keypair.generate();
 async function send(ix,signers=[payer]){return sendMany([ix],signers);}
 const create=SystemProgram.createAccount({fromPubkey:payer.publicKey,newAccountPubkey:mint.publicKey,lamports:await connection.getMinimumBalanceForRentExemption(82),space:82,programId:TOKEN_PROGRAM});
 const initializeMint=new TransactionInstruction({programId:TOKEN_PROGRAM,keys:[pk(mint.publicKey,false,true)],data:Buffer.concat([Buffer.from([20,6]),payer.publicKey.toBytes(),Buffer.from([0]),Buffer.alloc(32)])});
 await sendMany([create,initializeMint],[payer,mint]);
 for(const owner of [payer,provider,stranger]){const address=await ata(mint.publicKey,owner.publicKey);await send(new TransactionInstruction({programId:ASSOCIATED_PROGRAM,keys:[pk(payer.publicKey,true,true),pk(address,false,true),pk(owner.publicKey),pk(mint.publicKey),pk(SystemProgram.programId),pk(TOKEN_PROGRAM)],data:Buffer.from([1])}));}
 const payerTokens=await ata(mint.publicKey,payer.publicKey),providerTokens=await ata(mint.publicKey,provider.publicKey);
 await send(new TransactionInstruction({programId:TOKEN_PROGRAM,keys:[pk(mint.publicKey,false,true),pk(payerTokens,false,true),pk(payer.publicKey,true)],data:Buffer.concat([Buffer.from([7]),u64(100000000n)])}));
 const dataKey=(await PublicKey.findProgramAddress([PROGRAM_ID.toBytes()],LOADER))[0],data=await connection.getAccountInfo(dataKey,'finalized');
 const manifest={program:PROGRAM_ID.toBase58(),mint:mint.publicKey.toBase58(),genesis:await connection.getGenesisHash(),decimals:6,maxPerRun:'10000000',maxOutstanding:'100000000',programDataHash:sha(data.data.subarray(45)).toString('hex')};
 const sdk=new SolanaSettlement(connection,manifest);await assert.rejects(send(await sdk.initialize(stranger.publicKey,10000000n,100000000n),[stranger]));await send(await sdk.initialize(payer.publicKey,10000000n,100000000n));
 store=new Store(`${dir}/journal.sqlite`);let journal=new SolanaJournal(store,sdk),counter=0;
 const amount=async address=>BigInt((await connection.getTokenAccountBalance(address,'finalized')).value.amount);
 async function terms(overrides={}){counter++;const slot=await connection.getSlot('finalized'),now=Number(await connection.getBlockTime(slot));return {runId:sha(`solana-run-${counter}`),provider:provider.publicKey,arbitrator:arbitrator.publicKey,receiptSigner:provider.publicKey,amount:1000000n,workHash:sha('work'),evidencePolicy:sha('policy'),fundBy:now+60,acceptBy:now+120,deliverBy:now+180,reviewUntil:now+240,resolveBy:now+300,nonce:counter,...overrides};}
 async function durable(ix,signers=[payer]){const id=`op-${++counter}`;const op=await journal.prepare(id,ix,signers[0],signers);await journal.broadcast(id);await confirm(op.signature);return op;}
 await t.test('wrong initializer, actual funding/delivery/settlement and fixed recipient balances',async()=>{
   await assert.rejects(new SolanaSettlement(connection,{...manifest,maxPerRun:'100000000'}).verify(),/Wrong Solana caps/);
   await assert.rejects(new SolanaSettlement(connection,{...manifest,maxOutstanding:'1000000000'}).verify(),/Wrong Solana caps/);
   const a=await terms(),before=await amount(providerTokens);
   await durable(await sdk.fund(payer.publicKey,a));assert.equal((await sdk.inspect(payer.publicKey,a)).state,1);
   await assert.rejects(durable(await sdk.act('accept',stranger.publicKey,payer.publicKey,a),[stranger]));
   await durable(await sdk.act('accept',provider.publicKey,payer.publicKey,a),[provider]);
   await assert.rejects(durable(await sdk.deliver(provider.publicKey,stranger.publicKey,payer.publicKey,a,sha('evidence')),[provider,stranger]));
   await durable(await sdk.deliver(provider.publicKey,provider.publicKey,payer.publicKey,a,sha('evidence')),[provider]);
   await assert.rejects(durable(await sdk.payout('settle',provider.publicKey,payer.publicKey,a),[provider]));
   await durable(await sdk.payout('settle',payer.publicKey,payer.publicKey,a));assert.equal(await amount(providerTokens)-before,1000000n);
   await assert.rejects(durable(await sdk.payout('settle',payer.publicKey,payer.publicKey,a)));
   await assert.rejects(durable(await sdk.fund(payer.publicKey,a)));
 });
 await t.test('cancel refund and nonce revocation are enforced by actual accounts',async()=>{
   const a=await terms(),before=await amount(payerTokens);await durable(await sdk.fund(payer.publicKey,a));await durable(await sdk.payout('refund',payer.publicKey,payer.publicKey,a));assert.equal(await amount(payerTokens),before);
   const b=await terms();await durable(await sdk.revoke(payer.publicKey,b.nonce));await assert.rejects(durable(await sdk.fund(payer.publicKey,b)));
 });
 await t.test('dispute split, wrong token program, wrong payout account and unauthorized resolver',async()=>{
   const a=await terms();await durable(await sdk.fund(payer.publicKey,a));await durable(await sdk.act('accept',provider.publicKey,payer.publicKey,a),[provider]);await durable(await sdk.deliver(provider.publicKey,provider.publicKey,payer.publicKey,a,sha('evidence')),[provider]);await durable(await sdk.act('dispute',payer.publicKey,payer.publicKey,a));
   await assert.rejects(durable(await sdk.payout('resolve',stranger.publicKey,payer.publicKey,a,1n),[stranger]));
   const wrongDestination=await sdk.payout('resolve',arbitrator.publicKey,payer.publicKey,a,500000n);wrongDestination.keys[6].pubkey=await ata(mint.publicKey,stranger.publicKey);await assert.rejects(durable(wrongDestination,[arbitrator]));
   const wrongProgram=await sdk.payout('resolve',arbitrator.publicKey,payer.publicKey,a,500000n);wrongProgram.keys[7].pubkey=SystemProgram.programId;await assert.rejects(durable(wrongProgram,[arbitrator]));
   await durable(await sdk.payout('resolve',arbitrator.publicKey,payer.publicKey,a,500000n),[arbitrator]);assert.equal((await sdk.inspect(payer.publicKey,a)).state,7);
 });
 await t.test('signed funding survives restart and broadcast-response loss without a second payment',async()=>{
   const a=await terms(),ix=await sdk.fund(payer.publicKey,a),op=await journal.prepare('crash',ix,payer,[payer]);
   const raw=store.db.prepare('SELECT raw FROM solana_operations WHERE id=?').get('crash').raw;
   await connection.sendRawTransaction(Buffer.from(raw));await confirm(op.signature);store.close();store=new Store(`${dir}/journal.sqlite`);journal=new SolanaJournal(store,sdk);
   assert.equal((await journal.broadcast('crash')).signature,op.signature);assert.equal((await journal.prepare('crash',ix,payer,[payer])).signature,op.signature);
   await durable(await sdk.payout('refund',payer.publicKey,payer.publicKey,a));
   const wrong=new SolanaSettlement(connection,{...manifest,genesis:'wrong'});await assert.rejects(wrong.verify());
 });
 await t.test('missed acceptance/delivery and unavailable arbitrator refund; undisputed delivery settles after review deadline',async()=>{
   async function now(){return Number(await connection.getBlockTime(await connection.getSlot('finalized')));}
   async function deadline(a){for(let i=0;i<200;i++){if(await now()>a)return;await new Promise(r=>setTimeout(r,150));}throw Error('Clock deadline did not advance');}
   async function short(){const n=await now();return terms({fundBy:n+8,acceptBy:n+12,deliverBy:n+16,reviewUntil:n+20,resolveBy:n+24});}
   const a=await short();await durable(await sdk.fund(payer.publicKey,a));await deadline(a.acceptBy);await durable(await sdk.payout('refund',stranger.publicKey,payer.publicKey,a),[stranger]);assert.equal((await sdk.inspect(payer.publicKey,a)).state,6);
   const b=await short();await durable(await sdk.fund(payer.publicKey,b));await durable(await sdk.act('accept',provider.publicKey,payer.publicKey,b),[provider]);await deadline(b.deliverBy);await durable(await sdk.payout('refund',stranger.publicKey,payer.publicKey,b),[stranger]);
   const c=await short();await durable(await sdk.fund(payer.publicKey,c));await durable(await sdk.act('accept',provider.publicKey,payer.publicKey,c),[provider]);await durable(await sdk.deliver(provider.publicKey,provider.publicKey,payer.publicKey,c,sha('evidence')),[provider]);await durable(await sdk.act('dispute',payer.publicKey,payer.publicKey,c));await deadline(c.resolveBy);await durable(await sdk.payout('refund',stranger.publicKey,payer.publicKey,c),[stranger]);
   const d=await short();await durable(await sdk.fund(payer.publicKey,d));await durable(await sdk.act('accept',provider.publicKey,payer.publicKey,d),[provider]);await durable(await sdk.deliver(provider.publicKey,provider.publicKey,payer.publicKey,d,sha('evidence')),[provider]);await deadline(d.reviewUntil);await durable(await sdk.payout('settle',provider.publicKey,payer.publicKey,d),[provider]);assert.equal((await sdk.inspect(payer.publicKey,d)).state,5);
 });
 const config=await connection.getAccountInfo(sdk.config,'finalized');assert.equal(Buffer.from(config.data).readBigUInt64LE(57),0n);
 console.log(JSON.stringify({network:'local-validator',genesis:manifest.genesis,program:manifest.program,programDataHash:manifest.programDataHash,outstanding:'0'}));
});
