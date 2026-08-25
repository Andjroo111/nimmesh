#!/usr/bin/env python3
"""Add a local `getTrieProof` JSON-RPC method to a core-rs-albatross checkout.

WHY THIS EXISTS
    NIMmesh's trustless offline balance verification (issue #5, docs/BALANCE-PROOF.md)
    needs two things from a node: an accounts-trie Merkle proof, and the RAW serialized
    block header the proof is anchored to. Upstream Albatross exposes neither over
    JSON-RPC — proofs live only in the light-client network protocol
    (`RequestTrieProof`, request type 215), and the JSON `Block` type omits `diffRoot`,
    so a header can never be reconstructed from RPC JSON.

    A FULL node already holds everything required: `Blockchain::get_accounts_proof(keys)`
    is a public method on the blockchain crate. This patch surfaces it, plus the header
    bytes, through the node's own RPC. No new capability, no consensus change — it reads
    public state the node already has.

WHAT IT ADDS
    getTrieProof(addresses: [Address]) -> {
        blockNumber, blockHash, blockType: "micro"|"macro",
        header: hex,   # canonical nimiq-serde bytes; Blake2b256(header) == blockHash
        proof:  hex,   # canonical nimiq-serde bytes of the TrieProof
    }
    Both blobs are raw serializations rather than JSON re-renderings, so an offline
    client verifies the whole chain itself with no trust in the responder.

APPLIES TO
    core-rs-albatross at commit bb3aec298 (v2.0.0 + 23 CERT security commits). The
    anchors are distinctive enough to survive nearby churn; the script refuses rather
    than guessing if any anchor is missing or the patch is already applied.

USAGE
    python3 apply-gettrieproof.py --check   /path/to/core-rs-albatross
    python3 apply-gettrieproof.py           /path/to/core-rs-albatross
    then: cargo build --release -p nimiq-client

STATUS
    NOT YET COMPILED. Written against the sources read on the OVH node checkout, but the
    build has not been run (see contrib/albatross-gettrieproof/README.md). Treat the
    first `cargo build` as part of applying it, not as a formality.

SAFETY
    Run this on a checkout that is NOT the one a live validator builds from, and install
    the resulting binary under a different name for a separate, non-validator node. This
    patch touches only the RPC read surface, but restarting a block-producing validator
    to deploy an unrelated feature is an avoidable risk.
"""

import argparse
import sys
from pathlib import Path

EDITS = []


def edit(path, anchor, replacement, guard):
    """Register one anchored replacement. `guard` marks an already-patched file."""
    EDITS.append((path, anchor, replacement, guard))


# --- A: the response type -------------------------------------------------------------
_TRIE_PROOF_DATA = '''/// A Merkle proof of one or more accounts in the accounts trie, together with the block
/// header the proof is anchored to.
///
/// `header` and `proof` are hex of the canonical `nimiq-serde` serializations, NOT a JSON
/// re-rendering, so an offline client can verify the whole chain itself:
/// `Blake2b256(header) == blockHash`, and the proof verifies against the header's
/// `state_root`. The header is included because the JSON `Block` type omits `diffRoot`,
/// which makes a header impossible to reconstruct from RPC JSON alone.
///
/// LOCAL ADDITION (not upstream): serves nimmesh's trustless offline balance
/// verification. Read-only public state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrieProofData {
    /// Height of the block the proof is anchored to.
    pub block_number: u32,
    /// Hash of that block — equals `Blake2b256(header)`.
    pub block_hash: Blake2bHash,
    /// `"micro"` or `"macro"`: the two header layouts differ, so a parser must branch.
    pub block_type: String,
    /// The serialized block header (canonical `nimiq-serde` bytes).
    #[serde(with = "crate::serde_helpers::hex")]
    pub header: Vec<u8>,
    /// The serialized `TrieProof` (canonical `nimiq-serde` bytes).
    #[serde(with = "crate::serde_helpers::hex")]
    pub proof: Vec<u8>,
}

'''

_TYPES_ANCHOR = "pub type RPCResult<T, S, E> = Result<RPCData<T, S>, E>;"
edit(
    "rpc-interface/src/types.rs",
    _TYPES_ANCHOR,
    _TRIE_PROOF_DATA + _TYPES_ANCHOR,
    guard="pub struct TrieProofData",
)

# --- B: the trait method --------------------------------------------------------------
_IFACE_IMPORT = "    PenalizedSlots, RPCData, RPCResult, Slot, Staker, Validator,\n"
edit(
    "rpc-interface/src/blockchain.rs",
    _IFACE_IMPORT,
    "    PenalizedSlots, RPCData, RPCResult, Slot, Staker, TrieProofData, Validator,\n",
    guard="TrieProofData, Validator,",
)

_IFACE_METHOD_ANCHOR = (
    "    async fn get_accounts(&self) -> RPCResult<Vec<Account>, BlockchainState, Self::Error>;\n"
)
edit(
    "rpc-interface/src/blockchain.rs",
    _IFACE_METHOD_ANCHOR,
    _IFACE_METHOD_ANCHOR
    + '''
    /// Returns a Merkle proof of the given accounts against the current head's state root,
    /// together with the serialized head header the proof is anchored to. LOCAL ADDITION
    /// (not upstream): the payload nimmesh needs for trustless offline balance checks.
    async fn get_trie_proof(
        &self,
        addresses: Vec<Address>,
    ) -> RPCResult<TrieProofData, BlockchainState, Self::Error>;
''',
    guard="async fn get_trie_proof",
)

# --- C: the error variant -------------------------------------------------------------
_ERR_ANCHOR = '''    #[error("Method not implemented")]
    NotImplemented,
'''
edit(
    "rpc-server/src/error.rs",
    _ERR_ANCHOR,
    _ERR_ANCHOR
    + '''
    #[error("Invalid trie proof request: {0}")]
    InvalidTrieProofRequest(String),
''',
    guard="InvalidTrieProofRequest",
)

# --- D: the dispatcher ----------------------------------------------------------------
_DISP_USE_ACCOUNT = "use nimiq_account::{BlockLog as BBlockLog, TransactionLog};\n"
edit(
    "rpc-server/src/dispatchers/blockchain.rs",
    _DISP_USE_ACCOUNT,
    _DISP_USE_ACCOUNT + "use nimiq_block::Block as ChainBlock;\n",
    guard="use nimiq_block::Block as ChainBlock;",
)

_DISP_USE_TYPES = (
    "        ExecutedTransaction, Inherent, LogType, PenalizedSlots, RPCData, RPCResult, Slot, Staker,\n"
    "        Validator,\n"
)
edit(
    "rpc-server/src/dispatchers/blockchain.rs",
    _DISP_USE_TYPES,
    "        ExecutedTransaction, Inherent, LogType, PenalizedSlots, RPCData, RPCResult, Slot, Staker,\n"
    "        TrieProofData, Validator,\n",
    guard="TrieProofData, Validator,",
)

_DISP_USE_STREAM = "use tokio_stream::wrappers::BroadcastStream;\n"
edit(
    "rpc-server/src/dispatchers/blockchain.rs",
    _DISP_USE_STREAM,
    "use nimiq_serde::Serialize as NimiqSerialize;\n" + _DISP_USE_STREAM,
    guard="use nimiq_serde::Serialize as NimiqSerialize;",
)

_DISP_METHOD_ANCHOR = '''            Ok(RPCData::with_blockchain(accounts, &blockchain_proxy))
        } else {
            Err(Error::NotSupportedForLightBlockchain)
        }
    }
'''
edit(
    "rpc-server/src/dispatchers/blockchain.rs",
    _DISP_METHOD_ANCHOR,
    _DISP_METHOD_ANCHOR
    + '''
    /// LOCAL ADDITION (not upstream). Builds an accounts-trie Merkle proof for `addresses`
    /// against the current head, and returns it with the serialized head header so a
    /// client can verify offline: `Blake2b256(header) == blockHash`, then the proof
    /// against the header's `state_root`. Read-only public state; no keys, no signing.
    async fn get_trie_proof(
        &self,
        addresses: Vec<Address>,
    ) -> RPCResult<TrieProofData, BlockchainState, Self::Error> {
        // Mirror the network handler's bounds (consensus/src/messages/handlers.rs).
        if addresses.is_empty() {
            return Err(Error::InvalidTrieProofRequest(
                "no addresses given".to_string(),
            ));
        }
        if addresses.len() > 255 {
            return Err(Error::InvalidTrieProofRequest(format!(
                "too many addresses: {} (max 255)",
                addresses.len()
            )));
        }
        let blockchain_proxy = self.blockchain.read();
        if let BlockchainReadProxy::Full(ref blockchain) = blockchain_proxy {
            let keys: Vec<KeyNibbles> = addresses.iter().map(KeyNibbles::from).collect();
            let proof = blockchain
                .get_accounts_proof(keys.iter().collect())
                .map_err(|_| Error::NoConsensus)?;
            // The head the proof was built against, and its header bytes verbatim.
            let head = blockchain.head();
            let (header, block_type) = match &head {
                ChainBlock::Micro(block) => (block.header.serialize_to_vec(), "micro"),
                ChainBlock::Macro(block) => (block.header.serialize_to_vec(), "macro"),
            };
            let data = TrieProofData {
                block_number: head.block_number(),
                block_hash: head.hash(),
                block_type: block_type.to_string(),
                header,
                proof: proof.serialize_to_vec(),
            };
            Ok(RPCData::with_blockchain(data, &blockchain_proxy))
        } else {
            Err(Error::NotSupportedForLightBlockchain)
        }
    }
''',
    guard="async fn get_trie_proof",
)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("src", help="path to a core-rs-albatross checkout")
    ap.add_argument(
        "--check",
        action="store_true",
        help="verify every anchor is present and unpatched, then exit without writing",
    )
    args = ap.parse_args()
    src = Path(args.src)
    if not (src / "rpc-server").is_dir():
        sys.exit(f"not a core-rs-albatross checkout: {src}")

    # Read every file and validate every anchor BEFORE writing anything, so a missing
    # anchor can never leave the tree half-patched.
    originals = {}
    planned = {}
    for rel, anchor, replacement, guard in EDITS:
        path = src / rel
        if not path.is_file():
            sys.exit(f"missing file: {rel}")
        text = planned.get(rel, path.read_text())
        originals.setdefault(rel, path.read_text())
        if guard in text:
            sys.exit(f"already patched (found {guard!r} in {rel}) — nothing to do")
        if anchor not in text:
            sys.exit(f"anchor not found in {rel}; this checkout differs from bb3aec298")
        planned[rel] = text.replace(anchor, replacement, 1)

    if args.check:
        print(f"all {len(EDITS)} anchors present and unpatched in {src}")
        return

    for rel, text in planned.items():
        (src / rel).write_text(text)
        print(f"patched {rel}")
    print("\nnow build:  cargo build --release -p nimiq-client")


if __name__ == "__main__":
    main()
