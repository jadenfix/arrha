import {openSync,closeSync} from 'node:fs';
import { DatabaseSync } from 'node:sqlite';
import { keccak256, Transaction } from 'ethers';
import { json } from './protocol.mjs';
/** Signed payloads are persisted BEFORE broadcast. Protect this DB like payment authorizations. */
export class Store {
  constructor(path) {
    if(path!==':memory:')closeSync(openSync(path,'a',0o600));
    this.db=new DatabaseSync(path);
    this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;
      CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, raw TEXT NOT NULL, chain TEXT NOT NULL, sender TEXT NOT NULL, nonce INTEGER NOT NULL, status TEXT NOT NULL, block_hash TEXT, UNIQUE(chain,sender,nonce));
      CREATE TABLE IF NOT EXISTS evidence(hash TEXT PRIMARY KEY, document TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS snapshots(identity TEXT PRIMARY KEY, document TEXT NOT NULL);`);
  }
  close(){this.db.close();}
  operation(id){return this.db.prepare('SELECT * FROM operations WHERE id=?').get(id);}
  async prepare(id,wallet,request) {
    const existing=this.operation(id);
    if(existing) {
      const tx=Transaction.from(existing.raw);
      if (tx.from.toLowerCase() !== wallet.address.toLowerCase() ||
          tx.to?.toLowerCase() !== request.to?.toLowerCase() || tx.data !== (request.data ?? '0x') ||
          tx.value !== BigInt(request.value ?? 0) || tx.chainId !== BigInt(await wallet.provider.send('eth_chainId',[]))) throw Error('Operation id conflicts with existing payment');
      return existing;
    }
    // Unique(sender,nonce) fails closed if another process prepared the same nonce.
    const tx=await wallet.populateTransaction(request);
    const raw=await wallet.signTransaction(tx);
    const hash=keccak256(raw);
    this.db.prepare('INSERT INTO operations VALUES(?,?,?,?,?,?,?,NULL)').run(id,hash,raw,String(tx.chainId),wallet.address.toLowerCase(),tx.nonce,'Signed');
    return this.operation(id);
  }
  async broadcast(id,provider) {
    const op=this.operation(id); if(!op) throw Error('Unknown operation');
    if (BigInt(await provider.send('eth_chainId',[])) !== BigInt(op.chain)) throw Error('Wrong network');
    // Reconcile first; uncertain RPC responses never generate a new transaction.
    const receipt=await provider.getTransactionReceipt(op.hash);
    if(receipt) return this.reconcile(id,provider);
    try { await provider.broadcastTransaction(op.raw); }
    catch(e) { if(!await provider.getTransaction(op.hash)) throw e; }
    this.db.prepare('UPDATE operations SET status=?, block_hash=NULL WHERE id=?').run('Submitted',id);
    return this.operation(id);
  }
  async reconcile(id,provider,finality='finalized') {
    const op=this.operation(id); if(!op) throw Error('Unknown operation');
    if (BigInt(await provider.send('eth_chainId',[])) !== BigInt(op.chain)) throw Error('Wrong network');
    const receipt=await provider.getTransactionReceipt(op.hash);
    let status='Signed', blockHash=null;
    if(receipt) {
      const block=await provider.getBlock(receipt.blockNumber);
      if(!block || block.hash!==receipt.blockHash) status='Reorged';
      else { const final=await provider.getBlock(finality); if(!final) throw Error('Finality unavailable');
        status=receipt.status===0 ? 'Failed' : final.number>=receipt.blockNumber ? 'Finalized' : 'Included'; blockHash=block.hash; }
    } else if(await provider.getTransaction(op.hash)) status='Submitted';
    else if(await provider.getTransactionCount(op.sender,'latest')>op.nonce) status='ReplacedOrUnknown';
    this.db.prepare('UPDATE operations SET status=?,block_hash=? WHERE id=?').run(status,blockHash,id);
    return this.operation(id);
  }
  saveEvidence(hash,document) { this.db.prepare('INSERT INTO evidence VALUES(?,?) ON CONFLICT(hash) DO NOTHING').run(hash,json(document)); }
}
