// Fund the B4 BTC HTLC from the leftover wallet (no faucet) — spend the p2wpkh UTXO into the HTLC.
import * as bitcoin from 'bitcoinjs-lib';
import { ECPairFactory } from 'ecpair';
import * as ecc from 'tiny-secp256k1';
const ECPair = ECPairFactory(ecc);
const NET = bitcoin.networks.testnet, RPC = 'https://mempool.space/testnet/api';
const hex=(b)=>Buffer.from(b).toString('hex');
const seed = Buffer.from('0732f42a7ca294622134d3b54d985b2c4c741624b10b7e075e48d5613ebf3835','hex');
const kp = ECPair.fromPrivateKey(seed);
const pub = Buffer.from(kp.publicKey);
const wallet = bitcoin.payments.p2wpkh({ pubkey: pub, network: NET });
const HTLC = 'tb1q7mgalkrp2fxf229x63fr29txdmpf8v8mfxa2lry60wuucrvnzssskr336t';
const FEE = 300n;

const utxos = await (await fetch(`${RPC}/address/${wallet.address}/utxo`)).json();
console.log('wallet', wallet.address);
console.log('utxos:', JSON.stringify(utxos));
const u = utxos.filter(x=>x.status?.confirmed).sort((a,b)=>b.value-a.value)[0] || utxos.sort((a,b)=>b.value-a.value)[0];
if (!u){ console.log('no UTXO in the wallet'); process.exit(1); }
const fund = BigInt(u.value) - FEE;
console.log(`funding HTLC with ${fund} sat from ${u.txid}:${u.vout} (value ${u.value})`);
const psbt = new bitcoin.Psbt({ network: NET });
psbt.addInput({ hash: u.txid, index: u.vout, witnessUtxo: { script: wallet.output, value: BigInt(u.value) } });
psbt.addOutput({ address: HTLC, value: fund });
psbt.signInput(0, { publicKey: pub, sign: (h)=>Buffer.from(kp.sign(h)) });
psbt.finalizeAllInputs();
const raw = psbt.extractTransaction().toHex();
const resp = await fetch(`${RPC}/tx`, { method:'POST', body: raw });
const txt = await resp.text();
console.log(resp.ok ? `✅ FUNDED HTLC — txid ${txt}\n   https://mempool.space/testnet/tx/${txt}` : `broadcast http ${resp.status}: ${txt}`);
