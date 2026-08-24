package com.nimmesh.app.ble

/**
 * Reference-counts the TWO directed BLE links a single pair of phones forms.
 *
 * Every node runs both roles at once, so A connects to B's GATT server AND B connects to
 * A's. That is one peer with two links, and the mesh must hear about it exactly once.
 *
 * This is where the iOS radio was bitten in the field. Reporting a peer gone when one
 * direction flapped, while the other was still carrying traffic, crashed the peer count to
 * zero and the mesh looked empty while it was working. So:
 *
 *  - `onPeerConnected` fires only on the FIRST link up.
 *  - `onPeerDisconnected` fires only when the LAST link drops.
 *  - Each ROLE is deduplicated separately, so a re-fired callback (Android repeats
 *    `onServicesDiscovered` and subscribe callbacks readily) cannot double-count.
 *
 * The second lesson, also from the field: the link table lives on the RADIO, which outlives
 * any single node. A node installed after a peer has already linked would never hear
 * `onPeerConnected` and would sit at zero peers forever. [liveIds] exists so a new node can
 * be told what is already up.
 *
 * Pure and free of Android on purpose: this is the part with a bug history, and it is worth
 * having covered by a test that runs in CI rather than only on hardware nobody has yet.
 *
 * Not thread-safe by design. The radio serialises everything onto one worker, and adding a
 * lock here would hide the fact that callers must.
 */
class PeerLinks {

    /** Which directed link a callback refers to. */
    enum class Role {
        /** We hold a GATT client connection to their server. */
        CENTRAL,

        /** They subscribed to our GATT server's characteristic. */
        PERIPHERAL,
    }

    private val counts = HashMap<String, Int>()
    private val central = HashSet<String>()
    private val peripheral = HashSet<String>()

    /** Peers with at least one live link. A new node is replayed these. */
    val liveIds: Set<String> get() = counts.keys.toSet()

    val peerCount: Int get() = counts.size

    /** @return true if this is the peer's FIRST link, meaning the mesh should be told. */
    fun up(peerId: String, role: Role): Boolean {
        if (!set(role).add(peerId)) return false // already counted for this role
        val next = (counts[peerId] ?: 0) + 1
        counts[peerId] = next
        return next == 1
    }

    /** @return true if that was the peer's LAST link, meaning the mesh should be told. */
    fun down(peerId: String, role: Role): Boolean {
        if (!set(role).remove(peerId)) return false // was not counted for this role
        val current = counts[peerId] ?: return false
        return if (current <= 1) {
            counts.remove(peerId)
            true
        } else {
            counts[peerId] = current - 1
            false
        }
    }

    /** Everything drops, e.g. Bluetooth turned off. @return the peers the mesh must be told about. */
    fun clear(): Set<String> {
        val dropped = counts.keys.toSet()
        counts.clear()
        central.clear()
        peripheral.clear()
        return dropped
    }

    private fun set(role: Role): HashSet<String> =
        if (role == Role.CENTRAL) central else peripheral
}
