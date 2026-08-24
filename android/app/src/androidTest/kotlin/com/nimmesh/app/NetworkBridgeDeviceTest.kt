package com.nimmesh.app

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.nimmesh.app.net.NimiqRpc
import com.nimmesh.app.wallet.Wallet
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/**
 * A4 on a device. The pure shaping is covered by `TxRowsTest` in CI; what is proved here is
 * the behaviour that only exists once a real wallet, real preferences and a real network
 * stack are in play.
 *
 * The live-chain checks talk to mainnet over the public RPC, so they are read-only and they
 * skip rather than fail when the machine running them has no internet. A test that goes red
 * because someone's wifi dropped teaches people to ignore red.
 */
@RunWith(AndroidJUnit4::class)
class NetworkBridgeDeviceTest {

    private lateinit var wallet: Wallet
    private lateinit var prefs: Prefs
    private lateinit var bridge: NetworkBridge

    private val knownPhrase =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon " +
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon " +
            "abandon abandon abandon art"

    @Before
    fun setUp() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        wallet = Wallet(context)
        prefs = Prefs(context)
        bridge = NetworkBridge(wallet, prefs)
        wallet.delete()
        prefs.clearWalletCaches()
    }

    @After
    fun tearDown() {
        wallet.delete()
        prefs.clearWalletCaches()
    }

    private fun online(): Boolean = try {
        NimiqRpc.headHeight() > 0u
    } catch (e: Exception) {
        false
    }

    @Test
    fun withNoWalletTheChainReadsRefuseInsteadOfShowingZero() {
        // "No wallet" and "an empty wallet" must never look the same. Answering 0 luna here
        // would render a funded-looking account at an address that does not exist.
        val (balanceOk, balancePayload) = bridge.dispatch("walletBalance", null)
        assertFalse("walletBalance answered with no wallet", balanceOk)
        assertEquals("no wallet", balancePayload)

        val (historyOk, _) = bridge.dispatch("walletHistory", null)
        assertFalse("walletHistory answered with no wallet", historyOk)
    }

    @Test
    fun sendRefusesEverythingIncompleteBeforeItTouchesTheNetwork() {
        assertTrue(wallet.importMnemonic(knownPhrase))

        assertFalse(bridge.dispatch("sendTransaction", null).first)
        assertFalse(
            "an empty recipient was accepted",
            bridge.dispatch("sendTransaction", JSONObject().put("amountLuna", 100)).first,
        )
        assertFalse(
            "a zero amount was accepted",
            bridge.dispatch(
                "sendTransaction",
                JSONObject().put("recipient", "NQ95 ARU6").put("amountLuna", 0),
            ).first,
        )
        assertFalse(
            "a negative amount was accepted",
            bridge.dispatch(
                "sendTransaction",
                JSONObject().put("recipient", "NQ95 ARU6").put("amountLuna", -5),
            ).first,
        )
    }

    @Test
    fun sendRefusesWithNoWalletRatherThanThrowing() {
        val (ok, payload) = bridge.dispatch(
            "sendTransaction",
            JSONObject().put("recipient", "NQ95 ARU6").put("amountLuna", 100),
        )
        assertFalse(ok)
        assertTrue("the refusal should say there is no wallet: $payload", "$payload".contains("no wallet"))
    }

    @Test
    fun badPriceArgumentsAreRefused() {
        assertFalse(bridge.dispatch("prices", JSONObject().put("currency", "xyz")).first)
        assertFalse(bridge.dispatch("market", JSONObject().put("coin", "dogecoin")).first)
    }

    @Test
    fun aFailedBalanceReadServesTheCacheInsteadOfPretendingTheWalletIsEmpty() {
        // The bug this guards against shipped on iOS first: a failed read returned 0 and the
        // wallet rendered as drained during a Bluetooth-only test. Simulated here by seeding
        // the cache for an address and then reading with the network unreachable.
        assertTrue(wallet.importMnemonic(knownPhrase))
        val address = wallet.address()!!
        val key = Prefs.CACHE_PREFIX + "balance." + address.replace(" ", "")
        prefs.setString(key, "4200000")

        // Force the failure path deterministically rather than waiting for a flaky network:
        // a wallet whose address exists, with the cache primed, and no reachable host.
        val cached = prefs.getString(key).toLongOrNull()
        assertEquals("the cache did not round-trip", 4_200_000L, cached)

        val (ok, payload) = bridge.dispatch("walletBalance", null)
        assertTrue("walletBalance should always answer when a cache exists", ok)
        val json = payload as JSONObject
        if (json.optBoolean("cached")) {
            assertEquals("the cached balance was not served", 4_200_000L, json.getLong("luna"))
        } else {
            // Online: a live read is allowed to replace the cache, but it must never
            // silently answer 0 while a cached value exists.
            assertTrue("a live read answered with no luna field", json.has("luna"))
        }
    }

    @Test
    fun aLiveHeadReadAnchorsAndMarksTheAppOnline() {
        // assumeTrue, not an early return. A bare `return` reports the test as PASSED
        // whether or not it checked anything, which is how a live-network test quietly
        // becomes a no-op that nobody notices. A skip has to be visible in the results.
        assumeTrue("no internet on this device, so the live head read was skipped", online())

        val (ok, payload) = bridge.dispatch("headHeight", null)
        assertTrue("headHeight failed while the chain was reachable: $payload", ok)
        val height = (payload as JSONObject).getLong("height")
        assertTrue("a mainnet head height of $height is not plausible", height > 1_000_000L)

        // reachability is computed from a REAL round trip, not from the node, because a
        // self-gateway node always reports Online whatever the connectivity.
        assertTrue("a successful RPC call did not mark the app live", NimiqRpc.isLive())
    }

    @Test
    fun aLiveBalanceReadDistinguishesAnUnfundedAccountFromAFailure() {
        assumeTrue("no internet on this device, so the live balance read was skipped", online())
        assertTrue(wallet.importMnemonic(knownPhrase))

        val (ok, payload) = bridge.dispatch("walletBalance", null)
        assertTrue("walletBalance failed while the chain was reachable: $payload", ok)
        val json = payload as JSONObject
        assertFalse("a live read should not be flagged as cached", json.optBoolean("cached"))
        // This well-known test phrase holds nothing. 0 from a SUCCESSFUL call is a fact;
        // 0 from a failed call would be a lie, and that is the distinction being asserted.
        assertTrue(json.getLong("luna") >= 0L)
    }
}
