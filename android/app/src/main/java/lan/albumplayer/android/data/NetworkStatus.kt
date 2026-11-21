package lan.albumplayer.android.data

import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities

/** Reads the phone's current networks into a [ConnectionErrors.NetworkState]. */
object NetworkStatus {

    fun read(context: Context): ConnectionErrors.NetworkState {
        val cm = context.getSystemService(ConnectivityManager::class.java)
            ?: return ConnectionErrors.NetworkState(wifi = false, cellular = false, vpn = false, validated = false)

        // Every network rather than just the default: with Tailscale up, the
        // default network is the VPN, and its capabilities do not reliably say
        // which physical network it is riding on.
        @Suppress("DEPRECATION")
        val all = cm.allNetworks.mapNotNull { cm.getNetworkCapabilities(it) }
        fun any(transport: Int) = all.any {
            it.hasTransport(transport) && it.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
        }

        val default = cm.activeNetwork?.let { cm.getNetworkCapabilities(it) }
        return ConnectionErrors.NetworkState(
            wifi = any(NetworkCapabilities.TRANSPORT_WIFI),
            cellular = any(NetworkCapabilities.TRANSPORT_CELLULAR),
            vpn = all.any { it.hasTransport(NetworkCapabilities.TRANSPORT_VPN) },
            validated = default?.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true,
        )
    }
}
