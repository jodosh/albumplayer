package lan.albumplayer.android.data

import lan.albumplayer.android.data.ConnectionErrors.Failure
import lan.albumplayer.android.data.ConnectionErrors.NetworkState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.SocketTimeoutException
import java.net.UnknownHostException

class ConnectionErrorsTest {

    private val tailnet = "https://albumplayer.example-tailnet.ts.net"
    private val tailnetIp = "http://100.101.102.103:8080"
    private val lan = "http://192.168.1.20:8080"

    private fun net(wifi: Boolean = false, cellular: Boolean = false, vpn: Boolean = false, validated: Boolean = true) =
        NetworkState(wifi = wifi, cellular = cellular, vpn = vpn, validated = validated)

    private fun text(failure: Failure, network: NetworkState, url: String) =
        ConnectionErrors.explain(failure, network, url).text

    @Test
    fun `tailscale off says so and names the network`() {
        val unreachable = Failure.Unreachable(null)
        assertEquals("Tailscale is off (on mobile data).", text(unreachable, net(cellular = true), tailnet))
        assertEquals("Tailscale is off (on Wi-Fi).", text(unreachable, net(wifi = true), tailnetIp))
    }

    @Test
    fun `tailscale on but server silent blames the server, not the vpn`() {
        val timeout = Failure.Unreachable(SocketTimeoutException())
        assertEquals(
            "Tailscale is on, but the server isn't answering (on mobile data).",
            text(timeout, net(cellular = true, vpn = true), tailnet),
        )
    }

    @Test
    fun `no network at all`() {
        assertEquals("Phone has no connection.", text(Failure.Unreachable(null), net(), tailnet))
    }

    @Test
    fun `lan address on mobile data explains why it cannot work`() {
        assertEquals(
            "Server is a home Wi-Fi address; on mobile data it needs Tailscale.",
            text(Failure.Unreachable(SocketTimeoutException()), net(cellular = true), lan),
        )
    }

    @Test
    fun `lan address on wifi falls through to the cause`() {
        assertEquals(
            "The server took too long to answer (on Wi-Fi).",
            text(Failure.Unreachable(SocketTimeoutException()), net(wifi = true), lan),
        )
        assertEquals(
            "Can't find the server's address (on Wi-Fi).",
            text(Failure.Unreachable(UnknownHostException()), net(wifi = true), "http://nas.example.com"),
        )
    }

    @Test
    fun `bad gateway means the network worked`() {
        assertEquals(
            "Reached home, but AlbumPlayer isn't running (error 502).",
            text(Failure.HttpStatus(502), net(cellular = true, vpn = true), tailnet),
        )
    }

    @Test
    fun `preflight skips the request only when the outcome is certain`() {
        assertNotNull(ConnectionErrors.preflight(net(), lan))
        assertNotNull(ConnectionErrors.preflight(net(cellular = true), tailnet))
        assertNull(ConnectionErrors.preflight(net(cellular = true, vpn = true), tailnet))
        assertNull(ConnectionErrors.preflight(net(wifi = true), lan))
    }

    @Test
    fun `retries network blips but not definite answers`() {
        assertTrue(ConnectionErrors.isRetryable(Failure.Unreachable(SocketTimeoutException())))
        assertTrue(ConnectionErrors.isRetryable(Failure.HttpStatus(503)))
        assertFalse(ConnectionErrors.isRetryable(Failure.Unreachable(null)))
        assertFalse(ConnectionErrors.isRetryable(Failure.SignedOut))
        assertFalse(ConnectionErrors.isRetryable(Failure.HttpStatus(404)))
    }

    @Test
    fun `exceptions from the api client map to failures`() {
        assertEquals(Failure.SignedOut, ConnectionErrors.classify(Unauthorized()))
        assertTrue(ConnectionErrors.classify(UnknownHostException()) is Failure.Unreachable)
        assertTrue(ConnectionErrors.classify(IllegalStateException()) is Failure.Unexpected)
    }

    @Test
    fun `address classification`() {
        assertTrue(ConnectionErrors.isTailnetAddress("100.64.0.1"))
        assertTrue(ConnectionErrors.isTailnetAddress("100.127.255.254"))
        assertFalse(ConnectionErrors.isTailnetAddress("100.128.0.1"))
        assertTrue(ConnectionErrors.isTailnetAddress(ConnectionErrors.host("http://[fd7a:115c:a1e0::1]:8080")))
        assertTrue(ConnectionErrors.isLanAddress("172.20.0.5"))
        assertFalse(ConnectionErrors.isLanAddress("172.32.0.5"))
        assertTrue(ConnectionErrors.isLanAddress("nas.local"))
        assertFalse(ConnectionErrors.isLanAddress("8.8.8.8"))
    }
}
