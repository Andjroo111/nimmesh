// gen-htlc-fixtures.mjs — F1 reference fixtures for the Nimiq HTLC serializer (mesh swap).
// SEPARATE from gen-fixtures.mjs (G3 basic) so it never collides with concurrent work.
// Emits byte-exact references from @nimiq/core 2.7.0:
//   - HTLC creation tx (rawHex / serializeContent / hash / contract address / data bytes)
//   - HTLC redeem (claim) tx UNSIGNED serializeContent (the signing payload; @nimiq/core
//     cannot sign HTLC redemptions, so the full signed wire is verified separately by Rust+
//     verify-htlc.mjs).
// Run: bun install && bun run gen-htlc-fixtures.mjs
//   -> crates/nimmesh-core/tests/fixtures/swap_htlc_fixtures.json
import * as Nimiq from "@nimiq/core";
import { createHash } from "node:crypto";

const N = 5; // testnet
const hex = (u) => Buffer.from(u).toString("hex");
const seed = (f) => { const a = new Uint8Array(32); for (let i=0;i<32;i++) a[i]=f(i)&0xff; return a; };
const kp = (f) => Nimiq.KeyPair.derive(Nimiq.PrivateKey.deserialize(seed(f)));
const sha256 = (b) => new Uint8Array(createHash("sha256").update(b).digest());
const u64be = (v) => { const t=new Uint8Array(8); for(let i=7,x=BigInt(v);i>=0;i--,x>>=8n)t[i]=Number(x&0xffn); return t; };

const ALGO_SHA256 = 3, ALGO_BLAKE2B = 1;

function htlcData({ sender, recipient, algo, hashRoot, hashCount, timeout }) {
  const p = [sender.serialize(), recipient.serialize(), Uint8Array.from([algo]),
            hashRoot, Uint8Array.from([hashCount]), u64be(timeout)];
  const len = p.reduce((a,x)=>a+x.length,0), out = new Uint8Array(len); let o=0;
  for (const x of p){ out.set(x,o); o+=x.length; } return out;
}

// (creatorFill, claimerFill, secretFill, value, vsh, algo, timeout)
const cases = [
  { name: "sha256-1nim-t12345",  creator:(i)=>i+1,   claimer:(i)=>i+100, secret:(i)=>i+200, value:100000n, vsh:100, algo:ALGO_SHA256,  timeout:12345 },
  { name: "sha256-min-t0",       creator:(i)=>i+7,   claimer:(i)=>i*3+5,  secret:(i)=>i+9,   value:1n,      vsh:0,   algo:ALGO_SHA256,  timeout:0 },
  { name: "blake2b-mid-tbig",    creator:(i)=>255-i, claimer:(i)=>i+1,    secret:(i)=>i*2+1, value:50000000n,vsh:200000, algo:ALGO_BLAKE2B, timeout:4294967296 },
];

const out = { meta: {
  generator: "@nimiq/core", version: Nimiq.version ?? "2.7.0", network: "testnet", networkId: N,
  note: "Byte-exact HTLC references for nimmesh-core nimiq/htlc.rs (mesh swap F1). creationData=82B " +
        "(sender20|recipient20|algo1|hashRoot32|hashCount1|timeout u64be 8). hashAlgo: 3=sha256,1=blake2b. " +
        "redeemContent=the 67B basic content with sender_type=2. redeem full signed wire is built+verified in Rust."
}, fixtures: [] };

for (const c of cases) {
  const creator = kp(c.creator), claimer = kp(c.claimer);
  const S = seed(c.secret);
  const hashRoot = c.algo === ALGO_SHA256 ? sha256(S) : new Uint8Array(createHash("blake2b512").update(S).digest()).slice(0,32);
  const data = htlcData({ sender: creator.toAddress(), recipient: claimer.toAddress(),
                          algo: c.algo, hashRoot, hashCount: 1, timeout: c.timeout });
  // creation tx: compute the contract address, then build with it as recipient.
  let tx = new Nimiq.Transaction(creator.toAddress(), 0, null, creator.toAddress(), 2, data, c.value, 0n, 0b1, c.vsh, N);
  const cca = tx.getContractCreationAddress();
  tx = new Nimiq.Transaction(creator.toAddress(), 0, null, cca, 2, data, c.value, 0n, 0b1, c.vsh, N);
  // unsigned redeem (claim) content: from the contract, sender_type=2, to the claimer.
  const redeem = new Nimiq.Transaction(cca, 2, null, claimer.toAddress(), 0, null, c.value, 0n, 0, c.vsh + 1, N);

  out.fixtures.push({
    name: c.name,
    creatorPubKeyHex: hex(creator.publicKey.serialize()),
    creatorAddressUser: creator.toAddress().toUserFriendlyAddress(),
    claimerPubKeyHex: hex(claimer.publicKey.serialize()),
    claimerAddressUser: claimer.toAddress().toUserFriendlyAddress(),
    preimageHex: hex(S),
    hashAlgo: c.algo, hashRootHex: hex(hashRoot), hashCount: 1, timeout: c.timeout,
    value: c.value.toString(), validityStartHeight: c.vsh, networkId: N,
    htlcDataHex: hex(data),
    contractAddressUser: cca.toUserFriendlyAddress(),
    contractAddressRaw: hex(cca.serialize()),
    creationRawHex: hex(tx.serialize()),
    creationContentHex: hex(tx.serializeContent()),
    creationTxHash: tx.hash(),
    redeemContentHex: hex(redeem.serializeContent()),
    redeemValidityStartHeight: c.vsh + 1,
  });
}

const path = new URL("../../crates/nimmesh-core/tests/fixtures/swap_htlc_fixtures.json", import.meta.url).pathname;
await Bun.write(path, JSON.stringify(out, null, 2) + "\n");
console.log("wrote", out.fixtures.length, "HTLC fixtures ->", path);
for (const f of out.fixtures)
  console.log(`  ${f.name}: data ${f.htlcDataHex.length/2}B  create ${f.creationRawHex.length/2}B  redeemContent ${f.redeemContentHex.length/2}B  cca ${f.contractAddressUser.slice(0,14)}..`);
