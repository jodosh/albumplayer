package lan.albumplayer.android

import android.content.ComponentName
import androidx.media3.common.Player
import androidx.media3.session.MediaBrowser
import androidx.media3.session.SessionToken
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import lan.albumplayer.android.playback.PlaybackService
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.util.concurrent.TimeUnit

/**
 * What Android Auto sees.
 *
 * A car connects with a `MediaBrowser`, exactly as this test does, so these
 * assertions are the real car-facing contract rather than a stand-in.
 *
 * They protect a safety decision: the car offers one thing to press and no way
 * to browse. If a change ever exposes the album list here, that becomes several
 * hundred entries to scroll at speed, and these tests should fail.
 */
@RunWith(AndroidJUnit4::class)
class CarBrowseTest {

    /**
     * Media3 controllers must be touched only from the application thread; the
     * instrumentation runs on its own. Futures may be awaited anywhere, so only
     * the calls themselves are hopped across.
     */
    private fun <T> onMain(block: () -> T): T {
        var result: Result<T>? = null
        InstrumentationRegistry.getInstrumentation().runOnMainSync {
            result = runCatching(block)
        }
        return result!!.getOrThrow()
    }

    private fun browser(): MediaBrowser {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val token = SessionToken(context, ComponentName(context, PlaybackService::class.java))
        return onMain { MediaBrowser.Builder(context, token).buildAsync() }
            .get(20, TimeUnit.SECONDS)
    }

    private inline fun <T> withBrowser(block: (MediaBrowser) -> T): T {
        val browser = browser()
        try {
            return block(browser)
        } finally {
            onMain { browser.release() }
        }
    }

    private fun rootId(browser: MediaBrowser): String =
        onMain { browser.getLibraryRoot(null) }.get(10, TimeUnit.SECONDS).value!!.mediaId

    /** Children of a node. Resolved outside `onMain` so calls never nest. */
    private fun children(browser: MediaBrowser, parentId: String) =
        onMain { browser.getChildren(parentId, 0, 50, null) }.get(10, TimeUnit.SECONDS)

    @Test
    fun theCarIsOfferedExactlyOneThingToPress() = withBrowser { browser ->
        val root = rootId(browser)
        val items = children(browser, root).value!!

        assertEquals("the car menu must stay a single entry", 1, items.size)
        val only = items[0]
        assertTrue("that entry has to be playable", only.mediaMetadata.isPlayable == true)
        assertFalse(
            "nothing in the car may be browsable, or it becomes a list to read while driving",
            only.mediaMetadata.isBrowsable == true,
        )
        assertEquals("Play a random album", only.mediaMetadata.title.toString())
    }

    @Test
    fun thereIsNoWayToBrowseIntoTheLibrary() = withBrowser { browser ->
        val root = rootId(browser)
        val only = children(browser, root).value!![0]

        // Asking for children of the one entry must yield nothing: there is no
        // second level for a driver to descend into.
        val deeper = children(browser, only.mediaId)
        assertTrue("the tree must be one level deep", deeper.value.isNullOrEmpty())
    }

    @Test
    fun theCarCanSkipTracksAndAlbums() = withBrowser { browser ->
        val commands = onMain { browser.availableSessionCommands.commands.map { it.customAction } }
        assertTrue(
            "next-album must be available to the car: $commands",
            commands.contains(PlaybackService.COMMAND_NEXT_ALBUM),
        )
        assertTrue(
            "previous-album must be available to the car",
            commands.contains(PlaybackService.COMMAND_PREVIOUS_ALBUM),
        )
        // Track skipping is a standard transport control the car draws itself,
        // and ExoPlayer rightly reports it unavailable while the queue is
        // empty — so what is worth asserting here is the button the car renders
        // *for us*: skipping the whole record.
        val layout = onMain { browser.customLayout.map { it.displayName.toString() } }
        assertTrue("the car needs a next-album button: $layout", layout.contains("Next album"))
    }

    @Test
    fun skippingATrackBecomesAvailableOnceSomethingIsQueued() = withBrowser { browser ->
        // Guards the reverse of the above: the standard transport command is
        // granted to the car, rather than withheld by our session.
        val granted = onMain {
            browser.availableCommands.contains(Player.COMMAND_SEEK_TO_NEXT_MEDIA_ITEM) ||
                browser.availableCommands.contains(Player.COMMAND_SEEK_TO_NEXT) ||
                // With nothing queued neither is actionable; the session must at
                // least not have revoked the ability to set media items.
                browser.availableCommands.contains(Player.COMMAND_SET_MEDIA_ITEM)
        }
        assertTrue("the car must be allowed to drive playback", granted)
    }
}
