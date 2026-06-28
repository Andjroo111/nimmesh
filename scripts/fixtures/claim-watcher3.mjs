// Dual-chain watcher: watches the HTLC on BOTH the public signet AND Mutinynet; claims on whichever
// faucet funds it, broadcasting to that same chain. Same address works on both (tb1 = signet params).
import * as bitcoin from 'bitcoinjs-lib';
import { ECPairFactory } from 'ecpair';
import * as ecc from 'tiny-secp256k1';
import { createHash } from 'node:crypto';
const ECPair = ECPairFactory(ecc);
const { opcodes, script, payments, networks, Transaction, address } = bitcoin;
const NET = networks.testnet;
const CHAINS = [
  { name: "testnet3", api: "https://mempool.space/testnet/api" },
  { name: "testnet4", api: "https://mempool.space/testnet4/api" },
  { name: 'signet',    api: 'https://mempool.space/signet/api' },
  { name: 'mutinynet', api: 'https://mutinynet.com/api' },
];
const hex=(b)=>Buffer.from(b).toString('hex');
const sha256=(b)=>createHash('sha256').update(b).digest();
const seed = Buffer.from('0732f42a7ca294622134d3b54d985b2c4c741624b10b7e075e48d5613ebf3835','hex');
const kp = ECPair.fromPrivateKey(seed);
const pub = Buffer.from(kp.publicKey);
const secret = Buffer.from('250ffdd507bcbf335700770a183ce3c17b3725a5272ff9bd4414cefaa74e256d','hex');
const cltv = 1782604199;
const redeem = script.compile([opcodes.OP_IF,opcodes.OP_SHA256,sha256(secret),opcodes.OP_EQUALVERIFY,pub,opcodes.OP_CHECKSIG,opcodes.OP_ELSE,script.number.encode(cltv),opcodes.OP_CHECKLOCKTIMEVERIFY,opcodes.OP_DROP,pub,opcodes.OP_CHECKSIG,opcodes.OP_ENDIF]);
const htlcAddr = payments.p2wsh({redeem:{output:redeem,network:NET},network:NET}).address;
const payout = payments.p2wpkh({pubkey:pub,network:NET}).address;
const FEE = 330n;
console.log('dual-chain watcher up. address', htlcAddr);
async function claimOn(api, u){
  const tx = new Transaction(); tx.version=2; tx.locktime=0;
  tx.addInput(Buffer.from(u.txid,'hex').reverse(), u.vout, 0xffffffff);
  tx.addOutput(address.toOutputScript(payout, NET), BigInt(u.value)-FEE);
  const sh = tx.hashForWitnessV0(0, redeem, BigInt(u.value), Transaction.SIGHASH_ALL);
  const sig = bitcoin.script.signature.encode(Buffer.from(kp.sign(sh)), Transaction.SIGHASH_ALL);
  tx.setWitness(0, [sig, secret, Buffer.from([0x01]), redeem]);
  const r = await fetch(api+'/tx', {method:'POST', body:hex(tx.toBuffer())});
  return { ok:r.ok, txt: await r.text(), api };
}
for (let i=0;i<150;i++){ // ~50 min @ 20s
  for (const c of CHAINS){
    let utxos=[]; try{ utxos = await (await fetch(`${c.api}/address/${htlcAddr}/utxo`)).json(); }catch(e){}
    if (utxos && utxos.length){
      const u = utxos.sort((a,b)=>b.value-a.value)[0];
      console.log(`FUNDED on ${c.name}: ${u.value} sat at ${u.txid}:${u.vout}`);
      const res = await claimOn(c.api, u);
      if (res.ok){ const exp = c.name==='signet'?'https://mempool.space/signet/tx/':'https://mutinynet.com/tx/';
        console.log(`✅ CLAIM BROADCAST on ${c.name} — txid ${res.txt}`); console.log('   '+exp+res.txt); }
      else console.log(`claim broadcast failed on ${c.name}: ${res.txt}`);
      process.exit(0);
    }
  }
  if (i%6===0) console.log(`  t+${i*20}s: signet 0, mutinynet 0 …`);
  await new Promise(s=>setTimeout(s,20000));
}
console.log('watcher timed out after ~50 min — no faucet paid out to', htlcAddr);
