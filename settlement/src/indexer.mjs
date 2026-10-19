import { Interface } from 'ethers';
import { json, states } from './protocol.mjs';
import { verifyDeployment } from './sdk.mjs';
/** Rebuild a bounded pilot's projection atomically. No cursor can skip events after a crash/reorg.
 * Read in bounded RPC chunks, commit only after confirming the tip is still canonical.
 * Keep receipts/evidence in separate tables; these survive canonical rebuilds.
 */
export async function sync(store,provider,m,abi) {
  await verifyDeployment(provider,m,abi);
  const tip=await provider.getBlock(m.finality);
  if(!tip) throw Error('Finality unavailable');
  if(tip.number-m.deploymentBlock > (m.maxBackfillBlocks ?? 100000)) throw Error('Backfill limit reached; operator review required');
  const iface=new Interface(abi), runs={}, events=new Set();
  for(let from=m.deploymentBlock;from<=tip.number;from+=1000) {
    const logs=await provider.getLogs({address:m.escrow,fromBlock:from,toBlock:Math.min(from+999,tip.number)});
    for(const l of logs) {
      const key=`${l.blockHash}:${l.transactionHash}:${l.index}`;
      if(events.has(key)) continue; events.add(key);
      const e=iface.parseLog(l); if(!e) continue;
      if(e.name==='Funded') runs[e.args.runId]={runId:e.args.runId,payer:e.args.payer,provider:e.args.provider,amount:e.args.amount.toString(),state:'Funded',evidence:null,providerAmount:'0',payerAmount:'0',transaction:l.transactionHash};
      if(e.name==='Transition') {
        const r=runs[e.args.runId]; if(!r) throw Error('Incomplete history');
        Object.assign(r,{state:states[Number(e.args.state)],evidence:e.args.evidence,providerAmount:String(e.args.providerAmount),payerAmount:String(e.args.payerAmount),transaction:l.transactionHash});
      }
    }
  }
  if((await provider.getBlock(tip.number))?.hash!==tip.hash) throw Error('Canonical tip changed; retry sync');
  const outstanding=Object.values(runs).filter(r=>['Funded','Accepted','Delivered','Disputed'].includes(r.state)).reduce((n,r)=>n+BigInt(r.amount),0n);
  const document={availability:'available',chainId:m.chainId,escrow:m.escrow,finalityPolicy:m.finality,block:tip.number,blockHash:tip.hash,eventCount:events.size,outstanding:String(outstanding),runs:Object.values(runs),observedAt:new Date().toISOString()};
  store.db.prepare('INSERT INTO snapshots VALUES(?,?) ON CONFLICT(identity) DO UPDATE SET document=excluded.document').run(`${m.chainId}:${m.escrow}`,json(document));
  return document;
}
