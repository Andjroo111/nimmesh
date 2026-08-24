package com.nimmesh.app.ble

import com.nimmesh.app.ble.PeerLinks.Role.CENTRAL
import com.nimmesh.app.ble.PeerLinks.Role.PERIPHERAL
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/**
 * A5: the two directed links a single pair of phones forms.
 *
 * This is the part of the radio with a real bug history, and the only part that can be
 * tested without hardware, so it is tested hard. The field failure it guards against:
 * reporting a peer gone when ONE direction flapped while the other was still carrying
 * traffic, which crashed the peer count to zero and made a working mesh look empty.
 */
class PeerLinksTest {

    private lateinit var links: PeerLinks

    @Before
    fun setUp() {
        links = PeerLinks()
    }

    @Test
    fun theFirstLinkAnnouncesThePeerAndTheSecondDoesNot() {
        assertTrue("the first link must announce the peer", links.up("aa", CENTRAL))
        assertFalse("the second link must NOT announce again", links.up("aa", PERIPHERAL))
        assertEquals(1, links.peerCount)
    }

    @Test
    fun aPeerIsOnlyGoneWhenItsLastLinkDrops() {
        links.up("aa", CENTRAL)
        links.up("aa", PERIPHERAL)

        assertFalse(
            "one direction flapping must NOT report the peer gone while the other carries traffic",
            links.down("aa", CENTRAL),
        )
        assertEquals("the peer is still connected", 1, links.peerCount)

        assertTrue("the LAST link dropping reports the peer gone", links.down("aa", PERIPHERAL))
        assertEquals(0, links.peerCount)
    }

    @Test
    fun aRepeatedCallbackForTheSameRoleCannotDoubleCount() {
        // Android re-fires onServicesDiscovered and the subscribe callbacks readily. Without
        // per-role dedup the count would climb and the peer would never be reported gone.
        assertTrue(links.up("aa", CENTRAL))
        assertFalse(links.up("aa", CENTRAL))
        assertFalse(links.up("aa", CENTRAL))
        assertEquals(1, links.peerCount)

        assertTrue("one down should still clear it", links.down("aa", CENTRAL))
        assertEquals(0, links.peerCount)
    }

    @Test
    fun aDownForARoleThatWasNeverUpIsIgnored() {
        assertFalse("an unmatched down must not report anything", links.down("aa", CENTRAL))
        links.up("aa", CENTRAL)
        assertFalse(
            "a down for the OTHER role must not drop the peer",
            links.down("aa", PERIPHERAL),
        )
        assertEquals("the real link is still up", 1, links.peerCount)
    }

    @Test
    fun aPeerCanReconnectAfterFullyDropping() {
        links.up("aa", CENTRAL)
        assertTrue(links.down("aa", CENTRAL))
        // Walking out of range and back is the normal case, not an edge one.
        assertTrue("a returning peer must be announced again", links.up("aa", CENTRAL))
        assertEquals(1, links.peerCount)
    }

    @Test
    fun peersAreTrackedIndependently() {
        assertTrue(links.up("aa", CENTRAL))
        assertTrue(links.up("bb", CENTRAL))
        assertEquals(2, links.peerCount)

        assertTrue(links.down("aa", CENTRAL))
        assertEquals("dropping one peer must not affect the other", 1, links.peerCount)
        assertEquals(setOf("bb"), links.liveIds)
    }

    @Test
    fun liveIdsIsWhatANewNodeGetsReplayed() {
        // The link table lives on the RADIO, which outlives any node. A node installed after
        // a peer linked would never hear onPeerConnected and would sit at zero peers
        // forever, which is exactly the 2026-07-14 field bug on iOS.
        links.up("aa", CENTRAL)
        links.up("bb", PERIPHERAL)
        links.up("bb", CENTRAL)
        assertEquals(setOf("aa", "bb"), links.liveIds)
        assertEquals("a peer with two links is still ONE peer to replay", 2, links.liveIds.size)
    }

    @Test
    fun clearReportsEveryPeerExactlyOnce() {
        links.up("aa", CENTRAL)
        links.up("aa", PERIPHERAL)
        links.up("bb", CENTRAL)

        // Bluetooth being switched off drops everything at once.
        assertEquals(setOf("aa", "bb"), links.clear())
        assertEquals(0, links.peerCount)
        assertEquals(emptySet<String>(), links.liveIds)

        // And the role sets went with it, so a stale down cannot resurrect a count.
        assertFalse(links.down("aa", CENTRAL))
        assertTrue("after a clear, a peer links as if new", links.up("aa", CENTRAL))
    }
}
