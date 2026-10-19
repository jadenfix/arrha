import { readFileSync,writeFileSync, mkdirSync } from 'node:fs';
import { dirname } from 'node:path';
import { JsonRpcProvider, Wallet } from 'ethers';
import { Store } from './store.mjs';
import { EvmSettlement } from './sdk.mjs';
import { json } from './protocol.mjs';
import { sync } from './indexer.mjs';
const [command,...args]=process.argv.slice(2);
const read=p=>JSON.parse(readFileSync(p,'utf8'));
const save=(p,v)=>{mkdirSync(dirname(p),{recursive:true});writeFileSync(p,json(v),{mode:0o600,flag:'wx'});};
let db,provider;
try {
  if(command==='keygen') { const w=Wallet.createRandom(); save(args[0],{privateKey:w.privateKey}); console.log(json({address:w.address,keyFile:args[0]})); }
  else {
    if(!process.env.ARRHA_RPC_URL) throw Error('Set ARRHA_RPC_URL');
    provider=new JsonRpcProvider(process.env.ARRHA_RPC_URL,undefined,{cacheTimeout:-1}); provider.pollingInterval=100;
    const wallet=process.env.ARRHA_KEY_FILE ? new Wallet(read(process.env.ARRHA_KEY_FILE).privateKey,provider) : null;
    const requireWallet=()=>{if(!wallet) throw Error('Set ARRHA_KEY_FILE (never paste keys in command arguments)');return wallet;};
    if(['deploy-test','deploy-escrow'].includes(command)) {
      const {compile}=await import('./compile.mjs'); const {deployTest,deployEscrow}=await import('./deploy.mjs');
      const w=requireWallet();
      if(Number(await provider.send('eth_chainId',[]))!==31337 && !process.env.ARRHA_TESTNET_DEPLOY_BUDGET_WEI) throw Error('Set ARRHA_TESTNET_DEPLOY_BUDGET_WEI to authorize a testnet gas budget');
      // Check available balance; budget wallet must be dedicated and contain at most the approved budget.
      const balance=await provider.getBalance(w.address);
      if(process.env.ARRHA_TESTNET_DEPLOY_BUDGET_WEI && balance>BigInt(process.env.ARRHA_TESTNET_DEPLOY_BUDGET_WEI)) throw Error('Use a dedicated wallet funded within the approved gas budget');
      const artifacts=compile();const manifest=command==='deploy-test'?await deployTest(w,artifacts):await deployEscrow(w,artifacts.ArrhaEscrow,read(args[0]));save(args[command==='deploy-test'?0:1],manifest);console.log(json(manifest));
    } else {
      if(!process.env.ARRHA_MANIFEST) throw Error('Set ARRHA_MANIFEST');
      const m=read(process.env.ARRHA_MANIFEST), abi=read(new URL('../artifacts/ArrhaEscrow.json',import.meta.url)).abi;
      const sdk=new EvmSettlement(m,abi,wallet ?? provider); await sdk.verify();
      if(command==='sign') console.log(json({terms:read(args[0]),signature:await sdk.signAgreement(read(args[0]))}));
      else if(command==='receipt') console.log(json({signature:await sdk.signReceipt(read(args[0]),args[1])}));
      else {
        db=new Store(process.env.ARRHA_DB ?? '.local/settlement.sqlite');
        if(command==='prepare') { const req=read(args[1]); const op=await db.prepare(args[0],requireWallet(),req); console.log(json({id:op.id,hash:op.hash,status:op.status})); }
        else if(command==='broadcast') { const op=await db.broadcast(args[0],provider); console.log(json({id:op.id,hash:op.hash,status:op.status})); }
        else if(command==='operation') { const op=await db.reconcile(args[0],provider,m.finality); console.log(json({id:op.id,hash:op.hash,status:op.status})); }
        else if(command==='state') console.log(json(await sync(db,provider,m,abi)));
        else throw Error('Commands: keygen FILE | deploy-test MANIFEST | sign TERMS | receipt TERMS HASH | prepare ID REQUEST | broadcast ID | operation ID | state');
      }
    }
  }
} catch(e) { console.error(e.shortMessage ?? e.message); process.exitCode=1; }
finally {db?.close(); provider?.destroy();}
