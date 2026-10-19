import { createServer } from 'node:http';
import {readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import {JsonRpcProvider} from 'ethers';
import {Store} from './store.mjs';
import {sync} from './indexer.mjs';
import {json,capabilities} from './protocol.mjs';
export function createApp({store,provider,manifest,abi}) {
  let pending;
  const refresh=()=>pending ??=sync(store,provider,manifest,abi).finally(()=>{pending=null;});
  const files={'/':['../web/index.html','text/html'],'/app.mjs':['../web/app.mjs','text/javascript'],'/sdk.mjs':['./sdk.mjs','text/javascript'],'/protocol.mjs':['./protocol.mjs','text/javascript'],'/ethers.js':['../node_modules/ethers/dist/ethers.min.js','text/javascript']};
  return createServer(async(req,res)=>{
    res.setHeader('Cache-Control','no-store'); res.setHeader('X-Content-Type-Options','nosniff');
    res.setHeader('Content-Security-Policy',"default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' http://127.0.0.1:* http://localhost:* https:; frame-ancestors 'none'; base-uri 'none'");
    try {
      if(req.method!=='GET') {res.writeHead(405);res.end();return;}
      const path=new URL(req.url,'http://localhost').pathname;
      if(path==='/api/config') {res.setHeader('Content-Type','application/json');res.end(json({manifest,abi,capabilities}));}
      else if(path==='/api/state') {res.setHeader('Content-Type','application/json');res.end(json(await refresh()));}
      else if(files[path]) {const [file,type]=files[path];res.setHeader('Content-Type',type);const content=readFileSync(new URL(file,import.meta.url));res.end(type==='text/javascript'?content.toString().replaceAll("from 'ethers'","from '/ethers.js'"):content);}
      else {res.writeHead(404);res.end();}
    } catch {res.writeHead(503,{'Content-Type':'application/json'});res.end(json({availability:'unavailable',error:'Canonical chain state unavailable. Retry before acting.'}));}
  });
}
if(process.argv[1]===fileURLToPath(import.meta.url)) {
  const manifest=JSON.parse(readFileSync(process.env.ARRHA_MANIFEST,'utf8'));
  const abi=JSON.parse(readFileSync(new URL('../artifacts/ArrhaEscrow.json',import.meta.url),'utf8')).abi;
  const provider=new JsonRpcProvider(process.env.ARRHA_RPC_URL,undefined,{cacheTimeout:-1});
  const store=new Store(process.env.ARRHA_DB ?? '.local/settlement.sqlite');
  const server=createApp({store,provider,manifest,abi});
  server.listen(Number(process.env.PORT ?? 3300),'127.0.0.1',()=>console.log(`Arrha settlement http://127.0.0.1:${server.address().port}`));
  for(const signal of ['SIGTERM','SIGINT']) process.on(signal,()=>server.close(()=>{store.close();provider.destroy();process.exit(0);}));
}
