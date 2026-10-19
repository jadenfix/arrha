import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawn,execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {createServer} from 'node:net';
import {mkdtempSync,mkdirSync,readFileSync,rmSync,openSync,closeSync} from 'node:fs';
import {resolve} from 'node:path';
import {Store} from '../src/store.mjs';
import {BitcoinRpc} from '../src/bitcoin.mjs';
import {LndRpc,LightningPayments} from '../src/lightning.mjs';
import {id} from 'ethers';
const execute=promisify(execFile);
const free=()=>new Promise(r=>{const s=createServer();s.listen(0,'127.0.0.1',()=>{const p=s.address().port;s.close(()=>r(p));});});
const pause=()=>new Promise(r=>setTimeout(r,200));
test('two actual LND nodes and Bitcoin Core regtest: bound invoice, payment, expiry and interruption recovery',{timeout:180000},async t=>{
 if(!process.env.ARRHA_BITCOIND||!process.env.ARRHA_LND_BIN)throw Error('Set ARRHA_BITCOIND and ARRHA_LND_BIN to verified executables');
 const dir=mkdtempSync(resolve(import.meta.dirname,'../.local-test-lightning-')),children=[],logs=[];let store;
 t.after(async()=>{store?.close();for(const c of children.reverse()){c.kill('SIGTERM');await new Promise(r=>{if(c.exitCode!==null)r();else c.once('exit',r);});}for(const fd of logs)closeSync(fd);if(process.env.ARRHA_KEEP_TEST_DATA!=='1')rmSync(dir,{recursive:true,force:true});});
 const btcPort=await free(),rawBlocks=await free(),rawTx=await free();mkdirSync(`${dir}/bitcoin`);
 const fd=openSync(`${dir}/bitcoin.log`,'w');logs.push(fd);
 children.push(spawn(process.env.ARRHA_BITCOIND,[`-datadir=${dir}/bitcoin`,'-regtest','-server','-listen=0','-dnsseed=0','-connect=0','-fallbackfee=0.00002000',`-rpcport=${btcPort}`,'-rpcuser=arrha-local','-rpcpassword=local-regtest-only',`-zmqpubrawblock=tcp://127.0.0.1:${rawBlocks}`,`-zmqpubrawtx=tcp://127.0.0.1:${rawTx}`],{stdio:['ignore',fd,fd]}));
 const btc=new BitcoinRpc(`http://127.0.0.1:${btcPort}`,'arrha-local:local-regtest-only');
 for(let i=0;i<200;i++){try{await btc.call('getblockchaininfo');break;}catch{await pause();}}
 await btc.call('createwallet',['miner']);const miner=new BitcoinRpc(`${btc.url}/wallet/miner`,btc.cookie),address=await miner.call('getnewaddress');await btc.call('generatetoaddress',[101,address]);
 async function node(name){
   const base=`${dir}/${name}`;mkdirSync(base);const peerPort=await free(),grpcPort=await free(),restPort=await free();const log=openSync(`${base}/process.log`,'w');logs.push(log);
   const child=spawn(`${process.env.ARRHA_LND_BIN}/lnd`,[`--lnddir=${base}`,'--noseedbackup','--no-macaroons',`--listen=127.0.0.1:${peerPort}`,`--rpclisten=127.0.0.1:${grpcPort}`,`--restlisten=127.0.0.1:${restPort}`,'--bitcoin.active','--bitcoin.regtest','--bitcoin.node=bitcoind',`--bitcoind.rpchost=127.0.0.1:${btcPort}`,'--bitcoind.rpcuser=arrha-local','--bitcoind.rpcpass=local-regtest-only',`--bitcoind.zmqpubrawblock=tcp://127.0.0.1:${rawBlocks}`,`--bitcoind.zmqpubrawtx=tcp://127.0.0.1:${rawTx}`],{stdio:['ignore',log,log]});children.push(child);
   let rpc,info;for(let i=0;i<200;i++){if(child.exitCode!==null)throw Error(readFileSync(`${base}/process.log`,'utf8'));try{rpc=new LndRpc(`https://127.0.0.1:${restPort}`,{certificate:readFileSync(`${base}/tls.cert`)});info=await rpc.call('GET','/v1/getinfo');if(info.synced_to_chain)break;}catch{}await pause();}
   if(!info?.synced_to_chain)throw Error(`LND not synced: ${readFileSync(`${base}/process.log`,'utf8').slice(-5000)}`);
   const cli=async(...args)=>JSON.parse((await execute(`${process.env.ARRHA_LND_BIN}/lncli`,[`--lnddir=${base}`,'--network=regtest','--no-macaroons',`--rpcserver=127.0.0.1:${grpcPort}`,...args],{timeout:20000,maxBuffer:100000})).stdout);
   return {rpc,cli,peerPort,nodeId:info.identity_pubkey};
 }
 const payerNode=await node('payer'),recipientNode=await node('recipient');console.log('Both local LND nodes synchronized');
 const funding=(await payerNode.rpc.call('GET','/v1/newaddress?type=0')).address;await miner.call('sendtoaddress',[funding,'1.00000000']);await btc.call('generatetoaddress',[6,address]);
 await payerNode.cli('connect',`${recipientNode.nodeId}@127.0.0.1:${recipientNode.peerPort}`);
 await payerNode.cli('openchannel',`--node_key=${recipientNode.nodeId}`,'--local_amt=1000000','--sat_per_vbyte=2');await btc.call('generatetoaddress',[6,address]);
 for(let i=0;i<100;i++){const c=await payerNode.rpc.call('GET','/v1/channels');if(c.channels?.some(x=>x.active))break;await pause();}
 for(const n of [payerNode,recipientNode]){let info;for(let i=0;i<200;i++){info=await n.rpc.call('GET','/v1/getinfo');if(info.synced_to_chain && Number(info.block_height)>=113)break;await pause();}assert(info.synced_to_chain,JSON.stringify({chains:info.chains,block:info.block_height,synced:info.synced_to_chain}));}
 let routes;for(let i=0;i<100;i++){try{routes=await payerNode.rpc.call('GET',`/v1/graph/routes/${recipientNode.nodeId}/1234`);if(routes.routes?.length)break;}catch{}await pause();}assert(routes?.routes?.length,'Direct channel route did not become usable');
 store=new Store(`${dir}/lightning.sqlite`);let payer=new LightningPayments(store,payerNode.rpc,{network:'regtest',nodeId:payerNode.nodeId}),recipient=new LightningPayments(store,recipientNode.rpc,{network:'regtest',nodeId:recipientNode.nodeId});
 const request=await recipient.invoice({runId:'lightning-work',agreementHash:id('work terms'),amountSats:'1234'});assert.equal((await recipient.observe(request)).state,'Pending');
 await payer.prepare('one-payment',request,{maxMsat:'1234000',maxFeeSat:'10'});
 await assert.rejects(payer.prepare('duplicate-id',request,{maxMsat:'1234000',maxFeeSat:'10'}));
 await assert.rejects(payer.prepare('changed-amount',{...request,amountMsat:'1235000'},{maxMsat:'9999999',maxFeeSat:'10'}));
 // Execute the RPC then interrupt before recording the terminal result in the journal.
 store.db.prepare('UPDATE ln_payments SET status=? WHERE id=?').run('Attempted','one-payment');
 const response=await payerNode.rpc.call('POST','/v2/router/send',{payment_request:request.bolt11,fee_limit_sat:'10',timeout_seconds:10,no_inflight_updates:true,max_parts:1});assert.equal(response.status,'SUCCEEDED',JSON.stringify({status:response.status,failureReason:response.failure_reason}));
 store.close();store=new Store(`${dir}/lightning.sqlite`);payer=new LightningPayments(store,payerNode.rpc,{network:'regtest',nodeId:payerNode.nodeId});recipient=new LightningPayments(store,recipientNode.rpc,{network:'regtest',nodeId:recipientNode.nodeId});
 await assert.rejects(recipient.observe({...request,amountMsat:'1'}));
 const recovered=await payer.pay('one-payment');assert.equal(recovered.state,'Paid');assert.equal((await recipient.observe(request)).amountPaidMsat,'1234000');assert.equal((await payer.pay('one-payment')).state,'Paid');
 const short=await recipient.invoice({runId:'expires',agreementHash:id('expired work'),amountSats:'1',expirySeconds:1});await new Promise(r=>setTimeout(r,1500));await assert.rejects(payer.prepare('expired',short,{maxMsat:'1000',maxFeeSat:'1'}));assert(['Expired','Canceled'].includes((await recipient.observe(short)).state));assert.equal((await recipient.observe(short)).amountPaidMsat,'0');
 const payments=await payerNode.rpc.call('GET','/v1/payments?include_incomplete=true');assert.equal(payments.payments.filter(p=>p.status==='SUCCEEDED').length,1);
 console.log(JSON.stringify({network:'regtest',paymentHash:recovered.paymentHash,amountMsat:'1234000',successfulPayments:1,duplicatePayments:0,preimageVerified:true}));
});
