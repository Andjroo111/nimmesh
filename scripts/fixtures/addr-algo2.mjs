import * as Nimiq from "@nimiq/core";
import { createHash } from "node:crypto";
const N=5;
const hex=(u)=>Buffer.from(u).toString("hex");
const fromHex=(h)=>Uint8Array.from(Buffer.from(h,"hex"));
const seed=(f)=>{const a=new Uint8Array(32);for(let i=0;i<32;i++)a[i]=f(i)&0xff;return a;};
const kp=(f)=>Nimiq.KeyPair.derive(Nimiq.PrivateKey.deserialize(seed(f)));
const s256=(b)=>new Uint8Array(createHash("sha256").update(b).digest());
const u64be=(v)=>{const t=new Uint8Array(8);for(let i=7,x=BigInt(v);i>=0;i--,x>>=8n)t[i]=Number(x&0xffn);return t;};
const cat=(...a)=>{const n=a.reduce((s,x)=>s+x.length,0),o=new Uint8Array(n);let p=0;for(const x of a){o.set(x,p);p+=x.length;}return o;};
const blake2b256=(b)=>{const h=new Bun.CryptoHasher("blake2b256");h.update(b);return new Uint8Array(h.digest());};
const creator=kp(i=>i+1), claimer=kp(i=>i+100), hashRoot=s256(seed(i=>i+200));
const data=cat(creator.toAddress().serialize(),claimer.toAddress().serialize(),Uint8Array.from([3]),hashRoot,Uint8Array.from([1]),u64be(2000));
let t=new Nimiq.Transaction(creator.toAddress(),0,null,creator.toAddress(),2,data,100000n,0n,0b1,100,N);
const real=hex(t.getContractCreationAddress().serialize());
const content=t.serializeContent();   // has the REAL recipient substituted in
// recipient offset in content: 2(dataLen)+82(data)+20(sender)+1(senderType)=105 .. +20
const off=2+82+20+1;
const zeroed=Uint8Array.from(content); for(let i=off;i<off+20;i++) zeroed[i]=0;
const H={
  "blake2b(content, recipient zeroed)[0..20]": hex(blake2b256(zeroed).slice(0,20)),
  "blake2b(full content)[0..20]":               hex(blake2b256(content).slice(0,20)),
  "blake2b(htlcData)[0..20]":                    hex(blake2b256(data).slice(0,20)),
  "blake2b(sender||htlcData)[0..20]":            hex(blake2b256(cat(creator.toAddress().serialize(),data)).slice(0,20)),
};
console.log("real:", real);
for(const [k,v] of Object.entries(H)) console.log(`${v===real?"✅":"  "} ${k}: ${v}`);
