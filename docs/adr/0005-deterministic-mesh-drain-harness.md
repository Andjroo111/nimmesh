# ADR-0005 — Deterministic mesh-drain: fence the workers instead of racing a wall-clock budget

**Status:** accepted (2026-07-02) · **Context:** closes **#84** (re-enable the three multi-node discovery-stress tests that were `#[ignore]`d in CI). The mesh harness (ADR-0002) is deliberately asynchronous: `MeshNode::on_packet_received*` only enqueues (gotcha a), a per-node **worker thread** drains that queue, and the **`MockEther`** delivers each transmit on its own thread. A multi-pair "does it discover and settle?" test therefore depends on two background threads making progress. The original convergence driver (`pump_until`) polled every node then slept on a fixed **wall-clock budget** (`CONVERGE = 10s`), breaking the instant the swap settled. On CI's 2-core runners under `cargo test --all`, the whole suite oversubscribes the CPU, the worker threads starve, and a six-node scenario misses the budget — a **flake, not a bug** (the tests passed with `--ignored` locally). Three tests were parked: `many_complementary_pairs_all_discover_and_settle`, `a_partitioned_pair_discovers_after_the_link_heals_within_budget`, `a_reconnected_peer_resets_the_re_advertise_budget_and_the_pair_settles`.

## Decision

Replace the timed budget with a **fence-driven drain to global quiescence** — no clock in the convergence path. Two test-only FIFO barriers, one per async stage:

- **Node fence** — `MeshNode::fence()` enqueues `Job::Fence(reply)`; the worker (strictly FIFO) replies only after every earlier job has fully run, *including the `radio.send`s those jobs made* (already handed to the ether). Blocking on the reply proves the node's queue is drained.
- **Ether fence** — `MockEther::fence()` sends an `EtherMsg::Fence(reply)` down the same delivery channel; the delivery loop (FIFO) replies only after every transmit ahead of it has been delivered into its destination node's queue.

Global convergence is detected with one monotonic counter, `MockEther::enqueued()` (transmissions ever handed to the ether). The `settle(ether, nodes)` helper runs a pass — fence all nodes → read `enqueued` → fence the ether → fence all nodes — and repeats until a pass moves **zero** new transmissions:

```
loop {
    for n in nodes { n.fence(); }     // flush node queues; their sends land in the ether
    let before = ether.enqueued();
    ether.fence();                     // deliver everything pending → dest queues
    for n in nodes { n.fence(); }     // process those deliveries; new sends → ether
    if ether.enqueued() == before { break; }   // nothing new was produced ⇒ quiescent
}
```

The tick-driven tests wrap this in `drive_until`: poll every node's maintenance tick, `settle`, and break the instant the target predicate holds — a **fixed** round cap (headroom over the re-advertise schedule ticks 1/3/6/11/19 + the 2-tick best-rate window), never a clock.

## Why this is correct (and race-free)

`enqueued` is monotonic and read only between fences. After a node-fence pass, every worker is idle and every send it made is counted; after the ether-fence, every pending transmit has been delivered and its destination job enqueued (which the next node-fence then flushes). A concurrent worker that sends during the ether-fence bumps `enqueued`, so the pass reads `after > before` and loops — it can never *falsely* report quiescence. `before == after` across a full pass therefore means the ether was emptied and no node produced a reply — true global quiescence. The bounds (`MAX_SETTLE_PASSES`, `MAX_DRIVE_ROUNDS`) only convert a hypothetical harness bug into a loud panic instead of a hang; they are not part of the convergence logic.

## Why fences beat "just make the ether synchronous"

The harness **cannot** deliver inline: `test_support::SpyRadio` asserts a relay's `radio.send` never runs *inside* `on_packet_received` (ADR-0002 gotcha a — doing real work on the BLE callback thread would re-enter CoreBluetooth's dispatch queue on-device). The async worker is load-bearing, so the fix drains it deterministically rather than removing it. A bonus: `fence()` **blocks** the test thread on a channel `recv` instead of spinning, so the test yields the CPU to the workers — the exact opposite of the busy-poll that starved them, which is why this stays sub-second even under deliberate CPU oversubscription.

## Consequences

- **Zero production-behaviour change.** `Job::Fence` and `EtherMsg::Fence` are `#[cfg(test)]`; `fence()`/`enqueued()` are `pub(crate)` test helpers. A shipping build sees a single-variant `EtherMsg` (hence the one targeted `#[allow(clippy::infallible_destructuring_match)]` on the delivery match). The only always-compiled addition is `MockEther`'s `enqueued` counter — one relaxed `fetch_add` per transmit on a test/example substrate the real BLE path never touches.
- **The three tests run in CI again** (lib suite: 0 ignored). They pass 10/10 locally and under 6 background CPU burners, sub-second — the flake is structurally gone, not merely retuned.
- **Reusable primitive.** Any future multi-node harness test can `settle`/`drive_until` instead of hand-tuning a sleep budget. The single non-ignored partition test that only asserts a *negative* (`a_partition_outlasting_… stays silent`) keeps its simple bounded-tick loop — there is no convergence to wait for.
- `pump_until`/`CONVERGE` are removed.
