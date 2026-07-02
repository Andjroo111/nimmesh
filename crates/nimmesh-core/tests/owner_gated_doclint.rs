//! G56: a doc-lint guarding `docs/swap/OWNER-GATED.md` from silent rot — the docs analogue of the G49
//! demo smoke. The ledger names the exact code seams each gated item plugs into; if one is renamed and
//! the ledger isn't updated, the backlog points at nothing. This asserts every curated seam the ledger
//! cites still EXISTS in the source tree. Std-only, no sockets, deterministic.

use std::fs;
use std::path::{Path, PathBuf};

/// Read a repo file relative to the workspace root (the crate manifest dir is `crates/nimmesh-core`).
fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(format!("{}/../../{}", env!("CARGO_MANIFEST_DIR"), rel))
}

/// Every `.rs` file under the crate's `src/`, concatenated — for substring seam checks (we match the
/// source TEXT, so `#[cfg(...)]`-gated code still counts; this is rot detection, not symbol resolution).
fn all_src() -> String {
    fn collect(dir: &Path, out: &mut String) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push_str(&fs::read_to_string(&p).unwrap_or_default());
                out.push('\n');
            }
        }
    }
    let mut out = String::new();
    collect(&repo_path("crates/nimmesh-core/src"), &mut out);
    out
}

#[test]
fn owner_gated_cites_only_code_seams_that_still_exist() {
    let ledger =
        fs::read_to_string(repo_path("docs/swap/OWNER-GATED.md")).expect("read OWNER-GATED.md");
    let src = all_src();

    // The key code seams the ledger names. Curated (not every backtick) so a false positive can't make
    // this flaky. Each MUST be cited in the ledger AND still present in the crate source.
    let seams = [
        "SwapSigner",
        "MockSigner",
        "build_funding",
        "build_claim",
        "sim_secret",
        "sign_intent_ephemeral",
        "NimiqLeg",
        "BtcEnclaveKey",
        "BitcoinLeg",
        "LegBuildError",
        "SwapEngineHandle",
        "IntentMetrics",
        "handle_intent",
        "initiate_from_intent",
        "setup_scaffolding",
    ];
    for s in seams {
        assert!(
            ledger.contains(s),
            "the curated seam `{s}` is no longer cited in OWNER-GATED.md — update the list or the doc"
        );
        assert!(
            src.contains(s),
            "OWNER-GATED.md cites `{s}`, but it is gone from crates/nimmesh-core/src — the ledger has rotted"
        );
    }
}

#[test]
fn owner_gated_references_files_that_still_exist() {
    let ledger =
        fs::read_to_string(repo_path("docs/swap/OWNER-GATED.md")).expect("read OWNER-GATED.md");

    // Files the ledger points at, by repo-relative path. Each must exist, and the ledger must cite its
    // basename (so the reference can't drift from the path).
    let files = [
        "crates/nimmesh-core/src/swap_signer.rs",
        "crates/nimmesh-core/src/swap_node.rs",
        "crates/nimmesh-core/src/swap_intent.rs",
        "crates/nimmesh-core/src/swap_builder.rs",
        "crates/nimmesh-core/src/swap_btc_leg.rs",
        "crates/nimmesh-core/src/swap_ffi.rs",
        "crates/nimmesh-core/examples/swap_demo_server.rs",
        "webui/swap/intents.html",
    ];
    for f in files {
        assert!(
            repo_path(f).exists(),
            "OWNER-GATED.md references a file that no longer exists: {f}"
        );
        let base = f.rsplit('/').next().unwrap();
        assert!(
            ledger.contains(base),
            "OWNER-GATED.md no longer cites `{base}` — its seam may have moved"
        );
    }
}
