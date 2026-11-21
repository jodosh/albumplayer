package lan.albumplayer.android.playback

import android.content.Intent
import android.os.Bundle
import android.util.Log
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.session.CommandButton
import androidx.media3.session.LibraryResult
import androidx.media3.session.MediaLibraryService
import androidx.media3.session.MediaSession
import androidx.media3.session.SessionCommand
import androidx.media3.session.SessionResult
import com.google.common.collect.ImmutableList
import com.google.common.util.concurrent.Futures
import com.google.common.util.concurrent.ListenableFuture
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.guava.future
import kotlinx.coroutines.withContext
import androidx.media3.session.SessionError
import lan.albumplayer.android.R
import lan.albumplayer.android.data.Repository
import lan.albumplayer.android.data.Settings
import lan.albumplayer.android.data.ConnectionErrors
import lan.albumplayer.android.data.ConnectionErrors.Failure
import lan.albumplayer.android.data.NetworkStatus

/**
 * Hosts the player, and serves the browse tree Android Auto reads.
 *
 * A `MediaLibraryService` rather than a plain `MediaSessionService` because the
 * car talks to this service directly — the phone's Activity never runs while
 * driving, so anything the car can reach has to be loadable from here. On the
 * phone the same service earns the lock-screen and Bluetooth controls, and
 * keeps audio alive once the screen goes off.
 *
 * # What the car is allowed to do
 *
 * Deliberately almost nothing: one entry that starts a random album, and skip
 * controls. Choosing a specific record means reading a list of several hundred
 * while driving, which is not a thing to be doing. The interesting decision —
 * *what* to listen to — is left to the phone, at a standstill.
 */
// `sendError` is how a failed load reaches the car's screen; Media3 still marks
// it unstable, though Android Auto has honoured the underlying error state for
// years.
@androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)
class PlaybackService : MediaLibraryService() {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var session: MediaLibrarySession? = null
    private var repository: Repository? = null

    /** Cached album list, so a tap in the car does not wait on a full fetch. */
    private var albumIds: List<Long> = emptyList()

    /**
     * Why the last attempt failed, shown under the car's one browse entry
     * until an attempt succeeds. The car's error message disappears after a
     * few seconds; this is still there at the next red light.
     */
    @Volatile private var lastFailure: String? = null

    override fun onCreate() {
        super.onCreate()

        val player = ExoPlayer.Builder(this)
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setContentType(C.AUDIO_CONTENT_TYPE_MUSIC)
                    .setUsage(C.USAGE_MEDIA)
                    .build(),
                /* handleAudioFocus = */ true,
            )
            .setHandleAudioBecomingNoisy(true)
            .build()

        session = MediaLibrarySession.Builder(this, player, LibraryCallback())
            .setCustomLayout(customLayout())
            .build()

        // Music starting by any route — the phone's own controls included —
        // means a failure shown earlier no longer applies.
        player.addListener(object : Player.Listener {
            override fun onIsPlayingChanged(isPlaying: Boolean) {
                if (isPlaying) session?.setPlaybackException(null)
            }
        })
    }

    /**
     * The extra buttons the car shows.
     *
     * Next-track is a standard transport control the car draws itself;
     * next-album is ours, and is the one that matters when a record is not
     * what you wanted.
     */
    private fun customLayout(): ImmutableList<CommandButton> = ImmutableList.of(
        CommandButton.Builder()
            .setDisplayName("Next album")
            .setIconResId(R.drawable.ic_next_album)
            .setSessionCommand(SessionCommand(COMMAND_NEXT_ALBUM, Bundle.EMPTY))
            .build(),
        CommandButton.Builder()
            .setDisplayName("Random album")
            .setIconResId(R.drawable.ic_shuffle_album)
            .setSessionCommand(SessionCommand(COMMAND_RANDOM_ALBUM, Bundle.EMPTY))
            .build(),
    )

    private fun repository(): Repository? {
        repository?.let { return it }
        val settings = Settings(this)
        // Blocking is acceptable here: this runs on an IO dispatcher inside a
        // future, never on the main thread.
        val server = kotlinx.coroutines.runBlocking { settings.currentServer() } ?: return null
        val token = kotlinx.coroutines.runBlocking { settings.currentToken() } ?: return null
        return Repository(server) { token }.also { repository = it }
    }

    private sealed interface AlbumLoad {
        class Loaded(val items: List<MediaItem>) : AlbumLoad
        class Failed(val failure: Failure, val serverUrl: String) : AlbumLoad
    }

    /**
     * Fetch a random album, already expanded into playable tracks, and tell
     * the car why when that is not possible.
     *
     * Network failures get one retry after a short pause, which is usually
     * enough to ride out the phone switching from Wi-Fi to mobile data as the
     * car leaves the driveway.
     */
    private suspend fun randomAlbumItems(): List<MediaItem>? {
        var result = attemptRandomAlbum()
        if (result is AlbumLoad.Failed && ConnectionErrors.isRetryable(result.failure)) {
            delay(RETRY_DELAY_MS)
            result = attemptRandomAlbum()
        }
        return when (result) {
            is AlbumLoad.Loaded -> {
                withContext(Dispatchers.Main) { session?.setPlaybackException(null) }
                setLastFailure(null)
                result.items
            }

            is AlbumLoad.Failed -> {
                val message = ConnectionErrors.explain(
                    result.failure, NetworkStatus.read(this), result.serverUrl,
                )
                Log.w(TAG, "random album failed: ${result.failure} -> ${message.text}")
                // Media3 reads the player to build what the car sees, and
                // ExoPlayer throws if read off its own thread. Doing this from
                // the IO dispatcher once failed the whole request, so the car
                // showed only its own generic error.
                withContext(Dispatchers.Main) { report(message) }
                setLastFailure(message.text)
                null
            }
        }
    }

    private suspend fun attemptRandomAlbum(): AlbumLoad {
        val repo = repository() ?: return AlbumLoad.Failed(Failure.NotSignedIn, "")
        ConnectionErrors.preflight(NetworkStatus.read(this), repo.baseUrl)?.let {
            return AlbumLoad.Failed(it, repo.baseUrl)
        }
        return try {
            if (albumIds.isEmpty()) albumIds = repo.api.albums().map { it.id }
            val id = albumIds.randomOrNull()
                ?: return AlbumLoad.Failed(Failure.EmptyLibrary, repo.baseUrl)
            val album = repo.api.album(id)
            // Fetched here rather than left to the car: the tiles are drawn by
            // another process which cannot always load the cover URL itself.
            AlbumLoad.Loaded(AlbumQueue.mediaItemsWithArtwork(album, repo))
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (e: Exception) {
            val failure = ConnectionErrors.classify(e)
            // Forget the cached client, so signing in again on the phone takes
            // effect on the next tap rather than at the next service restart.
            if (failure == Failure.SignedOut) repository = null
            AlbumLoad.Failed(failure, repo.baseUrl)
        }
    }

    /**
     * Show a failure in the car. Main thread only.
     *
     * With nothing playing, the player is put into an error state carrying the
     * message: Android Auto shows that on its player screen until it clears,
     * which is what a driver glancing over needs. With an album already
     * playing, that would replace the music with an error screen, so it is a
     * passing notice instead and playback carries on.
     */
    private fun report(message: ConnectionErrors.Message) {
        val session = session ?: return
        val player = session.player
        if (player.mediaItemCount > 0 && player.playWhenReady) {
            session.sendError(SessionError(message.code, message.text))
        } else {
            session.setPlaybackException(
                PlaybackException(message.text, null, playbackErrorCode(message.code))
            )
        }
    }

    /** The player-error code closest to a session-error code. */
    private fun playbackErrorCode(sessionCode: Int): Int = when (sessionCode) {
        SessionError.ERROR_SESSION_AUTHENTICATION_EXPIRED ->
            PlaybackException.ERROR_CODE_AUTHENTICATION_EXPIRED
        SessionError.ERROR_SESSION_SETUP_REQUIRED -> PlaybackException.ERROR_CODE_SETUP_REQUIRED
        SessionError.ERROR_IO -> PlaybackException.ERROR_CODE_IO_NETWORK_CONNECTION_FAILED
        else -> PlaybackException.ERROR_CODE_UNSPECIFIED
    }

    private fun setLastFailure(text: String?) {
        if (lastFailure == text) return
        lastFailure = text
        val session = session ?: return
        android.os.Handler(session.player.applicationLooper).post {
            session.notifyChildrenChanged(ROOT_ID, 1, null)
        }
    }

    /** The current queue, so a failed tap leaves whatever is playing alone. */
    private suspend fun currentQueue(player: Player): MediaSession.MediaItemsWithStartPosition =
        withContext(Dispatchers.Main) {
            MediaSession.MediaItemsWithStartPosition(
                (0 until player.mediaItemCount).map(player::getMediaItemAt),
                player.currentMediaItemIndex,
                player.currentPosition,
            )
        }

    private inner class LibraryCallback : MediaLibrarySession.Callback {

        override fun onConnect(
            session: MediaSession,
            controller: MediaSession.ControllerInfo,
        ): MediaSession.ConnectionResult {
            val available = MediaSession.ConnectionResult.DEFAULT_SESSION_AND_LIBRARY_COMMANDS
                .buildUpon()
                .add(SessionCommand(COMMAND_NEXT_ALBUM, Bundle.EMPTY))
                .add(SessionCommand(COMMAND_PREVIOUS_ALBUM, Bundle.EMPTY))
                .add(SessionCommand(COMMAND_RANDOM_ALBUM, Bundle.EMPTY))
                .build()

            return MediaSession.ConnectionResult.AcceptedResultBuilder(session)
                .setAvailableSessionCommands(available)
                .setCustomLayout(customLayout())
                .build()
        }

        override fun onCustomCommand(
            session: MediaSession,
            controller: MediaSession.ControllerInfo,
            customCommand: SessionCommand,
            args: Bundle,
        ): ListenableFuture<SessionResult> = when (customCommand.customAction) {
            COMMAND_NEXT_ALBUM -> {
                AlbumQueue.nextAlbumStart(session.player)?.let { session.player.seekTo(it, 0) }
                Futures.immediateFuture(SessionResult(SessionResult.RESULT_SUCCESS))
            }

            COMMAND_PREVIOUS_ALBUM -> {
                AlbumQueue.previousAlbumStart(session.player)?.let { session.player.seekTo(it, 0) }
                Futures.immediateFuture(SessionResult(SessionResult.RESULT_SUCCESS))
            }

            COMMAND_RANDOM_ALBUM -> scope.future {
                val items = randomAlbumItems()
                    ?: return@future SessionResult(SessionError.ERROR_IO)
                withMainPlayer(session.player) {
                    setMediaItems(items, 0, 0)
                    prepare()
                    play()
                }
                SessionResult(SessionResult.RESULT_SUCCESS)
            }

            else -> Futures.immediateFuture(SessionResult(SessionError.ERROR_NOT_SUPPORTED))
        }

        override fun onGetLibraryRoot(
            session: MediaLibrarySession,
            browser: MediaSession.ControllerInfo,
            params: LibraryParams?,
        ): ListenableFuture<LibraryResult<MediaItem>> =
            Futures.immediateFuture(LibraryResult.ofItem(browsable(ROOT_ID, "AlbumPlayer"), params))

        /**
         * The whole browse tree: a single entry.
         *
         * Android Auto limits how much can be shown while moving anyway, but
         * the reason here is not the limit — it is that scrolling 653 albums at
         * speed is a bad idea, so the app does not offer it.
         */
        override fun onGetChildren(
            session: MediaLibrarySession,
            browser: MediaSession.ControllerInfo,
            parentId: String,
            page: Int,
            pageSize: Int,
            params: LibraryParams?,
        ): ListenableFuture<LibraryResult<ImmutableList<MediaItem>>> {
            val children = if (parentId == ROOT_ID) {
                ImmutableList.of(randomEntry())
            } else {
                ImmutableList.of()
            }
            return Futures.immediateFuture(LibraryResult.ofItemList(children, params))
        }

        override fun onGetItem(
            session: MediaLibrarySession,
            browser: MediaSession.ControllerInfo,
            mediaId: String,
        ): ListenableFuture<LibraryResult<MediaItem>> = Futures.immediateFuture(
            if (mediaId == RANDOM_ID) {
                LibraryResult.ofItem(randomEntry(), null)
            } else {
                LibraryResult.ofError(SessionError.ERROR_BAD_VALUE)
            }
        )

        /**
         * Turn the placeholder entry into a real album.
         *
         * The car hands back the item it was shown, which carries no audio; it
         * is resolved here into the tracks of a randomly chosen record.
         */
        override fun onSetMediaItems(
            mediaSession: MediaSession,
            controller: MediaSession.ControllerInfo,
            mediaItems: MutableList<MediaItem>,
            startIndex: Int,
            startPositionMs: Long,
        ): ListenableFuture<MediaSession.MediaItemsWithStartPosition> =
            if (mediaItems.size == 1 && mediaItems[0].mediaId == RANDOM_ID) {
                scope.future {
                    // On failure, hand back the current queue: an empty list
                    // here would replace it, and stop the album already
                    // playing because the next one could not be fetched.
                    randomAlbumItems()
                        ?.let { MediaSession.MediaItemsWithStartPosition(it, 0, 0) }
                        ?: currentQueue(mediaSession.player)
                }
            } else {
                Futures.immediateFuture(
                    MediaSession.MediaItemsWithStartPosition(
                        mediaItems, startIndex, startPositionMs
                    )
                )
            }

        override fun onAddMediaItems(
            mediaSession: MediaSession,
            controller: MediaSession.ControllerInfo,
            mediaItems: MutableList<MediaItem>,
        ): ListenableFuture<MutableList<MediaItem>> =
            Futures.immediateFuture(mediaItems)
    }

    /** Player calls must happen on the application thread. */
    private inline fun withMainPlayer(player: Player, crossinline block: Player.() -> Unit) {
        val handler = android.os.Handler(player.applicationLooper)
        handler.post { player.block() }
    }

    private fun browsable(id: String, title: String) = MediaItem.Builder()
        .setMediaId(id)
        .setMediaMetadata(
            MediaMetadata.Builder()
                .setTitle(title)
                .setIsBrowsable(true)
                .setIsPlayable(false)
                .setMediaType(MediaMetadata.MEDIA_TYPE_FOLDER_MIXED)
                .build()
        )
        .build()

    /**
     * The car's one entry. Its subtitle doubles as a status line: after a
     * failure it says why, until the next attempt succeeds.
     */
    private fun randomEntry() = playable(
        RANDOM_ID,
        "Play a random album",
        lastFailure ?: "A record chosen at random",
    )

    private fun playable(id: String, title: String, subtitle: String) = MediaItem.Builder()
        .setMediaId(id)
        .setMediaMetadata(
            MediaMetadata.Builder()
                .setTitle(title)
                .setSubtitle(subtitle)
                .setIsBrowsable(false)
                .setIsPlayable(true)
                .setMediaType(MediaMetadata.MEDIA_TYPE_MUSIC)
                .build()
        )
        .build()

    override fun onGetSession(controllerInfo: MediaSession.ControllerInfo): MediaLibrarySession? =
        session

    override fun onTaskRemoved(rootIntent: Intent?) {
        session?.player?.let { player ->
            if (!player.playWhenReady || player.mediaItemCount == 0) stopSelf()
        }
        super.onTaskRemoved(rootIntent)
    }

    override fun onDestroy() {
        scope.cancel()
        session?.run {
            player.release()
            release()
        }
        session = null
        super.onDestroy()
    }

    companion object {
        private const val TAG = "PlaybackService"
        private const val ROOT_ID = "root"
        private const val RETRY_DELAY_MS = 2_000L
        private const val RANDOM_ID = "random_album"

        const val COMMAND_NEXT_ALBUM = "lan.albumplayer.NEXT_ALBUM"
        const val COMMAND_PREVIOUS_ALBUM = "lan.albumplayer.PREVIOUS_ALBUM"
        const val COMMAND_RANDOM_ALBUM = "lan.albumplayer.RANDOM_ALBUM"
    }
}
