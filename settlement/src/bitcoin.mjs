import {createHash} from 'node:crypto';
import {json} from './protocol.mjs';
export function btcToSats(value) {
  if(typeof value!=='string'||!/^\d+(\.\d{1,8})?$/.test(value))throw Error('BTC amount must be an exact decimal string');
  const [whole,fraction='']=value.split('.');const sats=BigInt(whole)*100000000n+BigInt(fraction.padEnd(8,'0'));
  if(sats>21000000n*100000000n)throw Error('BTC amount outside monetary range');return sats;
}
export function satsToBtc(sats) {sats=BigInt(sats);if(sats<0n||sats>2100000000000000n)throw Error('Invalid satoshi amount');return `${sats/100000000n}.${String(sats%100000000n).padStart(8,'0')}`;}
export class BitcoinRpc {
  constructor(url,cookie){this.url=url;this.cookie=cookie;}
  async call(method,params=[]) {
    let r;
    try{r=await fetch(this.url,{method:'POST',signal:AbortSignal.timeout(15000),headers:{Authorization:`Basic ${Buffer.from(this.cookie).toString('base64')}`,'Content-Type':'application/json'},body:JSON.stringify({jsonrpc:'2.0',id:1,method,params})});}
    catch(error){throw Error(`${method}: RPC response uncertain (${error.message})`,{cause:error});}
    // Preserve original monetary decimal lexemes. Never convert financial amounts through floats.
    const result=JSON.parse(await r.text(),(key,value,context)=>typeof value==='number'?context.source:value);
    if(result.error)throw Error(`${method}: ${result.error.message}`);if(!r.ok)throw Error(`Bitcoin RPC HTTP ${r.status}`);return result.result;
  }
}
/** Native BTC payment capability, not escrow. A caller-owned Bitcoin Core wallet signs.
 * Requires an honest fully synchronized node and wallet history; balances are never inferred from a receipt signature.
 */
export class BitcoinPayments {
  constructor(store,rpc,{genesis,network,confirmations=6}) {
    if(!genesis||!['regtest','signet','test','main'].includes(network)||!Number.isSafeInteger(confirmations)||confirmations<1)throw Error('Explicit Bitcoin network/finality required');
    this.store=store;this.rpc=rpc;this.profile={genesis,network,confirmations};
    store.db.exec('CREATE TABLE IF NOT EXISTS btc_requests(id TEXT PRIMARY KEY, document TEXT NOT NULL); CREATE TABLE IF NOT EXISTS btc_operations(id TEXT PRIMARY KEY, request_hash TEXT UNIQUE NOT NULL, txid TEXT NOT NULL, raw TEXT NOT NULL);');
  }
  async verify(){if(await this.rpc.call('getblockhash',[0])!==this.profile.genesis)throw Error('Wrong Bitcoin network');const info=await this.rpc.call('getblockchaininfo');if(info.chain!==this.profile.network||info.initialblockdownload)throw Error('Bitcoin node unavailable or unsynchronized');}
  async request({runId,amountSats,expiresAt,agreementHash}) {
    await this.verify();const amount=BigInt(amountSats);
    if(amount<=0n||amount>2100000000000000n||!Number.isSafeInteger(expiresAt)||expiresAt<=Math.floor(Date.now()/1000)||!/^0x[0-9a-f]{64}$/i.test(agreementHash)||!runId)throw Error('Invalid payment request');
    const key=`${this.profile.genesis}:${runId}`,stored=this.store.db.prepare('SELECT document FROM btc_requests WHERE id=?').get(key);
    if(stored){const old=JSON.parse(stored.document);if(old.amountSats!==String(amount)||old.agreementHash!==agreementHash||old.expiresAt!==expiresAt)throw Error('Conflicting payment request');return old;}
    const label=`arrha:${createHash('sha256').update(key).digest('hex')}`;
    let addresses;try{addresses=await this.rpc.call('getaddressesbylabel',[label]);}catch(e){if(!e.message.includes('No addresses with label'))throw e;}
    const address=addresses?Object.keys(addresses)[0]:await this.rpc.call('getnewaddress',[label,'bech32']);
    const request={version:1,capability:'bitcoin-payment',...this.profile,runId,agreementHash,address,amountSats:String(amount),expiresAt};
    const encoded=json(request),hash=createHash('sha256').update(encoded).digest('hex');
    this.store.db.prepare('INSERT INTO btc_requests VALUES(?,?)').run(key,json({...request,requestHash:hash}));return {...request,requestHash:hash};
  }
  async observe(request) {
    await this.verify();if(request.genesis!==this.profile.genesis||request.network!==this.profile.network)throw Error('Wrong payment network');
    const retained=this.store.db.prepare('SELECT document FROM btc_requests WHERE id=?').get(`${this.profile.genesis}:${request.runId}`);
    if(!retained||json(JSON.parse(retained.document))!==json(request))throw Error('Unknown or modified retained Bitcoin request');
    const before=await this.rpc.call('getbestblockhash');
    const received=await this.rpc.call('listreceivedbyaddress',[0,true,false,request.address]);
    const txids=[...new Set(received.flatMap(r=>r.txids))],payments=[];let confirmed=0n,late=0n;
    for(const txid of txids){
      const tx=await this.rpc.call('gettransaction',[txid]);const conf=Number(tx.confirmations);
      if(conf<1)continue;
      const header=await this.rpc.call('getblockheader',[tx.blockhash]);if(Number(header.confirmations)<1)continue;
      const sats=tx.details.filter(d=>d.category==='receive'&&d.address===request.address).reduce((n,d)=>n+btcToSats(d.amount),0n);
      const qualified=conf>=this.profile.confirmations;
      if(qualified){if(Number(tx.blocktime)>request.expiresAt)late+=sats;else confirmed+=sats;}
      payments.push({txid,confirmations:conf,amountSats:String(sats),blockHash:tx.blockhash,qualified});
    }
    if(await this.rpc.call('getbestblockhash')!==before)throw Error('Bitcoin tip changed during observation; retry');
    return {capability:'bitcoin-payment',state:confirmed>=BigInt(request.amountSats)?'Paid':late>0n?'LatePayment':Math.floor(Date.now()/1000)>request.expiresAt?'Expired':'Pending',confirmedSats:String(confirmed),lateSats:String(late),payments,blockHash:before,requiredConfirmations:this.profile.confirmations};
  }
  async preparePayment(operationId,request,{maxSats}) {
    await this.verify();const copy={...request};delete copy.requestHash;
    if(createHash('sha256').update(json(copy)).digest('hex')!==request.requestHash)throw Error('Payment request modified');
    if(request.genesis!==this.profile.genesis||request.network!==this.profile.network)throw Error('Wrong payment network');
    const existing=this.store.db.prepare('SELECT * FROM btc_operations WHERE id=?').get(operationId);
    if(existing){if(existing.request_hash!==request.requestHash)throw Error('Conflicting payment operation');return {txid:existing.txid,state:'Signed'};}
    // One request hash can only have one local payment operation, across retries/operation IDs.
    if(this.store.db.prepare('SELECT id FROM btc_operations WHERE request_hash=?').get(request.requestHash))throw Error('Request already has a prepared payment');
    if(Math.floor(Date.now()/1000)>request.expiresAt||BigInt(request.amountSats)>BigInt(maxSats))throw Error('Expired or over-budget payment');
    // Disable automatic wallet broadcasting: fund a PSBT, sign, then persist before sendrawtransaction.
    const funded=await this.rpc.call('walletcreatefundedpsbt',[[],[{[request.address]:satsToBtc(request.amountSats)}],0,{lockUnspents:false,fee_rate:2},true]);
    if(btcToSats(funded.fee)+BigInt(request.amountSats)>BigInt(maxSats))throw Error('Fee exceeds total authorized budget');
    const signed=await this.rpc.call('walletprocesspsbt',[funded.psbt]);
    const final=await this.rpc.call('finalizepsbt',[signed.psbt]);if(!final.complete)throw Error('Wallet signature incomplete');
    const decoded=await this.rpc.call('decoderawtransaction',[final.hex]);
    this.store.db.prepare('INSERT INTO btc_operations VALUES(?,?,?,?)').run(operationId,request.requestHash,decoded.txid,final.hex);
    return {txid:decoded.txid,state:'Signed'};
  }
  async broadcast(operationId) {
    await this.verify();const op=this.store.db.prepare('SELECT * FROM btc_operations WHERE id=?').get(operationId);if(!op)throw Error('Unknown Bitcoin payment');
    try{const tx=await this.rpc.call('gettransaction',[op.txid]);if(Number(tx.confirmations)>=0)return {txid:op.txid,state:Number(tx.confirmations)>0?'Included':'Submitted'};throw Error('Conflicted Bitcoin transaction');}
    catch(e){if(!e.message.includes('Invalid or non-wallet transaction id'))throw e;}
    const txid=await this.rpc.call('sendrawtransaction',[op.raw]);if(txid!==op.txid)throw Error('Bitcoin transaction identity mismatch');return {txid,state:'Submitted'};
  }
}
