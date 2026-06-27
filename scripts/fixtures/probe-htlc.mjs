// probe-htlc.mjs — F0 spike: nail the exact Albatross HTLC byte layout from @nimiq/core 2.7.0.
// We construct the HTLC *creation data* bytes ourselves (no builder exists), build a tx with
// recipient_type=HTLC(2) + flags=contract-creation(0b1), and let @nimiq/core PARSE them back
// (toPlain) to prove our layout is right. Then we probe a redeem tx's serializeContent.
import * as Nimiq from "@nimiq/core";

const N = 5; // testnet
const hex = (u) => Buffer.from(u).toString("hex");
const seed = (f) => { const a = new Uint8Array(32); for (let i=0;i<32;i++) a[i]=f(i)&0xff; return a; };
const kp = (f) => Nimiq.KeyPair.derive(Nimiq.PrivateKey.deserialize(seed(f)));

const alice = kp((i)=>i+1);          // HTLC sender (refund) — the NIM giver
const bob   = kp((i)=>i+100);        // HTLC recipient (claim with preimage) — the NIM receiver
const S     = seed((i)=>i+200);      // 32-byte preimage secret
// hashRoot = SHA-256(S) for hashCount=1 (cross-chain hashlock uses sha256, algo id we probe)
const { createHash } = await import("node:crypto");
const sha256 = (b) => new Uint8Array(createHash("sha256").update(b).digest());
const hashRoot = sha256(S);

function htlcData({ algoByte, timeout, timeoutBytes }) {
  // candidate layout: sender(20) recipient(20) hashAlgorithm(1) hashRoot(32) hashCount(1) timeout(N)
  const parts = [];
  parts.push(alice.toAddress().serialize());        // 20
  parts.push(bob.toAddress().serialize());          // 20
  parts.push(Uint8Array.from([algoByte]));          // 1
  parts.push(hashRoot);                             // 32
  parts.push(Uint8Array.from([1]));                 // hashCount = 1
  const t = new Uint8Array(timeoutBytes);
  // big-endian
  for (let i=timeoutBytes-1, v=timeout; i>=0; i--, v=Math.floor(v/256)) t[i]=v&0xff;
  parts.push(t);                                    // timeout
  const len = parts.reduce((a,p)=>a+p.length,0);
  const out = new Uint8Array(len); let o=0;
  for (const p of parts){ out.set(p,o); o+=p.length; }
  return out;
}

function tryCreation(label, opts) {
  const data = htlcData(opts);
  // recipient for a creation tx must be the contract-creation address; compute it by first
  // building with a dummy recipient, then rebuild with the computed address.
  try {
    const dummy = alice.toAddress();
    let tx = new Nimiq.Transaction(alice.toAddress(), 0, null, dummy, 2, data, 1000n, 0n, 0b1, 100, N);
    const cca = tx.getContractCreationAddress();
    tx = new Nimiq.Transaction(alice.toAddress(), 0, null, cca, 2, data, 1000n, 0n, 0b1, 100, N);
    const plain = tx.toPlain();
    console.log(`\n### ${label}  (data ${data.length}B, algoByte=${opts.algoByte}, timeoutBytes=${opts.timeoutBytes})`);
    console.log("  dataHex      :", hex(data));
    console.log("  contractAddr :", cca.toUserFriendlyAddress());
    console.log("  rawHex       :", hex(tx.serialize()), `(${tx.serialize().length}B)`);
    console.log("  contentHex   :", hex(tx.serializeContent()), `(${tx.serializeContent().length}B)`);
    console.log("  hash         :", tx.hash());
    console.log("  recipientData:", JSON.stringify(plain.recipientData));
    return true;
  } catch (e) {
    console.log(`\n### ${label}: THREW -> ${e.message ?? e}`);
    return false;
  }
}

console.log("alice(sender/refund):", alice.toAddress().toUserFriendlyAddress());
console.log("bob(recipient/claim):", bob.toAddress().toUserFriendlyAddress());
console.log("preimage S          :", hex(S));
console.log("hashRoot=SHA256(S)  :", hex(hashRoot));

// Probe the timeout width (u32 vs u64) and the sha256 algo discriminant (try 1=blake2b,3=sha256).
for (const tb of [4, 8]) {
  for (const algo of [1, 3]) {
    tryCreation(`creation algo=${algo} timeoutBytes=${tb}`, { algoByte: algo, timeout: 12345, timeoutBytes: tb });
  }
}

// Probe a REDEEM tx's serializeContent (from the HTLC contract, sender_type=2, to bob, no flags).
// @nimiq/core can't *sign* an HTLC redemption, but it CAN serialize the unsigned content — that's
// the byte-exact signing payload our Rust signer must reproduce.
try {
  // first make a valid creation to get the contract address with a known-good layout (filled in
  // after the probe above identifies it).
  console.log("\n--- redeem content probe deferred until layout is confirmed above ---");
} catch (e) { console.log("redeem probe err:", e.message); }
