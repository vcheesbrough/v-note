package link.desync.vnote.net

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Handler
import android.os.Looper

// When to stop waiting out a retry backoff (#651): a network became the
// default one, or the one in use became usable (a captive portal signed into,
// a link that got its internet back). Kept apart from Android's callback so
// the rule is tested on the JVM. Not thread-safe: call it on one thread.
internal class NetworkReturn(
    private val onReturn: () -> Unit,
) {
    // Per network: whether it was last reported usable; null until it says.
    private val usable = hashMapOf<Long, Boolean?>()

    fun available(network: Long) {
        usable[network] = null
        onReturn()
    }

    // Capabilities change often (signal strength, bandwidth); only turning
    // usable is a return. The report that follows `available` is not one: the
    // network's arrival already was.
    fun capabilitiesChanged(
        network: Long,
        isUsable: Boolean,
    ) {
        val known = network in usable
        val was = usable.put(network, isUsable)
        if (isUsable && (!known || was == false)) onReturn()
    }

    fun lost(network: Long) {
        usable.remove(network)
    }
}

// Tells [onReturn], on the main thread, whenever the network may be back.
// Only while the process is alive: syncing in the background is not this
// card's (#651, out of scope).
internal class NetworkWatcher(
    context: Context,
    onReturn: () -> Unit,
) {
    private val connectivity = context.getSystemService(ConnectivityManager::class.java)
    private val main = Handler(Looper.getMainLooper())
    private val returns = NetworkReturn(onReturn)

    private val callback =
        object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                main.post { returns.available(network.networkHandle) }
            }

            override fun onCapabilitiesChanged(
                network: Network,
                capabilities: NetworkCapabilities,
            ) {
                val isUsable =
                    capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) &&
                        capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)
                main.post { returns.capabilitiesChanged(network.networkHandle, isUsable) }
            }

            override fun onLost(network: Network) {
                main.post { returns.lost(network.networkHandle) }
            }
        }

    fun register() {
        connectivity?.registerDefaultNetworkCallback(callback)
    }

    fun unregister() {
        connectivity?.unregisterNetworkCallback(callback)
    }
}
