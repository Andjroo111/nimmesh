// Determine the Albatross contract-creation-address algorithm for the F1 Rust signer.
import * as Nimiq from "@nimiq/core";
import { readFileSync } from "node:fs";
const N=5;
const hex=(u)=>Buffer.from(u).toString("hex");
const seed=(f)=>{const a=new Uint8Array(32);for(let i=0;i<32;i++)a[i]=f(i)&0xff;return a;};
const kp=(f)=>Nimiq.KeyPair.derive(Nimiq.PrivateKey.deserialize(seed(f)));
const sha256=(b)=>new Uint8Array(require?.("node:crypto")?.createHash?.("sha256")?.update(b)?.digest()??[]);
import { createHash } from "node:crypto";
const s256=(b)=>new Uint8Array(createHash("sha256").update(b).digest());
const u64be=(v)=>{const t=new Uint8Array(8);for(let i=7,x=BigInt(v);i>=0;i--,x>>=8n)t[i]=Number(x&0xffn);return t;};
const cat=(...a)=>{const n=a.reduce((s,x)=>s+x.length,0),o=new Uint8Array(n);let p=0;for(const x of a){o.set(x,p);p+=x.length;}return o;};
const creator=kp(i=>i+1), claimer=kp(i=>i+100), hashRoot=s256(seed(i=>i+200));
const data=cat(creator.toAddress().serialize(),claimer.toAddress().serialize(),Uint8Array.from([3]),hashRoot,Uint8Array.from([1]),u64be(2000));
// real address from @nimiq/core
let t=new Nimiq.Transaction(creator.toAddress(),0,null,creator.toAddress(),2,data,100000n,0n,0b1,100,N);
const real=hex(t.getContractCreationAddress().serialize());
// hypothesis: hash of serializeContent with recipient = ZERO address, take [0..20]
const ZERO=new Nimiq.Address(new Uint8Array(20));
const tz=new Nimiq.Transaction(creator.toAddress(),0,null,ZERO,2,data,100000n,0n,0b1,100,N);
const hZeroContent = tz.hash();                 // Blake2b256(serializeContent w/ zero recipient), hex
console.log("real contract addr   :", real);
console.log("hash(zero-recipient) :", hZeroContent, "-> [0..20]:", hZeroContent.slice(0,40));
console.log("MATCH (zero-recipient content hash[0..20])?", hZeroContent.slice(0,40)===real);
