package com.nimmesh.app.net

import org.json.JSONArray
import org.json.JSONObject
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/**
 * The whole HTTP layer: `HttpURLConnection`, no dependency.
 *
 * The iOS side uses `URLSession`, the platform's own client, so the honest Android twin is
 * the platform's own client too. OkHttp was tried and dropped: `okhttp-android` 5.5 demands
 * compileSdk 37, which does not exist yet, and nothing here needs more than a POST and a GET.
 *
 * Every call is BLOCKING and must run off the main thread. The bridge already dispatches on
 * its own executor, which is where these run.
 */
object Http {

    private const val CONNECT_TIMEOUT_MS = 15_000
    private const val READ_TIMEOUT_MS = 20_000

    class HttpException(message: String) : IOException(message)

    fun getJson(url: String): JSONObject = JSONObject(get(url))

    fun get(url: String): String = open(url, "GET", null)

    fun postJson(url: String, body: JSONObject): JSONObject =
        JSONObject(open(url, "POST", body.toString()))

    private fun open(url: String, method: String, body: String?): String {
        val connection = (URL(url).openConnection() as HttpURLConnection).apply {
            requestMethod = method
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("Accept", "application/json")
            if (body != null) {
                doOutput = true
                setRequestProperty("Content-Type", "application/json")
            }
        }
        try {
            if (body != null) {
                connection.outputStream.use { it.write(body.toByteArray(Charsets.UTF_8)) }
            }
            val code = connection.responseCode
            val stream = if (code in 200..299) connection.inputStream else connection.errorStream
            val text = stream?.bufferedReader()?.use { it.readText() }.orEmpty()
            if (code !in 200..299) throw HttpException("HTTP $code: ${text.take(200)}")
            return text
        } finally {
            connection.disconnect()
        }
    }

    /** JSONObject.optDouble returns NaN for a missing key; JSON has no NaN, so map it to null. */
    fun JSONObject.optDoubleOrNull(key: String): Double? {
        val v = optDouble(key, Double.NaN)
        return if (v.isNaN()) null else v
    }

    fun JSONArray.toObjectList(): List<JSONObject> =
        (0 until length()).mapNotNull { optJSONObject(it) }
}
