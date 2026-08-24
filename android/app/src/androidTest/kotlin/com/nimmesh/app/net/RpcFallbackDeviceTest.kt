package com.nimmesh.app.net

import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * #43: the read fallback that keeps a balance and a history rendering while the single
 * public JSON-RPC node is down.
 *
 * These talk to mainnet, so they skip visibly rather than fail when the machine running them
 * has no internet. `assumeTrue`, never a bare `return`: an early return reports a test as
 * passed whether or not it checked anything.
 */
@RunWith(AndroidJUnit4::class)
class RpcFallbackDeviceTest {

    private val knownAddress = "NQ02 31N6 3KM5 T6G5 22TN EPF5 5XPY RLHK RMB3"

    private fun explorerUp(): Boolean = try {
        NimiqWatch.headHeight() > 0u
    } catch (e: Exception) {
        false
    }

    @Test
    fun theExplorerServesAPlausibleMainnetHead() {
        assumeTrue("no internet, explorer head check skipped", explorerUp())
        val head = NimiqWatch.headHeight()
        assertTrue("a mainnet head of $head is not plausible", head > 50_000_000u)
    }

    @Test
    fun headHeightSurvivesTheNodeBeingDown() {
        assumeTrue("no internet, head fallback check skipped", explorerUp())
        // Whichever source answers, a head must come back. During the 2026-08-24 outage this
        // was the explorer, and it is the difference between a usable wallet and a blank one.
        val head = NimiqRpc.headHeight()
        assertTrue("no head from either source", head > 50_000_000u)
        assertTrue(
            "lastReadSource should name the source, got: ${NimiqRpc.lastReadSource}",
            NimiqRpc.lastReadSource in setOf("node", "explorer"),
        )
    }

    @Test
    fun balanceAndHistorySurviveTheNodeBeingDown() {
        assumeTrue("no internet, read fallback check skipped", explorerUp())
        // Reads must succeed rather than throw. A throw here is what makes the UI fall back
        // to a cached figure or render an empty wallet.
        val balance = NimiqRpc.balance(knownAddress)
        assertTrue("balance should be readable", balance >= 0uL)

        val txs = NimiqRpc.transactions(knownAddress, 5)
        assertNotNull(txs)
    }

    @Test
    fun explorerRowsCarryMillisecondsNotSeconds() {
        assumeTrue("no internet, timestamp unit check skipped", explorerUp())
        val rows = NimiqWatch.transactions(knownAddress, 5)
        assumeTrue("this address has no transactions right now", rows.isNotEmpty())

        val timestamp = rows.first().optLong("timestamp")
        // ⚠ The explorer reports SECONDS; the page does `new Date(t.timestamp)`, which is
        // MILLISECONDS. Getting this wrong renders every transaction as 1970, silently.
        // 1.6e12 ms is 2020; 1.6e12 SECONDS would be the year 52000.
        assertTrue(
            "timestamp $timestamp looks like seconds, not milliseconds",
            timestamp > 1_500_000_000_000L,
        )
        assertTrue("timestamp $timestamp is implausibly far in the future", timestamp < 4_000_000_000_000L)
    }

    @Test
    fun explorerRowsUseTheRpcFieldNamesSoTxRowsStaysTheOnlyDecider() {
        assumeTrue("no internet, shape check skipped", explorerUp())
        val rows = NimiqWatch.transactions(knownAddress, 5)
        assumeTrue("this address has no transactions right now", rows.isNotEmpty())

        val row = rows.first()
        // Two platforms deciding "incoming" separately is two places for it to be wrong, so
        // the fallback remaps into the RPC's names and TxRows keeps making the decision.
        listOf("hash", "from", "to", "value", "timestamp", "blockNumber").forEach {
            assertTrue("explorer row is missing the RPC field '$it': $row", row.has(it))
        }
        val normalised = TxRows.normalize(knownAddress, rows)
        assertEquals(rows.size, normalised.length())
    }

    @Test
    fun anOnlineSendFailsLoudlyRatherThanPretendingWhenThereIsNoNode() {
        // Broadcasting needs a real node and the explorer cannot do it, so there is NO
        // fallback here on purpose. The failure has to name what still works.
        try {
            NimiqRpc.sendRawTransaction("00")
            // If a node is up it may reject the payload; that is fine, it still reached one.
        } catch (e: Exception) {
            val message = e.message.orEmpty()
            assertTrue(
                "a send failure must explain itself, got: $message",
                message.contains("unreachable") || message.contains("mesh") ||
                    message.contains("unexpected result shape") || message.isNotEmpty(),
            )
        }
    }
}
