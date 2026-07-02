import * as bitcoin from 'bitcoinjs-lib';
import { ECPairFactory } from 'ecpair';
import * as ecc from 'tiny-secp256k1';
import { createHash } from 'node:crypto';
const ECPair = ECPairFactory(ecc);
const { opcodes, script, payments, networks, Transaction } = bitcoin;
const NET = networks.testnet;
const hex=(b)=>Buffer.from(b).toString('hex');
const sha256=(b)=>createHash('sha256').update(b).digest();
const preimage = Buffer.from(Array.from({length:32},(_,i)=>i+1));
const hashRoot = sha256(preimage);
const recipient = ECPair.fromPrivateKey(Buffer.alloc(32,0x11));
const sender    = ECPair.fromPrivateKey(Buffer.alloc(32,0x22));
const cltv = 1782588246;
const redeem = script.compile([
  opcodes.OP_IF, opcodes.OP_SHA256, hashRoot, opcodes.OP_EQUALVERIFY, Buffer.from(recipient.publicKey), opcodes.OP_CHECKSIG,
  opcodes.OP_ELSE, script.number.encode(cltv), opcodes.OP_CHECKLOCKTIMEVERIFY, opcodes.OP_DROP, Buffer.from(sender.publicKey), opcodes.OP_CHECKSIG, opcodes.OP_ENDIF,
]);
console.log("hashRootHex:", hex(hashRoot));
console.log("recipientPubHex:", hex(recipient.publicKey));
console.log("senderPubHex:", hex(sender.publicKey));
console.log("redeemHex:", hex(redeem));
const fundTxid = "11".repeat(32), vout = 0, fundValue = 100000n, payout = 99000n;
const dest = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx";
function spend(branch, locktime, sequence, signer, label){
  const tx = new Transaction();
  tx.version = 2;
  tx.locktime = locktime;
  tx.addInput(Buffer.from(fundTxid,'hex').reverse(), vout, sequence);
  tx.addOutput(bitcoin.address.toOutputScript(dest, NET), payout);
  const sighash = tx.hashForWitnessV0(0, redeem, fundValue, Transaction.SIGHASH_ALL);
  const sig = bitcoin.script.signature.encode(Buffer.from(signer.sign(sighash)), Transaction.SIGHASH_ALL);
  tx.setWitness(0, [...branch(sig), redeem]);
  console.log(`${label}_sighashHex: ${hex(sighash)}`);
  console.log(`${label}_signedTxHex: ${hex(tx.toBuffer())}`);
}
spend((sig)=>[sig, preimage, Buffer.from([0x01])], 0, 0xffffffff, recipient, "CLAIM");
spend((sig)=>[sig, Buffer.alloc(0)], cltv, 0xfffffffe, sender, "REFUND");
