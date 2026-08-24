package com.nimmesh.app.net

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * A4: the transaction rows the page renders.
 *
 * Every assertion here is about something a user would SEE being wrong: a received payment
 * labelled as sent, a pending payment shown as confirmed, or the wrong party named. None
 * of those crash, and none would be caught by a test that only checked the request went
 * out.
 */
class TxRowsTest {

    private val self = "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6"
    private val other = "NQ37 7AAH 9YEK QJUD 5GLH KASD R8RG QUH5 7HNQ"

    private fun tx(
        from: String = other,
        to: String = self,
        value: Long = 1_000L,
        hash: String = "abc",
        blockNumber: Long? = 100L,
        timestamp: Double = 1_700_000_000.0,
    ) = JSONObject()
        .put("hash", hash)
        .put("from", from)
        .put("to", to)
        .put("value", value)
        .put("timestamp", timestamp)
        .apply { if (blockNumber != null) put("blockNumber", blockNumber) }

    @Test
    fun aTransactionToUsIsIncomingAndNamesTheSender() {
        val row = TxRows.normalize(self, listOf(tx(from = other, to = self))).getJSONObject(0)
        assertTrue("a payment TO us was labelled as sent", row.getBoolean("incoming"))
        assertEquals("the counterparty should be the sender", other, row.getString("counterparty"))
    }

    @Test
    fun aTransactionFromUsIsOutgoingAndNamesTheRecipient() {
        val row = TxRows.normalize(self, listOf(tx(from = self, to = other))).getJSONObject(0)
        assertFalse("a payment FROM us was labelled as received", row.getBoolean("incoming"))
        assertEquals("the counterparty should be the recipient", other, row.getString("counterparty"))
    }

    @Test
    fun directionSurvivesSpacingAndCaseDifferences() {
        // The RPC returns addresses unspaced; the wallet displays them in groups. Comparing
        // them raw would label every RECEIVED payment as sent.
        val unspaced = self.replace(" ", "")
        assertTrue(
            "an unspaced address broke the direction check",
            TxRows.normalize(self, listOf(tx(to = unspaced))).getJSONObject(0).getBoolean("incoming"),
        )
        assertTrue(
            "a lowercase address broke the direction check",
            TxRows.normalize(self, listOf(tx(to = self.lowercase()))).getJSONObject(0).getBoolean("incoming"),
        )
        assertTrue(
            "a spaced wallet address broke the direction check",
            TxRows.normalize(unspaced, listOf(tx(to = self))).getJSONObject(0).getBoolean("incoming"),
        )
    }

    @Test
    fun aPendingTransactionIsNotConfirmed() {
        // blockNumber is absent until inclusion. Defaulting to confirmed would tell a user
        // their payment landed when it is still in the mempool.
        assertFalse(
            "a transaction with no blockNumber was shown as confirmed",
            TxRows.normalize(self, listOf(tx(blockNumber = null))).getJSONObject(0).getBoolean("confirmed"),
        )
        assertFalse(
            "blockNumber 0 was shown as confirmed",
            TxRows.normalize(self, listOf(tx(blockNumber = 0L))).getJSONObject(0).getBoolean("confirmed"),
        )
        assertTrue(
            TxRows.normalize(self, listOf(tx(blockNumber = 1L))).getJSONObject(0).getBoolean("confirmed"),
        )
    }

    @Test
    fun valueAndOrderAreCarriedThroughUnchanged() {
        val rows = TxRows.normalize(
            self,
            listOf(tx(hash = "newest", value = 3), tx(hash = "older", value = 2)),
        )
        assertEquals(2, rows.length())
        assertEquals("newest first order was not preserved", "newest", rows.getJSONObject(0).getString("hash"))
        assertEquals(3L, rows.getJSONObject(0).getLong("valueLuna"))
    }

    @Test
    fun anEmptyHistoryIsAnEmptyArrayNotAnError() {
        assertEquals(0, TxRows.normalize(self, emptyList()).length())
    }
}
