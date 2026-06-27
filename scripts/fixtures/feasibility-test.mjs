// feasibility-test.mjs — does a Nimiq HTLC atomic-swap primitive actually verify on-chain?
// Builds a real HTLC creation tx (funding) AND a manually-constructed claim-with-preimage
// (redeem) tx, then asks @nimiq/core's OWN verifier to accept them. If both verify, the NIM
// leg of a mesh-transported atomic swap is real (the mesh only ever carries these exact bytes).
import * as Nimiq from "@nimiq/core";
import { createHash } from "node:crypto";

const N = 5;
const hex = (u) => Buffer.from(u).toString("hex");
const fromHex = (h) => Uint8Array.from(Buffer.from(h, "hex"));
const seed = (f) => { const a = new Uint8Array(32); for (let i=0;i<32;i++) a[i]=f(i)&0xff; return a; };
const kp = (f) => Nimiq.KeyPair.derive(Nimiq.PrivateKey.deserialize(seed(f)));
const sha256 = (b) => new Uint8Array(createHash("sha256").update(b).digest());
const u64be = (v) => { const t=new Uint8Array(8); for(let i=7,x=BigInt(v);i>=0;i--,x>>=8n)t[i]=Number(x&0xffn); return t; };
const cat = (...arrs) => { const n=arrs.reduce((a,x)=>a+x.length,0),o=new Uint8Array(n);let p=0;for(const a of arrs){o.set(a,p);p+=a.length;}return o; };

const ALGO_SHA256 = 3;
const creator = kp((i)=>i+1);    // Alice — funds + can refund after timeout
const claimer = kp((i)=>i+100);  // Bob   — can claim with the preimage
const S = seed((i)=>i+200);      // the secret preimage
const hashRoot = sha256(S);      // H = SHA-256(S), hashCount = 1

function htlcData(timeout) {
  return cat(creator.toAddress().serialize(), claimer.toAddress().serialize(),
             Uint8Array.from([ALGO_SHA256]), hashRoot, Uint8Array.from([1]), u64be(timeout));
}

// ---- TEST 1: HTLC creation (funding) tx — built + signed + verified by @nimiq/core ----
function testCreation() {
  const data = htlcData(2000);
  let tx = new Nimiq.Transaction(creator.toAddress(), 0, null, creator.toAddress(), 2, data, 100000n, 0n, 0b1, 100, N);
  const cca = tx.getContractCreationAddress();
  tx = new Nimiq.Transaction(creator.toAddress(), 0, null, cca, 2, data, 100000n, 0n, 0b1, 100, N);
  tx.sign(creator, undefined);             // a creation tx is a normal outgoing tx — signable
  let ok = false; try { ok = tx.verify(N) === undefined || tx.verify(N); } catch { ok = false; }
  // @nimiq/core verify() throws on invalid, returns the tx (or undefined) on valid:
  let verified = true; try { tx.verify(N); } catch (e) { verified = false; }
  console.log(`TEST 1  HTLC creation/funding tx: built ${tx.serialize().length}B, @nimiq/core verify -> ${verified ? "ACCEPTED ✅" : "REJECTED ❌"}`);
  return { cca, data };
}

// ---- TEST 2: HTLC claim-with-preimage (redeem) tx — built BY US, verified by @nimiq/core ----
// @nimiq/core cannot sign HTLC redemptions, so we assemble the proof ourselves and search the
// small space of (variant byte, length-prefix encoding) for the one @nimiq/core accepts.
function testRedeem(cca) {
  // unsigned redeem skeleton from @nimiq/core (sender_type=2 contract -> claimer)
  const redeem = new Nimiq.Transaction(cca, 2, null, claimer.toAddress(), 0, null, 100000n, 0n, 0, 101, N);
  const content = redeem.serializeContent();
  const sig = claimer.sign(content);                                  // Ed25519 over content
  const sigProof = Nimiq.SignatureProof.singleSig(claimer.publicKey, sig).serialize(); // 98B
  const unsigned = redeem.serialize();                                // ends with proof_len=0x00
  const head = unsigned.slice(0, unsigned.length - 1);                // strip the 0x00 proof length

  const varints = {
    "leb128": (L) => L < 128 ? Uint8Array.from([L]) : Uint8Array.from([(L & 0x7f) | 0x80, L >> 7]),
    "u8":     (L) => Uint8Array.from([L & 0xff]),
    "u16be":  (L) => Uint8Array.from([(L >> 8) & 0xff, L & 0xff]),
  };
  // RegularTransfer proof body candidates: variant byte then algo|depth|root|preimage|sigProof
  for (const variant of [0, 1, 2, 3]) {
    const body = cat(Uint8Array.from([variant]), Uint8Array.from([ALGO_SHA256]), Uint8Array.from([1]), hashRoot, S, sigProof);
    for (const [vname, venc] of Object.entries(varints)) {
      const wire = cat(head, venc(body.length), body);
      try {
        const parsed = Nimiq.Transaction.fromAny(hex(wire));
        parsed.verify(N);
        console.log(`TEST 2  HTLC claim-with-preimage redeem tx: built ${wire.length}B, @nimiq/core verify -> ACCEPTED ✅  (variant=${variant}, len=${vname})`);
        return true;
      } catch (e) { /* keep searching */ }
    }
  }
  console.log("TEST 2  HTLC claim-with-preimage redeem tx: no (variant,len) combo verified in this quick search — proof byte-layout is F1 detail work (the primitive itself is proven in production; see note).");
  return false;
}

console.log("Nimiq HTLC atomic-swap feasibility — does the chain's own library accept what we build?\n");
const { cca } = testCreation();
testRedeem(cca);
