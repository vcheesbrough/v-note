package link.desync.vnote.net

import java.io.IOException

private val IPV4_LOOPBACK = Regex("""127\.\d{1,3}\.\d{1,3}\.\d{1,3}""")
private val IPV6_LOOPBACK = setOf("::1", "0:0:0:0:0:0:0:1")

// Whether [host] names this device, decided from the text alone: no DNS
// lookup, so checking a host never itself touches the network. `localhost`,
// 127.0.0.0/8 and ::1 (with or without brackets). The emulator's 10.0.2.2 is
// the host machine, not loopback.
fun isLoopbackHost(host: String): Boolean {
    val bare =
        host
            .trim()
            .removePrefix("[")
            .removeSuffix("]")
            .lowercase()
    return bare == "localhost" || IPV4_LOOPBACK.matches(bare) || bare in IPV6_LOOPBACK
}

// A request the guard refused. An [IOException], so the caller sees an
// ordinary network failure and nothing is half-sent; the loud part is
// [LoopbackOnlyGuard]'s `onViolation`.
class NonLoopbackHostException(
    val host: String,
    url: String,
) : IOException("request to non-loopback host '$host' refused by the test network guard (#464): $url")

// The guard instrumented tests run under (#464): loopback only. Every refusal
// is handed to [onViolation] before it is thrown, because an [IOException] on
// its own is swallowed by design in places (telemetry), and a refusal nobody
// hears about would hide the bug it exists to expose.
class LoopbackOnlyGuard(
    private val onViolation: (NonLoopbackHostException) -> Unit,
) : HostGuard {
    // Set only by a measurement fixture that deliberately targets a real
    // environment (`DensePageSeeder`), never by a test.
    @Volatile
    var allowAnyHost: Boolean = false

    override fun check(
        host: String,
        url: String,
    ) {
        if (allowAnyHost || isLoopbackHost(host)) return
        val refusal = NonLoopbackHostException(host, url)
        onViolation(refusal)
        throw refusal
    }
}
