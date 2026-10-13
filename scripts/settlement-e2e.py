#!/usr/bin/env python3
"""Real ephemeral devnet: signed CLI transfer -> receipt -> persistent indexer.
No mocks, hosted providers, external wallets or funded networks are used.
"""
import argparse, hashlib, json, os, signal, socket, subprocess, tempfile, time
from pathlib import Path
from urllib.request import Request, build_opener, ProxyHandler
from urllib.error import HTTPError

HTTP=build_opener(ProxyHandler({}))
if not __debug__:
    raise RuntimeError("Settlement verification requires assertions enabled; do not use python -O or PYTHONOPTIMIZE")
def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1",0));return sock.getsockname()[1]
def get(url):
    with HTTP.open(url,timeout=5) as response:return json.load(response)
def rpc(url,method,params):
    request=Request(url,data=json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params}).encode(),headers={"Content-Type":"application/json"})
    with HTTP.open(request,timeout=5) as response:result=json.load(response)
    if result.get("error"):raise RuntimeError(result["error"])
    return result["result"]
def wait(check,timeout=120):
    deadline=time.monotonic()+timeout;error=None
    while time.monotonic()<deadline:
        try:
            value=check()
            if value:return value
        except (OSError,ValueError,RuntimeError) as exc:error=exc
        time.sleep(.25)
    raise RuntimeError(f"workflow timed out: {error}")
def stop(process):
    if process.poll() is None:
        process.send_signal(signal.SIGINT)
        try:process.wait(timeout=10)
        except subprocess.TimeoutExpired:process.kill();process.wait(timeout=10)
def main():
    parser=argparse.ArgumentParser();parser.add_argument("--bin-dir",default="target/debug");parser.add_argument("--receipt",default="artifacts/settlement-e2e.json");args=parser.parse_args()
    binaries=Path(args.bin_dir).resolve();receipt_path=Path(args.receipt)
    for name in ["aether-node","aetherctl","aether-indexer"]:
        if not (binaries/name).is_file():raise RuntimeError(f"missing actual binary {name}")
    binary_hashes={name:hashlib.sha256((binaries/name).read_bytes()).hexdigest() for name in ["aether-node","aetherctl","aether-indexer"]}
    processes=[]
    with tempfile.TemporaryDirectory(prefix="aether-settlement-") as temp:
        root=Path(temp);node_port,index_port,p2p,metrics=[port() for _ in range(4)];node_url=f"http://127.0.0.1:{node_port}";index_url=f"http://127.0.0.1:{index_port}"
        env={**os.environ,"AETHER_NETWORK":"devnet","AETHER_NODE_DB_PATH":str(root/"node"),"AETHER_RPC_PORT":str(node_port),"AETHER_P2P_PORT":str(p2p),"AETHER_METRICS_PORT":str(metrics),"INDEXER_PORT":str(index_port),"INDEXER_DB_PATH":str(root/"index"),"INDEXER_RPC_URL":node_url}
        for key in ["AETHER_BOOTSTRAP_PEERS","AETHER_GENESIS_PATH","AETHER_CONFIG_PATH","AETHER_VALIDATOR_KEY"]:env.pop(key,None)
        logs=[]
        def launch(name):
            log=open(root/f"{name}-{len(logs)}.log","w+");logs.append(log)
            process=subprocess.Popen([str(binaries/name)],env=env,stdout=log,stderr=subprocess.STDOUT);processes.append(process);return process
        def cli(*options):
            return json.loads(subprocess.check_output([str(binaries/"aetherctl"),"--config",str(root/"config.toml"),*options],env=env,text=True,timeout=20))
        try:
            node=launch("aether-node");wait(lambda:rpc(node_url,"aeth_health",[]),30)
            assert rpc(node_url,"aeth_chainId",[])=="0x384","expected isolated devnet chain 900"
            key=cli("keys","generate","--out",str(root/"sender.json"))
            recipient=cli("keys","generate","--out",str(root/"recipient.json"))
            (root/"config.toml").write_text(f'endpoint = "{node_url}"\ndefault_key = "{root / "sender.json"}"\n')
            rpc(node_url,"aeth_requestAirdrop",[key["address"],"100000000"])
            index=launch("aether-indexer")
            transfer=cli("transfer","--to",recipient["address"],"--amount","1000","--nonce","0","--memo","ephemeral settlement e2e")
            assert transfer["accepted"],"node rejected signed transfer"
            tx_hash=transfer["tx_hash"]
            onchain=wait(lambda:rpc(node_url,"aeth_getTransactionReceipt",[tx_hash]))
            assert onchain["status"]=="Success",onchain
            indexed=wait(lambda:get(index_url+"/tx/"+tx_hash))
            for field in ["tx_hash","block_hash","slot","status","gas_used"]:assert indexed[field]==onchain[field],field
            block=get(index_url+f'/block/{onchain["slot"]}');assert block["hash"]==onchain["block_hash"]
            assert onchain["tx_hash"] in block["tx_hashes"]
            account=rpc(node_url,"aeth_getAccount",[recipient["address"],"latest"]);assert account["balance"]==1000,account
            try:get(index_url+"/tx/0x"+"00"*32);raise AssertionError("unknown transaction falsely successful")
            except HTTPError as error:assert error.code==404
            cursor=get(index_url+"/status")["next_slot"]
            stop(index);stop(node)
            index=launch("aether-indexer")
            persisted=wait(lambda:get(index_url+"/tx/"+tx_hash),30);assert persisted==indexed
            assert get(index_url+"/status")["next_slot"]>=cursor
            node=launch("aether-node");wait(lambda:rpc(node_url,"aeth_health",[]),30)
            assert rpc(node_url,"aeth_getTransactionReceipt",[tx_hash])==onchain
            wait(lambda:get(index_url+"/status")["synchronization_error"] is None)
            report={"complete":True,"scope":"ephemeral devnet signed transfer/receipt/indexer durable restart","head":subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip(),"tx_hash":tx_hash,"slot":onchain["slot"],"recipient_balance":account["balance"],"indexer_cursor_before_restart":cursor,"binary_sha256":binary_hashes,"fixture_providers":False,"funded_network":False}
            receipt_path.parent.mkdir(parents=True,exist_ok=True);receipt_path.write_text(json.dumps(report,indent=2)+"\n");print(json.dumps(report))
        except Exception:
            for log in logs:
                log.flush();log.seek(0);print(log.read()[-5000:])
            raise
        finally:
            for process in reversed(processes):stop(process)
            for log in logs:log.close()
if __name__=="__main__":main()
