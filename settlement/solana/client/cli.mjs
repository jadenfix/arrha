import {readFileSync,writeFileSync,mkdirSync} from 'node:fs';
import {dirname} from 'node:path';
import {Connection,Keypair,PublicKey} from '@solana/web3.js';
import {Store} from '../../src/store.mjs';
import {json} from '../../src/protocol.mjs';
import {SolanaSettlement,SolanaJournal} from './sdk.mjs';
const [command,...args]=process.argv.slice(2),read=p=>JSON.parse(readFileSync(p,'utf8'));
const bytes=value=>{if(typeof value!=='string'||!/^(0x)?[a-f0-9]{64}$/i.test(value))throw Error('Expected 32-byte hex commitment');return Buffer.from(value.replace(/^0x/i,''),'hex');};
let store;
try {
 if(command==='keygen'){const key=await Keypair.generate();mkdirSync(dirname(args[0]),{recursive:true,mode:0o700});writeFileSync(args[0],JSON.stringify(Array.from(key.secretKey)),{mode:0o600,flag:'wx'});console.log(json({address:key.publicKey.toBase58(),keyFile:args[0]}));}
 else {
  if(!process.env.ARRHA_SOLANA_MANIFEST||!process.env.ARRHA_SOLANA_RPC)throw Error('Set ARRHA_SOLANA_MANIFEST and ARRHA_SOLANA_RPC');
  const manifest=read(process.env.ARRHA_SOLANA_MANIFEST),connection=new Connection(process.env.ARRHA_SOLANA_RPC,'finalized'),sdk=new SolanaSettlement(connection,manifest);await sdk.verify();
  const terms=path=>{const t=read(path);return {...t,runId:bytes(t.runId),provider:t.provider?new PublicKey(t.provider):undefined,arbitrator:t.arbitrator?new PublicKey(t.arbitrator):undefined,receiptSigner:t.receiptSigner?new PublicKey(t.receiptSigner):undefined,workHash:t.workHash?bytes(t.workHash):undefined,evidencePolicy:t.evidencePolicy?bytes(t.evidencePolicy):undefined};};
  if(command==='state'){const t=terms(args[0]);console.log(json({finality:'finalized',program:manifest.program,...await sdk.inspect(new PublicKey(t.payer),t)}));}
  else {
   if(!process.env.ARRHA_DB)throw Error('Persistent ARRHA_DB required');mkdirSync(dirname(process.env.ARRHA_DB),{recursive:true,mode:0o700});store=new Store(process.env.ARRHA_DB);const journal=new SolanaJournal(store,sdk);
   if(command==='broadcast')console.log(json(await journal.broadcast(args[0])));
   else if(command==='operation'){
    const op=store.db.prepare('SELECT signature,last_valid FROM solana_operations WHERE id=?').get(args[0]);if(!op)throw Error('Unknown Solana operation');const status=(await connection.getSignatureStatuses([op.signature],{searchTransactionHistory:true})).value[0];const state=status?(status.err?'Failed':status.confirmationStatus):(await connection.getBlockHeight('finalized')>op.last_valid?'ExpiredOrUnknown':'SignedOrUnknown');console.log(json({signature:op.signature,lastValidBlockHeight:op.last_valid,state}));
   } else if(command==='prepare'){
    if(!process.env.ARRHA_KEY_FILE)throw Error('Set ARRHA_KEY_FILE to a protected native Solana keypair file');
    const {statSync}=await import('node:fs');if(statSync(process.env.ARRHA_KEY_FILE).mode&0o077)throw Error('Solana key file must have mode 0600');
    const actor=await Keypair.fromSecretKey(Uint8Array.from(read(process.env.ARRHA_KEY_FILE))),[id,action,file,extra]=args,t=terms(file),payer=new PublicKey(t.payer),signers=[actor];let instruction;
    if(action==='fund'){if(!actor.publicKey.equals(payer))throw Error('Funding signer must be the payer');instruction=await sdk.fund(payer,t);}
    else if(['accept','dispute'].includes(action))instruction=await sdk.act(action,actor.publicKey,payer,t);
    else if(action==='revoke')instruction=await sdk.revoke(actor.publicKey,t.nonce);
    else if(action==='deliver'){
     if(!actor.publicKey.equals(t.receiptSigner)){if(!process.env.ARRHA_RECEIPT_KEY_FILE)throw Error('Separate receipt authority must co-sign using ARRHA_RECEIPT_KEY_FILE');if(statSync(process.env.ARRHA_RECEIPT_KEY_FILE).mode&0o077)throw Error('Receipt key must have mode 0600');const receipt=await Keypair.fromSecretKey(Uint8Array.from(read(process.env.ARRHA_RECEIPT_KEY_FILE)));if(!receipt.publicKey.equals(t.receiptSigner))throw Error('Wrong receipt authority');signers.push(receipt);}
     instruction=await sdk.deliver(actor.publicKey,t.receiptSigner,payer,t,bytes(extra));
    } else if(['settle','refund','resolve'].includes(action))instruction=await sdk.payout(action,actor.publicKey,payer,t,extra ?? '0');
    else throw Error('Action: fund, accept, deliver, settle, dispute, resolve, refund or revoke');
    console.log(json({state:'Signed',...await journal.prepare(id,instruction,actor,signers)}));
   }else throw Error('keygen FILE | state TERMS | prepare ID ACTION TERMS [EVIDENCE_OR_SPLIT] | broadcast ID | operation ID');
  }
 }
}catch(e){console.error(e.shortMessage ?? e.message);process.exitCode=1;}finally{store?.close();}
