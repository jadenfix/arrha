import {request as httpsRequest} from 'node:https';
import {createHash,randomBytes} from 'node:crypto';
import {json} from './protocol.mjs';
/** LND REST with certificate verification and optional macaroon. No insecure TLS fallback. */
export class LndRpc {
 constructor(url,{certificate,macaroon}={}){this.url=url;this.certificate=certificate;this.macaroon=macaroon;}
 call(method,path,body){return new Promise((resolve,reject)=>{
   const req=httpsRequest(new URL(path,this.url),{method,ca:this.certificate,headers:{'Content-Type':'application/json',...(this.macaroon?{'Grpc-Metadata-macaroon':this.macaroon}:{})}},res=>{
     let data='';res.on('data',chunk=>{data+=chunk;if(data.length>2*1024*1024)req.destroy(Error('LND response too large'));});
     res.on('end',()=>{try{
       const messages=data.trim().split('\n').filter(Boolean).map(x=>JSON.parse(x));
       const last=messages.at(-1);const error=last?.error ?? (last?.code?last:null);
       if(error||res.statusCode>=400)throw Object.assign(Error(error?.message ?? `LND HTTP ${res.statusCode}`),{rpcCode:Number(error?.code ?? last?.code),httpStatus:res.statusCode});
       if(!last)throw Error('Empty LND response');resolve(last.result ?? last);
     }catch(e){reject(e);}});
   });
   req.setTimeout(15000,()=>req.destroy(Error('LND response uncertain: timeout')));req.on('error',reject);if(body)req.write(JSON.stringify(body));req.end();
 });}
}
const sha=b=>createHash('sha256').update(b).digest();
export class LightningPayments {
 constructor(store,rpc,{network,nodeId}) {
   if(!['regtest','signet','testnet','mainnet'].includes(network)||!/^0[23][0-9a-f]{64}$/i.test(nodeId))throw Error('Explicit Lightning network/node identity required');
   this.db=store.db;this.rpc=rpc;this.profile={network,nodeId};
   this.db.exec('CREATE TABLE IF NOT EXISTS ln_invoices(id TEXT PRIMARY KEY, document TEXT NOT NULL); CREATE TABLE IF NOT EXISTS ln_payments(id TEXT UNIQUE NOT NULL, payment_hash TEXT PRIMARY KEY, document TEXT NOT NULL, status TEXT NOT NULL);');
 }
 async verify(){const info=await this.rpc.call('GET','/v1/getinfo');if(info.identity_pubkey!==this.profile.nodeId||!info.chains.some(c=>c.chain==='bitcoin'&&c.network===this.profile.network)||!info.synced_to_chain)throw Error('Wrong or unsynchronized Lightning node');}
 async invoice({runId,agreementHash,amountSats,expirySeconds=300}){
   await this.verify();if(!runId||!/^0x[0-9a-f]{64}$/i.test(agreementHash)||BigInt(amountSats)<=0n||!Number.isSafeInteger(expirySeconds)||expirySeconds<1||expirySeconds>86400)throw Error('Invalid Lightning request');
   const id=`${this.profile.network}:${this.profile.nodeId}:${runId}`;
   let saved=this.db.prepare('SELECT document FROM ln_invoices WHERE id=?').get(id);
   let record=saved?JSON.parse(saved.document):null;
   if(record&&(record.agreementHash!==agreementHash||record.amountSats!==String(amountSats)||record.expirySeconds!==expirySeconds))throw Error('Conflicting invoice');
   if(!record){const preimage=randomBytes(32);record={runId,agreementHash,amountSats:String(amountSats),expirySeconds,preimage:preimage.toString('hex'),paymentHash:sha(preimage).toString('hex')};this.db.prepare('INSERT INTO ln_invoices VALUES(?,?)').run(id,json(record));}
   if(record.request)return record.request;
   let invoice;
   try{invoice=await this.rpc.call('GET',`/v1/invoice/${record.paymentHash}`);}
   catch(e){if(e.rpcCode!==5)throw e;invoice=await this.rpc.call('POST','/v1/invoices',{r_preimage:Buffer.from(record.preimage,'hex').toString('base64'),value:record.amountSats,expiry:String(expirySeconds),description_hash:Buffer.from(agreementHash.slice(2),'hex').toString('base64')});}
   const bolt11=invoice.payment_request;
   const decoded=await this.rpc.call('GET',`/v1/payreq/${encodeURIComponent(bolt11)}`);
   if(decoded.payment_hash!==record.paymentHash||decoded.description_hash!==agreementHash.slice(2)||BigInt(decoded.num_satoshis)!==BigInt(amountSats))throw Error('Invoice binding mismatch');
   record.request={version:1,capability:'lightning-payment',network:this.profile.network,payee:this.profile.nodeId,runId,agreementHash,paymentHash:record.paymentHash,amountMsat:String(BigInt(amountSats)*1000n),expiresAt:Number(decoded.timestamp)+Number(decoded.expiry),bolt11};
   this.db.prepare('UPDATE ln_invoices SET document=? WHERE id=?').run(json(record),id);return record.request;
 }
 async observe(request){await this.verify();if(request.network!==this.profile.network||request.payee!==this.profile.nodeId)throw Error('Wrong invoice destination/network');const retained=this.db.prepare('SELECT document FROM ln_invoices WHERE id=?').get(`${this.profile.network}:${this.profile.nodeId}:${request.runId}`);if(!retained||json(JSON.parse(retained.document).request)!==json(request))throw Error('Unknown or modified retained Lightning request');const invoice=await this.rpc.call('GET',`/v1/invoice/${request.paymentHash}`);return {paymentHash:request.paymentHash,state:invoice.state==='SETTLED'?'Paid':invoice.state==='CANCELED'?'Canceled':Math.floor(Date.now()/1000)>=request.expiresAt?'Expired':'Pending',amountPaidMsat:invoice.amt_paid_msat ?? '0'};}
 async prepare(id,request,{maxMsat,maxFeeSat}){
   await this.verify();if(request.network!==this.profile.network||BigInt(request.amountMsat)>BigInt(maxMsat)||BigInt(maxFeeSat)<0n)throw Error('Wrong network or unauthorized budget');
   const decoded=await this.rpc.call('GET',`/v1/payreq/${encodeURIComponent(request.bolt11)}`);
   if(decoded.destination!==request.payee||decoded.payment_hash!==request.paymentHash||decoded.description_hash!==request.agreementHash.slice(2)||BigInt(decoded.num_msat)!==BigInt(request.amountMsat)||Number(decoded.timestamp)+Number(decoded.expiry)!==request.expiresAt)throw Error('Invoice terms modified');
   const existing=this.db.prepare('SELECT * FROM ln_payments WHERE id=? OR payment_hash=?').get(id,request.paymentHash);
   const document=json({request,maxMsat:String(maxMsat),maxFeeSat:String(maxFeeSat)});
   if(existing){if(existing.id!==id||existing.document!==document)throw Error('Conflicting or duplicate Lightning operation');return {id,paymentHash:request.paymentHash,state:existing.status};}
   if(Math.floor(Date.now()/1000)>=request.expiresAt)throw Error('Expired invoice');
   this.db.prepare('INSERT INTO ln_payments VALUES(?,?,?,?)').run(id,request.paymentHash,document,'Prepared');return {id,paymentHash:request.paymentHash,state:'Prepared'};
 }
 async track(hash){
   // Use paginated canonical node history: REST path encoding of arbitrary hash bytes
   // differs between gRPC gateways; no uncertain response can trigger a second payment.
   let offset='0';
   for(let page=0;page<100;page++){
     const history=await this.rpc.call('GET',`/v1/payments?include_incomplete=true&index_offset=${offset}&max_payments=100&reversed=false`);
     const found=history.payments?.find(p=>p.payment_hash===hash);if(found)return found;
     if(!history.payments?.length)throw Object.assign(Error('Payment not found'),{rpcCode:5});
     const next=String(history.last_index_offset);if(BigInt(next)<=BigInt(offset))throw Error('Invalid payment history cursor');offset=next;
   }
   throw Error('Payment history recovery bound exceeded; operator review required');
 }

 async pay(id){
   await this.verify();const op=this.db.prepare('SELECT * FROM ln_payments WHERE id=?').get(id);if(!op)throw Error('Unknown Lightning operation');
   const {request,maxFeeSat}=JSON.parse(op.document);let payment;
   try{payment=await this.track(op.payment_hash);}catch(e){if(e.rpcCode!==5)throw e;if(op.status!=='Prepared')return {paymentHash:op.payment_hash,state:'Unknown'};}
   if(!payment){
     if(Math.floor(Date.now()/1000)>=request.expiresAt)throw Error('Expired invoice');
     this.db.prepare('UPDATE ln_payments SET status=? WHERE id=?').run('Attempted',id);
     // If response is lost, resume with track; never create another payment or reattempt blindly.
     payment=await this.rpc.call('POST','/v2/router/send',{payment_request:request.bolt11,fee_limit_sat:maxFeeSat,timeout_seconds:10,no_inflight_updates:true,max_parts:1});
   }
   if(payment.status==='SUCCEEDED'&&sha(Buffer.from(payment.payment_preimage,'hex')).toString('hex')!==op.payment_hash)throw Error('Invalid payment preimage');
   const state=payment.status==='SUCCEEDED'?'Paid':payment.status==='FAILED'?'Failed':'InFlight';
   this.db.prepare('UPDATE ln_payments SET status=? WHERE id=?').run(state,id);
   return {paymentHash:op.payment_hash,state,feeMsat:payment.fee_msat,preimage:state==='Paid'?payment.payment_preimage:undefined};
 }
}
