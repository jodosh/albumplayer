package lan.albumplayer.android

import android.content.ComponentName
import android.media.browse.MediaBrowser
import android.media.session.MediaController
import android.media.session.PlaybackState
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import lan.albumplayer.android.playback.PlaybackService
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Pressing "Play a random album" must end in music or in a reason.
 *
 * Silence is the failure this guards against: the car once showed only its
 * own generic error when Tailscale was off, because the explanation never
 * reached it. Android Auto talks to the service through the platform's
 * `MediaBrowser` and reads the platform `PlaybackState`, so that is what this
 * test does too: a Media3 controller sees a different view of the session.
 *
 * Run it with the server reachable and with it not (for example with
 * Tailscale disconnected); either way the outcome must be one of the two.
 * Needs a signed-in app on the device.
 */
@RunWith(AndroidJUnit4::class)
class CarFailureTest {

    private val instrumentation = InstrumentationRegistry.getInstrumentation()

    private fun <T> onMain(block: () -> T): T {
        var result: Result<T>? = null
        instrumentation.runOnMainSync { result = runCatching(block) }
        return result!!.getOrThrow()
    }

    @Test
    fun aRandomAlbumEitherPlaysOrSaysWhy() {
        val context = instrumentation.targetContext
        val connected = CountDownLatch(1)
        val browser = onMain {
            MediaBrowser(
                context,
                ComponentName(context, PlaybackService::class.java),
                object : MediaBrowser.ConnectionCallback() {
                    override fun onConnected() = connected.countDown()
                },
                null,
            ).also { it.connect() }
        }
        assertTrue("could not connect like the car does", connected.await(20, TimeUnit.SECONDS))

        try {
            val controller = onMain { MediaController(context, browser.sessionToken) }
            // Exactly what the car sends when its one entry is tapped.
            onMain { controller.transportControls.playFromMediaId("random_album", null) }

            var state: PlaybackState? = null
            val deadline = System.currentTimeMillis() + 40_000
            while (System.currentTimeMillis() < deadline) {
                state = onMain { controller.playbackState }
                if (state?.state == PlaybackState.STATE_PLAYING ||
                    state?.state == PlaybackState.STATE_ERROR
                ) {
                    break
                }
                Thread.sleep(250)
            }

            val outcome = state?.state
            assertTrue(
                "neither played nor explained why: $state",
                outcome == PlaybackState.STATE_PLAYING || outcome == PlaybackState.STATE_ERROR,
            )
            if (outcome == PlaybackState.STATE_ERROR) {
                val message = state?.errorMessage?.toString()
                assertFalse("the error must carry a message", message.isNullOrBlank())
                android.util.Log.i("CarFailureTest", "car would show: $message")
            }
            onMain { controller.transportControls.stop() }
        } finally {
            onMain { browser.disconnect() }
        }
    }
}
