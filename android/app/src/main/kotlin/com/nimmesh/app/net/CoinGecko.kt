package com.nimmesh.app.net

import org.json.JSONArray
import org.json.JSONObject

/**
 * The fiat price proxy, ported from the `prices` and `market` bridge cases.
 *
 * ⚠ On iOS this proxy is NECESSARY: the page runs on a `file://` origin where WKWebView
 * blocks `fetch()` to the network outright. On Android the page is served over
 * `https://appassets.androidplatform.net`, a real origin, so it could call CoinGecko
 * itself. The proxy is kept anyway so `webui/` stays one codebase, and it earns its place
 * regardless: the coin and currency are whitelisted here, so the page cannot be talked into
 * fetching an arbitrary URL through the native side.
 */
object CoinGecko {

    private const val BASE = "https://api.coingecko.com/api/v3"
    private val CURRENCIES = setOf("usd", "mxn", "eur", "brl")
    private val COINS = setOf("nimiq-2", "bitcoin")

    class BadRequest(message: String) : IllegalArgumentException(message)

    /** Spot prices for the three assets the home screen shows. */
    fun prices(currency: String): JSONObject {
        val vs = currency.lowercase()
        if (vs !in CURRENCIES) throw BadRequest("bad currency")
        val json = Http.getJson("$BASE/simple/price?ids=nimiq-2,bitcoin,usd-coin&vs_currencies=$vs")
        fun price(id: String): Any =
            json.optJSONObject(id)?.let { with(Http) { it.optDoubleOrNull(vs) } } ?: JSONObject.NULL
        return JSONObject()
            .put("nim", price("nimiq-2"))
            .put("btc", price("bitcoin"))
            .put("usdc", price("usd-coin"))
    }

    /** The 24h series behind a card's sparkline. Only the price series crosses the bridge. */
    fun market(coin: String, currency: String): JSONObject {
        val vs = currency.lowercase()
        if (coin !in COINS || vs !in CURRENCIES) throw BadRequest("bad coin or currency")
        val json = Http.getJson("$BASE/coins/$coin/market_chart?vs_currency=$vs&days=1")
        val raw = json.optJSONArray("prices") ?: throw BadRequest("bad response")
        val series = JSONArray()
        for (i in 0 until raw.length()) {
            val row = raw.optJSONArray(i) ?: continue
            if (row.length() > 1) series.put(row.optDouble(1))
        }
        return JSONObject().put("prices", series)
    }
}
