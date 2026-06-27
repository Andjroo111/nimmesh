import * as Nimiq from "@nimiq/core";
import { createHash } from "node:crypto";
const N=5;
const hex=(u)=>Buffer.from(u).toString("hex");
const seed=(f)=>{const a=new Uint8Array(32);for(let i=0;i<32;i++)a[i]=f(i)&0xff;return a;};
const kp=(f)=>Nimiq.KeyPair.derive(Nimiq.PrivateKey.deserialize(seed(f)));
const s256=(b)=>new Uint8Array(createHash("sha256").update(b).digest());
const u64be=(v)=>{const t=new Uint8Array(8);for(let i=7,x=BigInt(v);i>=0;i--,x>>=8n)t[i]=Number(x&0xffn);return t;};
const cat=(...a)=>{const n=a.reduce((s,x)=>s+x.length,0),o=new Uint8Array(n);let p=0;for(const x of a){o.set(x,p);p+=x.length;}return o;};
const leb=(L)=>{const o=[];let x=L;do{let b=x&0x7f;x>>=7;if(x)b|=0x80;o.push(b);}while(x);return Uint8Array.from(o);};
const creator=kp(i=>i+1), claimer=kp(i=>i+100), S=seed(i=>i+200), hashRoot=s256(S);
// build the contract + an unsigned redeem skeleton (proof_len=0 at the tail)
const data=cat(creator.toAddress().serialize(),claimer.toAddress().serialize(),Uint8Array.from([3]),hashRoot,Uint8Array.from([1]),u64be(2000));
let t=new Nimiq.Transaction(creator.toAddress(),0,null,creator.toAddress(),2,data,100000n,0n,0b1,100,N);
const cca=t.getContractCreationAddress();
const redeem=new Nimiq.Transaction(cca,2,null,claimer.toAddress(),0,null,100000n,0n,0,101,N);
const sigProof=Nimiq.SignatureProof.singleSig(claimer.publicKey, claimer.sign(redeem.serializeContent())).serialize();
const head=redeem.serialize().slice(0,-1); // strip trailing proof_len=0x00
// AnyHash(sha256)=03||32 ; PreImage tag unknown -> search; proof variant RegularTransfer (PoS=0)
const ANYHASH_SHA256=3;
let win=null, tries=0;
for (const variant of [0,1,2]) {
  for (const preTag of [3,1,0,2]) {     // PreImage discriminant candidates
    for (const hashDepth of [1]) {
      const body=cat(
        Uint8Array.from([variant, hashDepth, ANYHASH_SHA256]), hashRoot,  // hash_root: AnyHash
        Uint8Array.from([preTag]), S,                                     // pre_image: PreImage
        sigProof);
      const wire=cat(head, leb(body.length), body);
      tries++;
      try { const p=Nimiq.Transaction.fromAny(hex(wire)); p.verify(N);
        win=`variant=${variant} preTag=${preTag} bodyLen=${body.length}`; 
        console.log("redeem rawHex:", hex(wire), `(${wire.length}B)`);
        break; } catch(e){ if(tries<=2) console.log(`  try v=${variant} pre=${preTag}: ${(e.message??e).slice(0,60)}`); }
    }
    if(win)break;
  }
  if(win)break;
}
console.log(`\ntried ${tries} -> ${win ? "✅ CLAIM-WITH-PREIMAGE REDEEM VERIFIED: "+win : "none verified"}`);
