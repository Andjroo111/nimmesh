// Reference HTLC redeem script + P2WSH (signet) via bitcoinjs-lib, for the Rust BitcoinLeg test.
import * as bitcoin from 'bitcoinjs-lib';
const { opcodes, script, payments, networks } = bitcoin;
const hex=(b)=>Buffer.from(b).toString('hex');
// deterministic test params (pubkeys are arbitrary valid-prefix 33B; the script/address don't sign here)
const hashRoot = Buffer.from(Array.from({length:32},(_,i)=>i+1));            // 01..20
const recipientPubkey = Buffer.concat([Buffer.from([0x02]), Buffer.alloc(32,0x11)]);
const senderPubkey    = Buffer.concat([Buffer.from([0x03]), Buffer.alloc(32,0x22)]);
const cltvLocktime = 1782588246; // Unix seconds
const redeem = script.compile([
  opcodes.OP_IF, opcodes.OP_SHA256, hashRoot, opcodes.OP_EQUALVERIFY, recipientPubkey, opcodes.OP_CHECKSIG,
  opcodes.OP_ELSE, script.number.encode(cltvLocktime), opcodes.OP_CHECKLOCKTIMEVERIFY, opcodes.OP_DROP,
  senderPubkey, opcodes.OP_CHECKSIG, opcodes.OP_ENDIF,
]);
const p2wsh = payments.p2wsh({ redeem: { output: redeem, network: networks.testnet }, network: networks.testnet });
console.log("redeemScriptHex:", hex(redeem), `(${redeem.length}B)`);
console.log("p2wshAddress   :", p2wsh.address);
console.log("scriptPubKeyHex:", hex(p2wsh.output));
