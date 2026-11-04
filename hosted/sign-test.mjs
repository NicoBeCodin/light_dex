// Operator test helper. A test-agent seed arrives through stdin, never argv.
import {readFile} from 'node:fs/promises';
import {webcrypto,createPrivateKey,createPublicKey} from 'node:crypto';
try {
 let raw='';for await(const chunk of process.stdin){raw+=chunk;if(raw.length>65536)throw Error();}
 const input=JSON.parse(raw);
 const der=Buffer.concat([Buffer.from('302e020100300506032b657004220420','hex'),Buffer.from(input.seed,'hex')]);
 const key=createPrivateKey({key:der,format:'der',type:'pkcs8'});
 const address=createPublicKey(key).export({type:'spki',format:'der'}).subarray(-32).toString('hex');
 if(input.address_only){process.stdout.write(address);process.exit(0);}
 const transaction={[input.kind]:{...input.fields,account_id:address,nonce:input.nonce}};
 const {instance:{exports:wasm}}=await WebAssembly.instantiate(await readFile(new URL('./public/codec.wasm',import.meta.url)));
 const bytes=new TextEncoder().encode(JSON.stringify({chain:[...Buffer.from(input.chain_id,'hex')],transaction}));
 const pointer=wasm.allocate(bytes.length);new Uint8Array(wasm.memory.buffer,pointer,bytes.length).set(bytes);
 if(wasm.encode(pointer,bytes.length)!==1)throw Error();
 const encoded=new Uint8Array(wasm.memory.buffer,wasm.output_ptr(),wasm.output_len()).slice();wasm.release(pointer,bytes.length);
 const signer=await webcrypto.subtle.importKey('pkcs8',der,'Ed25519',false,['sign']);
 const signature=Buffer.from(await webcrypto.subtle.sign('Ed25519',signer,encoded)).toString('hex');
 process.stdout.write(JSON.stringify({transaction,signature}));
} catch {console.error('Could not sign test transaction');process.exit(1);}
