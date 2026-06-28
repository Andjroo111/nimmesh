// Fund a BTC HTLC from the persistent treasury wallet (no faucet). Usage:
//   NIMMESH_BTC_SEED=<hex> bun run fund-htlc-from-treasury.mjs <htlc-address> [sats]
import * as bitcoin from 'bitcoinjs-lib';
import { ECPairFactory } from 'ecpair';
import * as ecc from 'tiny-secp256k1';
const ECPair = ECPairFactory(ecc);
const NET = bitcoin.networks.testnet, RPC = 'https://mempool.space/testnet/api';
const hex=(b)=>Buffer.from(b).toString('hex');
const HTLC = process.argv[2];
if (!HTLC) { console.log('usage: fund-htlc-from-treasury.mjs <htlc-address> [sats]'); process.exit(1); }
const want = process.argv[3] ? BigInt(process.argv[3]) : null;
const seed = Buffer.from(process.env.NIMMESH_BTC_SEED, 'hex');
const kp = ECPair.fromPrivateKey(seed);
const pub = Buffer.from(kp.publicKey);
const wallet = bitcoin.payments.p2wpkh({ pubkey: pub, network: NET });
const FEE = 300n;
const utxos = await (await fetch(`${RPC}/address/${wallet.address}/utxo`)).json();
console.log('treasury', wallet.address, 'utxos', utxos.length);
const u = utxos.sort((a,b)=>b.value-a.value)[0];
if (!u) { console.log('treasury is empty — fund', wallet.address, 'once via a testnet3 faucet'); process.exit(1); }
const send = want ?? (BigInt(u.value) - FEE);
const change = BigInt(u.value) - send - FEE;
const psbt = new bitcoin.Psbt({ network: NET });
psbt.addInput({ hash: u.txid, index: u.vout, witnessUtxo: { script: wallet.output, value: BigInt(u.value) } });
psbt.addOutput({ address: HTLC, value: send });
if (change > 330n) psbt.addOutput({ address: wallet.address, value: change });
psbt.signInput(0, { publicKey: pub, sign: (h)=>Buffer.from(kp.sign(h)) });
psbt.finalizeAllInputs();
const raw = psbt.extractTransaction().toHex();
const r = await fetch(`${RPC}/tx`, { method:'POST', body: raw });
const t = await r.text();
console.log(r.ok ? `✅ funded HTLC ${send} sat -> ${HTLC}\n   tx ${t}` : `broadcast http ${r.status}: ${t}`);
