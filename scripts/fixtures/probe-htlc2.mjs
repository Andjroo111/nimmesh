// probe-htlc2.mjs — F0 spike part 2: confirm the sha256 algo discriminant + the redeem
// serializeContent (the byte-exact signing payload our Rust signer must reproduce for a claim).
import * as Nimiq from "@nimiq/core";
import { createHash } from "node:crypto";
const N = 5;
const hex = (u) => Buffer.from(u).toString("hex");
const seed = (f) => { const a = new Uint8Array(32); for (let i=0;i<32;i++) a[i]=f(i)&0xff; return a; };
const kp = (f) => Nimiq.KeyPair.derive(Nimiq.PrivateKey.deserialize(seed(f)));
const sha256 = (b) => new Uint8Array(createHash("sha256").update(b).digest());
const u64be = (v) => { const t=new Uint8Array(8); for(let i=7,x=BigInt(v);i>=0;i--,x>>=8n)t[i]=Number(x&0xffn); return t; };

const alice = kp((i)=>i+1), bob = kp((i)=>i+100);
const S = seed((i)=>i+200), hashRoot = sha256(S);

function htlcData(algoByte, timeout) {
  const p = [alice.toAddress().serialize(), bob.toAddress().serialize(),
            Uint8Array.from([algoByte]), hashRoot, Uint8Array.from([1]), u64be(timeout)];
  const len = p.reduce((a,x)=>a+x.length,0), out = new Uint8Array(len); let o=0;
  for (const x of p){ out.set(x,o); o+=x.length; } return out;
}

for (const algo of [1, 3, 4]) {
  try {
    const data = htlcData(algo, 12345);
    let tx = new Nimiq.Transaction(alice.toAddress(), 0, null, alice.toAddress(), 2, data, 1000n, 0n, 0b1, 100, N);
    const cca = tx.getContractCreationAddress();
    tx = new Nimiq.Transaction(alice.toAddress(), 0, null, cca, 2, data, 1000n, 0n, 0b1, 100, N);
    const parsed = Nimiq.Transaction.fromAny(hex(tx.serialize()));
    const plain = parsed.toPlain(1, 0n);
    console.log(`algo=${algo} -> recipientData:`, JSON.stringify(plain.recipientData));
  } catch (e) { console.log(`algo=${algo} -> THREW: ${(e.message??e).slice(0,90)}`); }
}

// Redeem (claim) tx: FROM the HTLC contract (sender_type=2) TO bob. serializeContent is the
// signing payload; @nimiq/core can build it unsigned but sign() throws for HTLC redemption.
const data = htlcData(3, 12345);
let c = new Nimiq.Transaction(alice.toAddress(), 0, null, alice.toAddress(), 2, data, 1000n, 0n, 0b1, 100, N);
const cca = c.getContractCreationAddress();
console.log("\ncontractAddr:", cca.toUserFriendlyAddress());
try {
  const redeem = new Nimiq.Transaction(cca, 2, null, bob.toAddress(), 0, null, 1000n, 0n, 0, 200, N);
  console.log("redeem contentHex:", hex(redeem.serializeContent()), `(${redeem.serializeContent().length}B)`);
  console.log("redeem rawHex(unsigned):", hex(redeem.serialize()), `(${redeem.serialize().length}B)`);
  try { redeem.sign(bob, undefined); console.log("redeem.sign did NOT throw (unexpected)"); }
  catch (e) { console.log("redeem.sign THREW (expected):", (e.message??e).slice(0,70)); }
} catch (e) { console.log("redeem build THREW:", (e.message??e).slice(0,120)); }
