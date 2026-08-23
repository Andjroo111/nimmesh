package com.nimmesh.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * The drift gate.
 *
 * `webui/` is one codebase shared by both apps, so `window.nimmesh` has to mean the same
 * thing on both. Two hand-maintained shim copies would drift within a release, and the
 * symptom would be a feature that silently does nothing on one platform: the page calls a
 * method, the shim does not define it, the call throws, and the page's `try/catch`
 * swallows it. Nothing logs, nothing renders, nobody notices.
 *
 * So this reads the METHOD LIST straight out of `WebHostView.swift` and compares it to
 * the Kotlin one. Add a bridge method on either side and the other platform's build says
 * so, by name.
 *
 * A JVM unit test, not an instrumented one: it reads source, not a device.
 */
class BridgeMethodParityTest {

    @Test
    fun theAndroidShimExposesExactlyWhatTheIosShimDoes() {
        val swift = swiftSource()
        val shim = swift.substringAfter(JS_SHIM_MARKER, "")
        assertTrue(
            "could not find `$JS_SHIM_MARKER` in WebHostView.swift; the parity gate is not " +
                "reading anything and would pass no matter what",
            shim.isNotEmpty(),
        )

        val iosMethods = METHOD_LINE.findAll(shim.substringBefore("})();"))
            .map { it.groupValues[1] }
            .filter { it != "call" }
            .toSortedSet()

        assertTrue(
            "parsed ${iosMethods.size} methods out of the Swift shim, which is too few to " +
                "be the real list; the parser has probably gone stale",
            iosMethods.size > 40,
        )

        val onlyOnIos = iosMethods - BridgeJs.METHODS
        val onlyOnAndroid = BridgeJs.METHODS.toSortedSet() - iosMethods

        assertEquals(
            "these bridge methods exist on iOS but not in BridgeJs.METHODS",
            emptySet<String>(), onlyOnIos,
        )
        assertEquals(
            "these bridge methods exist on Android but not in WebHostView.swift's jsShim",
            emptySet<String>(), onlyOnAndroid,
        )
    }

    @Test
    fun everyPublishedMethodIsReachableFromTheInjectedShim() {
        // METHODS is what the bridge will accept; SHIM is what the page can actually call.
        // A name in one and not the other is a method nobody can reach, or a page call
        // nobody answers.
        val unreachable = BridgeJs.METHODS.filter { !BridgeJs.SHIM.contains("$it: function") }
        assertEquals("published but not in the injected shim", emptyList<String>(), unreachable)
    }

    private fun swiftSource(): String {
        // The test runs with the module dir as its working directory.
        val candidates = listOf(
            File("../../apple/NimmeshApp/Sources/WebHostView.swift"),
            File("../apple/NimmeshApp/Sources/WebHostView.swift"),
        )
        val found = candidates.firstOrNull { it.exists() }
        requireNotNull(found) {
            "WebHostView.swift not found from ${File(".").absolutePath}; the parity gate " +
                "cannot silently skip"
        }
        return found.readText()
    }

    companion object {
        private const val JS_SHIM_MARKER = "static let jsShim"
        private val METHOD_LINE = Regex("""^\s+([a-zA-Z]+): function""", RegexOption.MULTILINE)
    }
}
