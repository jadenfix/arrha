import {createHash} from 'node:crypto';
import {PublicKey,TransactionInstruction,Transaction,SystemProgram} from '@solana/web3.js';
export const PROGRAM_ID=new PublicKey('2iXtA8oeZqUU5pofxK971TCEvFGfems2AcDRaZHKD2pQ');
export const TOKEN_PROGRAM=new PublicKey('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA');
export const ASSOCIATED_PROGRAM=new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL');
export const LOADER=new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111');
const hash=b=>createHash('sha256').update(b).digest();
const discriminator=name=>hash(`global:${name}`).subarray(0,8);
export const u64=n=>{if(typeof n==='number'&&!Number.isSafeInteger(n))throw Error('Unsafe integer amount');if(typeof n==='string'&&!/^(0|[1-9][0-9]*)$/.test(n))throw Error('Noncanonical integer');const b=Buffer.alloc(8);b.writeBigUInt64LE(BigInt(n));return b;};
const fixed32=x=>{const b=Buffer.from(x);if(b.length!==32)throw Error('Expected 32-byte commitment');return b;};
const i64=n=>{const b=Buffer.alloc(8);b.writeBigInt64LE(BigInt(n));return b;};
const pk=x=>new PublicKey(x);
const meta=(pubkey,isSigner=false,isWritable=false)=>({pubkey:pk(pubkey),isSigner,isWritable});
export const ata=async(mint,authority)=>(await PublicKey.findProgramAddress([pk(authority).toBytes(),TOKEN_PROGRAM.toBytes(),pk(mint).toBytes()],ASSOCIATED_PROGRAM))[0];
export const termsBytes=t=>Buffer.concat([fixed32(t.runId),pk(t.provider).toBytes(),pk(t.arbitrator).toBytes(),pk(t.receiptSigner).toBytes(),u64(t.amount),fixed32(t.workHash),fixed32(t.evidencePolicy),...['fundBy','acceptBy','deliverBy','reviewUntil','resolveBy'].map(k=>i64(t[k])),u64(t.nonce)]);
/** Native SPL-token escrow adapter. Native transaction signatures bind program, accounts,
 * all terms and evidence. Only legacy Token Program assets supported; no Token-2022 extensions.
 */
export class SolanaSettlement {
 constructor(connection,manifest){this.connection=connection;this.manifest=manifest;this.program=pk(manifest.program);this.mint=pk(manifest.mint);this.config=null;}
 async run(payer,runId){return (await PublicKey.findProgramAddress([Buffer.from('run'),pk(payer).toBytes(),Buffer.from(runId)],this.program))[0];}
 async nonce(payer,nonce){return (await PublicKey.findProgramAddress([Buffer.from('nonce'),pk(payer).toBytes(),u64(nonce)],this.program))[0];}
 async verify(){this.config ??= (await PublicKey.findProgramAddress([Buffer.from('config')],this.program))[0];
   if(await this.connection.getGenesisHash()!==this.manifest.genesis)throw Error('Wrong Solana network');
   const info=await this.connection.getAccountInfo(this.program,'finalized');if(!info?.executable||!info.owner.equals(LOADER))throw Error('Wrong Solana deployment');
   const dataAddress=(await PublicKey.findProgramAddress([this.program.toBytes()],LOADER))[0];const data=await this.connection.getAccountInfo(dataAddress,'finalized');
   if(!data||hash(data.data.subarray(45)).toString('hex')!==this.manifest.programDataHash)throw Error('Solana program bytecode changed');
   const config=await this.connection.getAccountInfo(this.config,'finalized');if(config&&(!config.owner.equals(this.program)||config.data.length!==65||!new PublicKey(config.data.subarray(8,40)).equals(this.mint)||config.data[40]!==this.manifest.decimals))throw Error('Wrong Solana asset profile');
   if(config){const data=Buffer.from(config.data);if((this.manifest.maxPerRun!==undefined&&data.readBigUInt64LE(41)!==BigInt(this.manifest.maxPerRun))||(this.manifest.maxOutstanding!==undefined&&data.readBigUInt64LE(49)!==BigInt(this.manifest.maxOutstanding)))throw Error('Wrong Solana caps');}
 }
 instruction(name,keys,data=Buffer.alloc(0)){return new TransactionInstruction({programId:this.program,keys,data:Buffer.concat([discriminator(name),data])});}
 async initialize(authority,maxPerRun,maxOutstanding){await this.verify();return this.instruction('initialize',[meta(authority,true,true),meta(this.config,false,true),meta(this.mint),meta(this.program),meta((await PublicKey.findProgramAddress([this.program.toBytes()],LOADER))[0]),meta(SystemProgram.programId)],Buffer.concat([u64(maxPerRun),u64(maxOutstanding)]));}
 async fund(payer,t){await this.verify();const run=await this.run(payer,t.runId);return this.instruction('fund',[meta(payer,true,true),meta(this.config,false,true),meta(run,false,true),meta(await this.nonce(payer,t.nonce),false,true),meta(this.mint),meta(await ata(this.mint,payer),false,true),meta(await ata(this.mint,run),false,true),meta(TOKEN_PROGRAM),meta(ASSOCIATED_PROGRAM),meta(SystemProgram.programId)],termsBytes(t));}
 async revoke(payer,nonce){await this.verify();return this.instruction('revoke',[meta(payer,true,true),meta(await this.nonce(payer,nonce),false,true),meta(SystemProgram.programId)],u64(nonce));}
 async act(name,actor,payer,t){await this.verify();if(!['accept','dispute'].includes(name))throw Error('Unsupported action');return this.instruction(name,[meta(actor,true),meta(await this.run(payer,t.runId),false,true)]);}
 async deliver(provider,receiptSigner,payer,t,evidence){await this.verify();return this.instruction('deliver',[meta(provider,true),meta(receiptSigner,true),meta(await this.run(payer,t.runId),false,true)],fixed32(evidence));}
 async payout(name,actor,payer,t,amount=0){await this.verify();if(!['settle','refund','resolve'].includes(name))throw Error('Unsupported action');const run=await this.run(payer,t.runId);return this.instruction(name,[meta(actor,true),meta(this.config,false,true),meta(run,false,true),meta(this.mint),meta(await ata(this.mint,run),false,true),meta(await ata(this.mint,payer),false,true),meta(await ata(this.mint,t.provider),false,true),meta(TOKEN_PROGRAM)],name==='resolve'?u64(amount):Buffer.alloc(0));}
 async inspect(payer,t){await this.verify();const a=await this.connection.getAccountInfo(await this.run(payer,t.runId),'finalized');if(!a)return {state:0};if(!a.owner.equals(this.program)||a.data.length!==322)throw Error('Invalid run account');return {state:a.data[288],evidence:Buffer.from(a.data.subarray(289,321)).toString('hex')};}
}
function base58(bytes){const alphabet='123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';let n=BigInt(`0x${Buffer.from(bytes).toString('hex')}`),s='';while(n){s=alphabet[Number(n%58n)]+s;n/=58n;}for(const b of bytes){if(b!==0)break;s='1'+s;}return s;}
/** Journal exact signed bytes before send. Expired blockhashes require review;
 * never automatically re-sign an uncertain payment with a new blockhash. */
export class SolanaJournal {
 constructor(store,sdk){this.db=store.db;this.sdk=sdk;this.db.exec('CREATE TABLE IF NOT EXISTS solana_operations(id TEXT PRIMARY KEY, genesis TEXT NOT NULL, signature TEXT NOT NULL, raw BLOB NOT NULL, last_valid INTEGER NOT NULL, intent_hash TEXT NOT NULL);');}
 async prepare(id,instruction,feePayer,signers){
   await this.sdk.verify();const intent=hash(Buffer.concat([instruction.programId.toBytes(),instruction.data,...instruction.keys.map(k=>Buffer.concat([k.pubkey.toBytes(),Buffer.from([Number(k.isSigner),Number(k.isWritable)])])),feePayer.publicKey.toBytes()])).toString('hex');
   const existing=this.db.prepare('SELECT * FROM solana_operations WHERE id=?').get(id);if(existing){if(existing.intent_hash!==intent||existing.genesis!==this.sdk.manifest.genesis)throw Error('Conflicting Solana operation');return {signature:existing.signature};}
   const latest=await this.sdk.connection.getLatestBlockhash('finalized');const tx=new Transaction({feePayer:feePayer.publicKey,...latest}).add(instruction);await tx.sign(...[...new Map([feePayer,...signers].map(k=>[k.publicKey.toBase58(),k])).values()]);
   const raw=await tx.serialize(),signature=base58(tx.signature);
   this.db.prepare('INSERT INTO solana_operations VALUES(?,?,?,?,?,?)').run(id,this.sdk.manifest.genesis,signature,raw,latest.lastValidBlockHeight,intent);return {signature};
 }
 async broadcast(id){await this.sdk.verify();const op=this.db.prepare('SELECT * FROM solana_operations WHERE id=?').get(id);if(!op||op.genesis!==this.sdk.manifest.genesis)throw Error('Unknown operation/network');
   const status=(await this.sdk.connection.getSignatureStatuses([op.signature],{searchTransactionHistory:true})).value[0];
   if(status)return {signature:op.signature,state:status.err?'Failed':status.confirmationStatus};
   if(await this.sdk.connection.getBlockHeight('finalized')>op.last_valid)throw Error('Expired or uncertain Solana operation: inspect canonical run and wallet history before re-signing');
   const signature=await this.sdk.connection.sendRawTransaction(Buffer.from(op.raw),{skipPreflight:false,maxRetries:0});if(signature!==op.signature)throw Error('Signature mismatch');return {signature,state:'Submitted'};
 }
}
