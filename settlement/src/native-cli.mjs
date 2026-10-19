import {readFileSync,statSync,mkdirSync} from 'node:fs';
import {dirname} from 'node:path';
import {Store} from './store.mjs';
import {BitcoinRpc,BitcoinPayments} from './bitcoin.mjs';
import {LndRpc,LightningPayments} from './lightning.mjs';
import {json,capabilities} from './protocol.mjs';
const [chain,command,...args]=process.argv.slice(2),read=p=>JSON.parse(readFileSync(p,'utf8'));
const decimal=value=>{if(typeof value!=='string'||!/^(0|[1-9][0-9]*)$/.test(value))throw Error('Amounts and budgets require canonical integer strings');return value;};
const secret=path=>{if(!path||(statSync(path).mode&0o077)!==0)throw Error('Credential files must be private (mode 0600)');return readFileSync(path);};
let store;
try {
 if(chain==='capabilities')console.log(json(capabilities));
 else {
  if(!['bitcoin','lightning'].includes(chain))throw Error('Use bitcoin or lightning; Solana CLI is solana/client/cli.mjs');
  if(!process.env.ARRHA_NATIVE_CONFIG)throw Error('Set ARRHA_NATIVE_CONFIG to an operator-owned JSON profile');
  const config=read(process.env.ARRHA_NATIVE_CONFIG),url=new URL(config.rpcUrl);
  if(url.username||url.password||!(url.protocol==='https:'||(url.protocol==='http:'&&['localhost','127.0.0.1','[::1]'].includes(url.hostname))))throw Error('RPC requires HTTPS or loopback HTTP, with credentials in files');
  const path=process.env.ARRHA_DB ?? config.dbPath;if(!path)throw Error('Persistent ARRHA_DB or profile dbPath required');mkdirSync(dirname(path),{recursive:true,mode:0o700});store=new Store(path);
  if(chain==='bitcoin') {
   const adapter=new BitcoinPayments(store,new BitcoinRpc(config.rpcUrl,secret(config.cookieFile).toString('utf8').trim()),config.profile);
   if(command==='request'){const terms=read(args[0]);decimal(terms.amountSats);console.log(json(await adapter.request(terms)));}
   else if(command==='prepare'){const request=read(args[1]);decimal(request.amountSats);console.log(json(await adapter.preparePayment(args[0],request,{maxSats:decimal(args[2])})));}
   else if(command==='broadcast')console.log(json(await adapter.broadcast(args[0])));
   else if(command==='observe')console.log(json(await adapter.observe(read(args[0]))));
   else throw Error('bitcoin: request TERMS | prepare ID REQUEST MAX_TOTAL_SATS | broadcast ID | observe REQUEST');
  } else {
   const adapter=new LightningPayments(store,new LndRpc(config.rpcUrl,{certificate:config.certificateFile?readFileSync(config.certificateFile):undefined,macaroon:config.macaroonFile?secret(config.macaroonFile).toString('hex'):undefined}),config.profile);
   if(command==='invoice'){const terms=read(args[0]);decimal(terms.amountSats);console.log(json(await adapter.invoice(terms)));}
   else if(command==='prepare'){const request=read(args[1]);decimal(request.amountMsat);console.log(json(await adapter.prepare(args[0],request,{maxMsat:decimal(args[2]),maxFeeSat:decimal(args[3])})));}
   else if(command==='pay')console.log(json(await adapter.pay(args[0])));
   else if(command==='observe')console.log(json(await adapter.observe(read(args[0]))));
   else throw Error('lightning: invoice TERMS | prepare ID REQUEST MAX_MSAT MAX_FEE_SAT | pay ID | observe REQUEST');
  }
 }
}catch(e){console.error(e.shortMessage ?? e.message);process.exitCode=1;}finally{store?.close();}
