import solc from 'solc';
import { readFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve, dirname } from 'node:path';
export const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
export function compile() {
  const names = ['ArrhaEscrow', 'TestToken', 'AttackToken'];
  const input = { language: 'Solidity', sources: Object.fromEntries(names.map(n => [`${n}.sol`, {content:readFileSync(`${root}/contracts/${n}.sol`,'utf8')}])), settings: { optimizer:{enabled:true,runs:200}, evmVersion:'cancun', outputSelection:{'*':{'*':['abi','evm.bytecode.object','evm.deployedBytecode.object','metadata']}} }};
  const output = JSON.parse(solc.compile(JSON.stringify(input), { import: p => {
    try { const contents=readFileSync(`${root}/node_modules/${p}`,'utf8');input.sources[p]={content:contents};return {contents}; }
    catch { return {error:`Missing import ${p}`}; }
  }}));
  for (const e of output.errors ?? []) { if (e.severity === 'error') throw new Error(e.formattedMessage); }
  mkdirSync(`${root}/artifacts`,{recursive:true});
  writeFileSync(`${root}/artifacts/compiler-input.json`, JSON.stringify(input,null,2));
  const artifacts = Object.fromEntries(names.map(n => [n, output.contracts[`${n}.sol`][n]]));
  for (const [n,a] of Object.entries(artifacts)) writeFileSync(`${root}/artifacts/${n}.json`,JSON.stringify(a,null,2));
  return artifacts;
}
if (process.argv[1] === fileURLToPath(import.meta.url)) { compile(); console.log(`Compiled with solc ${solc.version()}`); }
