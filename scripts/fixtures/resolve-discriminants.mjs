import * as Nimiq from "@nimiq/core";
import { createHash } from "node:crypto";
const N=5;
const hex=(u)=>Buffer.from(u).toString("hex");
const seed=(f)=>{const a=new Uint8Array(32);for(let i=0;i<32;i++)a[i]=f(i)&0xff;return a;};
const kp=(f)=>Nimiq.KeyPair.derive(Nimiq.PrivateKey.deserialize(seed(f)));
const s256=(b)=>new Uint8Array(createHash("sha256").update(b).digest());
const u64be=(v)=>{const t=new Uint8Array(8);for(let i=7,x=BigInt(v);i>=0;i--,x>>=8n)t[i]=Number(x&0xffn);return t;};
const cat=(...a)=>{const n=a.reduce((s,x)=>s+x.length,0),o=new Uint8Array(n);let p=0;for(const x of a){o.set(x,p);p+=x.length;}return o;};
const creator=kp(i=>i+1), claimer=kp(i=>i+100), S=seed(i=>i+200), hashRoot=s256(S);

// (A) which AnyHash discriminant is sha256? Build creation data with byte X, parse, read hashAlgorithm.
function creationData(algoByte){return cat(creator.toAddress().serialize(),claimer.toAddress().serialize(),Uint8Array.from([algoByte]),hashRoot,Uint8Array.from([1]),u64be(2000));}
console.log("== (A) AnyHash sha256 discriminant ==");
for (const algo of [1,3]) {
  try {
    const data=creationData(algo);
    let t=new Nimiq.Transaction(creator.toAddress(),0,null,creator.toAddress(),2,data,100000n,0n,0b1,100,N);
    const cca=t.getContractCreationAddress();
    t=new Nimiq.Transaction(creator.toAddress(),0,null,cca,2,data,100000n,0n,0b1,100,N);
    t.sign(creator,undefined);
    const plain=t.toPlain();
    const rd=plain.recipientData ?? plain.data ?? "(no recipientData field)";
    console.log(`  algoByte=${algo} -> recipientData.hashAlgorithm = ${JSON.stringify(rd?.hashAlgorithm ?? rd)}`);
  } catch(e){ console.log(`  algoByte=${algo} -> ${(e.message??e).slice(0,80)}`); }
}

// (B) does a correctly-structured redeem proof PARSE in @nimiq/core? AnyHash=tag||32, PreImage=tag||32.
console.log("\n== (B) redeem RegularTransfer proof structure (does fromAny parse?) ==");
const data=creationData(3);
let c=new Nimiq.Transaction(creator.toAddress(),0,null,creator.toAddress(),2,data,100000n,0n,0b1,100,N);
const cca=c.getContractCreationAddress();
const redeem=new Nimiq.Transaction(cca,2,null,claimer.toAddress(),0,null,100000n,0n,0,101,N);
const sigProof=Nimiq.SignatureProof.singleSig(claimer.publicKey, claimer.sign(redeem.serializeContent())).serialize();
const head=redeem.serialize().slice(0,-1);
const leb=(L)=>{const o=[];let x=L;do{let b=x&0x7f;x>>=7;if(x)b|=0x80;o.push(b);}while(x);return Uint8Array.from(o);};
let parsed=null;
for (const variant of [0,1,2]) for (const anyTag of [3,1]) for (const preTag of [3,1,0,2]) {
  // proof = variant | hash_depth(1) | AnyHash(anyTag||root) | PreImage(preTag||S) | sigProof
  const body=cat(Uint8Array.from([variant,1,anyTag]),hashRoot,Uint8Array.from([preTag]),S,sigProof);
  const wire=cat(head,leb(body.length),body);
  try { const p=Nimiq.Transaction.fromAny(hex(wire)); 
    const pl=p.toPlain(); 
    parsed=`variant=${variant} anyTag=${anyTag} preTag=${preTag} -> PARSED proof.type=${pl.proof?.type}`;
    console.log("  "+parsed+"  rawHex="+hex(wire).slice(0,40)+"..");
    break;
  } catch(e){}
  if(parsed)break;
}
if(!parsed) console.log("  no structural combo PARSED -> @nimiq/core deserializer can't validate redeem; network is the only gate");
