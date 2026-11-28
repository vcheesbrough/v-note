package link.desync.vnote.net

import org.junit.Assert.assertEquals
import org.junit.Test

// When a retry backoff is cut short because the network may be back (#651).
class NetworkReturnTest {
    private var returns = 0
    private val network = NetworkReturn { returns += 1 }

    @Test
    fun aNetworkBecomingAvailableIsAReturn() {
        network.available(1)

        assertEquals(1, returns)
    }

    /** Android reports a new network's capabilities straight after it: one return, not two. */
    @Test
    fun theCapabilitiesReportThatFollowsAvailableIsNotASecondReturn() {
        network.available(1)
        network.capabilitiesChanged(1, isUsable = true)

        assertEquals(1, returns)
    }

    /** A captive portal signed into, or a link that got its internet back. */
    @Test
    fun aNetworkTurningUsableIsAReturn() {
        network.available(1)
        network.capabilitiesChanged(1, isUsable = false)
        network.capabilitiesChanged(1, isUsable = true)

        assertEquals(2, returns)
    }

    /** Capabilities change with every signal-strength update; those are not returns. */
    @Test
    fun capabilityChangesThatDoNotTurnItUsableAreIgnored() {
        network.available(1)
        network.capabilitiesChanged(1, isUsable = true)
        repeat(5) { network.capabilitiesChanged(1, isUsable = true) }
        network.capabilitiesChanged(1, isUsable = false)

        assertEquals(1, returns)
    }

    @Test
    fun aLostNetworkComingBackIsAReturnAgain() {
        network.available(1)
        network.lost(1)
        network.available(1)

        assertEquals(2, returns)
    }
}
