package com.nimmesh.app.net

import org.junit.Assert.assertThrows
import org.junit.Test

/**
 * The price proxy whitelists its coin and currency.
 *
 * On Android the page is served from a real https origin, so it could call CoinGecko
 * itself; the proxy exists for parity with iOS, where `file://` blocks `fetch()`. That
 * makes the whitelist the proxy's own justification: without it, the page could ask the
 * native side to fetch an arbitrary URL.
 *
 * These cases reject BEFORE any request is made, which is why they can be asserted with no
 * network.
 */
class CoinGeckoWhitelistTest {

    @Test
    fun anUnknownCurrencyIsRefused() {
        assertThrows(CoinGecko.BadRequest::class.java) { CoinGecko.prices("xyz") }
        assertThrows(CoinGecko.BadRequest::class.java) { CoinGecko.prices("") }
    }

    @Test
    fun anUnknownCoinIsRefused() {
        assertThrows(CoinGecko.BadRequest::class.java) { CoinGecko.market("dogecoin", "usd") }
        assertThrows(CoinGecko.BadRequest::class.java) { CoinGecko.market("", "usd") }
    }

    @Test
    fun anInjectedPathIsRefusedRatherThanConcatenated() {
        // The coin goes straight into the URL path, so anything not on the list must be
        // refused rather than escaped-and-hoped.
        assertThrows(CoinGecko.BadRequest::class.java) {
            CoinGecko.market("nimiq-2/../../evil", "usd")
        }
        assertThrows(CoinGecko.BadRequest::class.java) {
            CoinGecko.prices("usd&ids=anything")
        }
    }
}
