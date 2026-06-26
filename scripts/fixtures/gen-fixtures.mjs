// gen-fixtures.mjs — produce REFERENCE FIXTURES from @nimiq/core (v2.7.0) for the
// bitmesh-core G3 byte-exact signer. The Rust signer's output MUST equal these
// byte-for-byte (rawHex, txHash, serializeContent, proof, address round-trip).
// Run: bun install && bun run gen
//   -> writes crates/bitmesh-core/tests/fixtures/g3_signing_fixtures.json (the committed,
//      canonical copy the Rust acceptance test asserts against).
import * as Nimiq from "@nimiq/core";
const NETWORK_TESTNET = 5;
const kpFrom = (b) => Nimiq.KeyPair.derive(Nimiq.PrivateKey.deserialize(b));
const seed = (fill) => { const a = new Uint8Array(32); for (let i=0;i<32;i++) a[i]=(fill(i))&0xff; return a; };
const hex = (u) => Buffer.from(u).toString("hex");
// (privFill, recipFill, value, vsh) — fee is always 0 (basic format), testnet.
const cases = [
  { name: "seq-keys-100k-vsh1234", priv: (i)=>i+1,    recip: (i)=>i+100, value: 100000n, vsh: 1234 },
  { name: "one-luna-vsh0",        priv: (i)=>i+7,    recip: (i)=>i*3+5,  value: 1n,      vsh: 0 },
  { name: "big-value-vsh-max32",  priv: (i)=>i*7+1,  recip: (i)=>i+200,  value: 4200000000000n, vsh: 4294967295 },
  { name: "mid-vsh-200000",       priv: (i)=>255-i,  recip: (i)=>i+1,    value: 50000000n, vsh: 200000 },
];
const out = { meta: {
  generator: "@nimiq/core", version: Nimiq.version ?? "2.7.0",
  network: "testnet", networkId: NETWORK_TESTNET, fee: "0",
  note: "Byte-exact reference for bitmesh-core src/nimiq G3. serializeContent=67B, proof=98B, rawHex=139B basic-format transfer. txHash=Blake2b256(serializeContent)."
}, fixtures: [] };
for (const c of cases) {
  const skBytes = seed(c.priv);
  const kp = kpFrom(skBytes);
  const recipient = kpFrom(seed(c.recip)).toAddress();
  const tx = Nimiq.TransactionBuilder.newBasic(kp.toAddress(), recipient, c.value, 0n, c.vsh, NETWORK_TESTNET);
  const content = tx.serializeContent();
  const sig = kp.sign(content);
  const proof = Nimiq.SignatureProof.singleSig(kp.publicKey, sig).serialize();
  tx.proof = proof;
  out.fixtures.push({
    name: c.name,
    privKeyHex: hex(skBytes),
    pubKeyHex: hex(kp.publicKey.serialize()),
    senderAddressUser: kp.toAddress().toUserFriendlyAddress(),
    senderAddressRaw: hex(kp.toAddress().serialize()),
    recipientAddressUser: recipient.toUserFriendlyAddress(),
    recipientAddressRaw: hex(recipient.serialize()),
    value: c.value.toString(),
    fee: "0",
    validityStartHeight: c.vsh,
    networkId: NETWORK_TESTNET,
    serializeContentHex: hex(content),
    signatureHex: hex(sig.serialize()),
    proofHex: hex(proof),
    rawHex: hex(tx.serialize()),
    txHash: tx.hash(),
  });
}
const path = new URL(
  "../../crates/bitmesh-core/tests/fixtures/g3_signing_fixtures.json",
  import.meta.url,
).pathname;
await Bun.write(path, JSON.stringify(out, null, 2) + "\n");
console.log("wrote", out.fixtures.length, "fixtures ->", path);
for (const f of out.fixtures) console.log(`  ${f.name}: raw ${f.rawHex.length/2}B hash ${f.txHash.slice(0,12)}..`);
