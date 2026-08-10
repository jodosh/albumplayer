package lan.albumplayer.android.playback

import android.content.Intent
import android.os.Bundle
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
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
import kotlinx.coroutines.guava.future
import lan.albumplayer.android.R
import lan.albumplayer.android.data.Repository
import lan.albumplayer.android.data.Settings

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
class PlaybackService : MediaLibraryService() {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var session: MediaLibrarySession? = null
    private var repository: Repository? = null

    /** Cached album list, so a tap in the car does not wait on a full fetch. */
    private var albumIds: List<Long> = emptyList()

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

    /** Fetch a random album, already expanded into playable tracks. */
    private suspend fun randomAlbumItems(): List<MediaItem> {
        val repo = repository() ?: return emptyList()
        if (albumIds.isEmpty()) {
            albumIds = runCatching { repo.api.albums().map { it.id } }.getOrDefault(emptyList())
        }
        val id = albumIds.randomOrNull() ?: return emptyList()
        val album = runCatching { repo.api.album(id) }.getOrNull() ?: return emptyList()
        // Fetched here rather than left to the car: the tiles are drawn by
        // another process which cannot always load the cover URL itself.
        return AlbumQueue.mediaItemsWithArtwork(album, repo)
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
                if (items.isNotEmpty()) {
                    withMainPlayer(session.player) {
                        setMediaItems(items, 0, 0)
                        prepare()
                        play()
                    }
                }
                SessionResult(SessionResult.RESULT_SUCCESS)
            }

            else -> Futures.immediateFuture(SessionResult(SessionResult.RESULT_ERROR_NOT_SUPPORTED))
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
                ImmutableList.of(playable(RANDOM_ID, "Play a random album"))
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
                LibraryResult.ofItem(playable(RANDOM_ID, "Play a random album"), null)
            } else {
                LibraryResult.ofError(LibraryResult.RESULT_ERROR_BAD_VALUE)
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
                    val items = randomAlbumItems()
                    MediaSession.MediaItemsWithStartPosition(items, 0, 0)
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

    private fun playable(id: String, title: String) = MediaItem.Builder()
        .setMediaId(id)
        .setMediaMetadata(
            MediaMetadata.Builder()
                .setTitle(title)
                .setSubtitle("A record chosen at random")
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
        private const val ROOT_ID = "root"
        private const val RANDOM_ID = "random_album"

        const val COMMAND_NEXT_ALBUM = "lan.albumplayer.NEXT_ALBUM"
        const val COMMAND_PREVIOUS_ALBUM = "lan.albumplayer.PREVIOUS_ALBUM"
        const val COMMAND_RANDOM_ALBUM = "lan.albumplayer.RANDOM_ALBUM"
    }
}
