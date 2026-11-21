package lan.albumplayer.android.data

import androidx.media3.session.SessionError
import java.io.InterruptedIOException
import java.net.URI
import java.net.UnknownHostException

/**
 * Turns a failed request to the server into a sentence someone can act on.
 *
 * The usual causes look identical from the outside: the phone handing over
 * from Wi-Fi to mobile data, Tailscale having dropped, or the server itself
 * being down. Each message therefore says which network the phone is on as
 * well as what failed, so a glance tells you whether to wait, open Tailscale,
 * or give up. The car shows these after a failed tap, and the phone shows them
 * above the library instead of an empty grid.
 *
 * Pure logic with no Android dependencies, so it is unit-tested on the JVM.
 */
@androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)
object ConnectionErrors {

    /** Map an exception from the API client to the [Failure] it represents. */
    fun classify(e: Throwable): Failure = when (e) {
        is Unauthorized -> Failure.SignedOut
        is java.io.IOException -> Failure.Unreachable(e)
        is retrofit2.HttpException -> Failure.HttpStatus(e.code())
        else -> Failure.Unexpected(e)
    }

    /** Why an album could not be loaded. */
    sealed interface Failure {
        /** No server or token saved: the app has never been signed in. */
        data object NotSignedIn : Failure

        /** The server rejected the saved token. */
        data object SignedOut : Failure

        /** The server answered, but has no albums. */
        data object EmptyLibrary : Failure

        /**
         * No HTTP response at all. [cause] is null when the request was never
         * sent because the network state already made the outcome certain.
         */
        data class Unreachable(val cause: Throwable?) : Failure

        /** An HTTP error status, e.g. 502 from `tailscale serve` with the app down. */
        data class HttpStatus(val code: Int) : Failure

        /** Anything else, such as a response that would not parse. */
        data class Unexpected(val cause: Throwable) : Failure
    }

    /**
     * What the phone's networks look like at the moment of failure.
     *
     * Tailscale on Android is a VPN, so [vpn] is the closest thing to "is
     * Tailscale connected" that an app can see without Tailscale's own API.
     */
    data class NetworkState(
        val wifi: Boolean,
        val cellular: Boolean,
        val vpn: Boolean,
        /** Android has confirmed the default network reaches the internet. */
        val validated: Boolean,
    ) {
        val online: Boolean get() = wifi || cellular

        /** Short, glanceable: the phrase a driver needs to tell handover from outage. */
        val label: String
            get() = when {
                wifi -> "on Wi-Fi"
                cellular -> "on mobile data"
                else -> "offline"
            }
    }

    /** A message for the car, with the session error code that best matches it. */
    data class Message(val code: Int, val text: String)

    /**
     * A failure that can be decided without touching the network, or null when
     * a request is worth making.
     *
     * Saves the driver a 15-second connect timeout in the two commonest cases.
     */
    fun preflight(network: NetworkState, serverUrl: String): Failure? = when {
        !network.online -> Failure.Unreachable(null)
        isTailnetAddress(host(serverUrl)) && !network.vpn -> Failure.Unreachable(null)
        else -> null
    }

    /** Worth one more attempt: a network blip, not a definite answer. */
    fun isRetryable(failure: Failure): Boolean =
        (failure is Failure.Unreachable && failure.cause != null) ||
            (failure is Failure.HttpStatus && failure.code in 502..504)

    fun explain(failure: Failure, network: NetworkState, serverUrl: String): Message {
        val where = network.label
        val host = host(serverUrl)
        return when (failure) {
            Failure.NotSignedIn -> Message(
                SessionError.ERROR_SESSION_SETUP_REQUIRED,
                "Not signed in. Open AlbumPlayer on the phone to sign in.",
            )

            Failure.SignedOut -> Message(
                SessionError.ERROR_SESSION_AUTHENTICATION_EXPIRED,
                "Signed out. Sign in again on the phone.",
            )

            Failure.EmptyLibrary -> Message(
                SessionError.ERROR_INVALID_STATE,
                "The library is empty.",
            )

            is Failure.HttpStatus -> Message(
                SessionError.ERROR_IO,
                if (failure.code in 502..504) {
                    // `tailscale serve` answers for the container even when the
                    // app inside it is down, so the network is evidently fine.
                    "Reached home, but AlbumPlayer isn't running (error ${failure.code})."
                } else {
                    "The server returned error ${failure.code}."
                },
            )

            is Failure.Unexpected -> Message(
                SessionError.ERROR_UNKNOWN,
                "Couldn't load an album: the server's reply made no sense.",
            )

            is Failure.Unreachable -> Message(
                SessionError.ERROR_IO,
                unreachable(failure.cause, network, host, where),
            )
        }
    }

    private fun unreachable(
        cause: Throwable?,
        network: NetworkState,
        host: String,
        where: String,
    ): String = when {
        !network.online -> "Phone has no connection."
        isTailnetAddress(host) && !network.vpn -> "Tailscale is off ($where)."
        isTailnetAddress(host) -> "Tailscale is on, but the server isn't answering ($where)."
        isLanAddress(host) && !network.wifi && !network.vpn ->
            "Server is a home Wi-Fi address; on mobile data it needs Tailscale."
        !network.validated -> "Connected, but no internet ($where)."
        cause is UnknownHostException -> "Can't find the server's address ($where)."
        cause is InterruptedIOException -> "The server took too long to answer ($where)."
        else -> "Can't reach the server ($where)."
    }

    fun host(serverUrl: String): String =
        runCatching { URI(serverUrl).host }.getOrNull()?.lowercase()?.trim('[', ']').orEmpty()

    /**
     * Tailscale's MagicDNS names and its address ranges: 100.64.0.0/10 for
     * IPv4 and fd7a:115c:a1e0::/48 for IPv6.
     */
    fun isTailnetAddress(host: String): Boolean {
        if (host.endsWith(".ts.net")) return true
        if (host.startsWith("fd7a:115c:a1e0:")) return true
        val octets = ipv4(host) ?: return false
        return octets[0] == 100 && octets[1] in 64..127
    }

    /** RFC 1918 addresses and the names home routers hand out. */
    fun isLanAddress(host: String): Boolean {
        if (host.endsWith(".local") || host.endsWith(".lan") || host.endsWith(".home.arpa")) {
            return true
        }
        val o = ipv4(host) ?: return false
        return o[0] == 10 ||
            (o[0] == 172 && o[1] in 16..31) ||
            (o[0] == 192 && o[1] == 168)
    }

    private fun ipv4(host: String): List<Int>? {
        val parts = host.split('.')
        if (parts.size != 4) return null
        val octets = parts.map { it.toIntOrNull() ?: return null }
        return octets.takeIf { all -> all.all { it in 0..255 } }
    }
}
