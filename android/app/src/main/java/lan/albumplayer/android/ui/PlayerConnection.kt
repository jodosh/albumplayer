package lan.albumplayer.android.ui

import android.content.ComponentName
import android.content.Context
import androidx.media3.common.MediaItem
import androidx.media3.common.Player
import androidx.media3.session.MediaController
import androidx.media3.session.SessionToken
import com.google.common.util.concurrent.MoreExecutors
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import lan.albumplayer.android.playback.AlbumQueue
import lan.albumplayer.android.playback.PlaybackService

/** What the interface needs to render the transport. */
data class PlaybackState(
    val isPlaying: Boolean = false,
    val title: String = "",
    val artist: String = "",
    val albumTitle: String = "",
    val albumId: Long? = null,
    val trackId: Long? = null,
    val artworkUri: String? = null,
    val positionMs: Long = 0,
    val durationMs: Long = 0,
    val hasQueue: Boolean = false,
)

/**
 * Binds to the playback service.
 *
 * Everything the interface does goes through a `MediaController`, so the same
 * commands arrive whether they came from a button here, the lock screen, a
 * headset, or the car.
 */
class PlayerConnection(context: Context) {

    private val _state = MutableStateFlow(PlaybackState())
    val state: StateFlow<PlaybackState> = _state

    private var controller: MediaController? = null

    /** Notified when a track finishes, so the play log can be written. */
    var onTrackFinished: ((trackId: Long, msPlayed: Long) -> Unit)? = null
    var onAlbumChanged: ((albumId: Long) -> Unit)? = null

    private var currentTrackId: Long? = null
    private var currentAlbumId: Long? = null

    init {
        val token = SessionToken(context, ComponentName(context, PlaybackService::class.java))
        val future = MediaController.Builder(context, token).buildAsync()
        future.addListener({
            controller = future.get().also { attach(it) }
        }, MoreExecutors.directExecutor())
    }

    private fun attach(controller: MediaController) {
        controller.addListener(object : Player.Listener {
            override fun onEvents(player: Player, events: Player.Events) = publish(player)

            override fun onMediaItemTransition(item: MediaItem?, reason: Int) {
                // Report the outgoing track before adopting the new one.
                currentTrackId?.let { finished ->
                    onTrackFinished?.invoke(finished, lastKnownPosition)
                }
                currentTrackId = item?.mediaId?.toLongOrNull()
                lastKnownPosition = 0

                val albumId = item?.mediaMetadata?.extras?.getLong(AlbumQueue.ALBUM_ID)
                if (albumId != null && albumId != currentAlbumId) {
                    currentAlbumId = albumId
                    onAlbumChanged?.invoke(albumId)
                }
                publish(controller)
            }
        })
        publish(controller)
    }

    /** Furthest position seen, which is what the play log records. */
    private var lastKnownPosition = 0L

    fun refreshPosition() {
        controller?.let { player ->
            lastKnownPosition = maxOf(lastKnownPosition, player.currentPosition)
            publish(player)
        }
    }

    private fun publish(player: Player) {
        val metadata = player.mediaMetadata
        _state.value = PlaybackState(
            isPlaying = player.isPlaying,
            title = metadata.title?.toString().orEmpty(),
            artist = metadata.artist?.toString().orEmpty(),
            albumTitle = metadata.albumTitle?.toString().orEmpty(),
            albumId = metadata.extras?.getLong(AlbumQueue.ALBUM_ID),
            trackId = player.currentMediaItem?.mediaId?.toLongOrNull(),
            artworkUri = metadata.artworkUri?.toString(),
            positionMs = player.currentPosition.coerceAtLeast(0),
            durationMs = player.duration.coerceAtLeast(0),
            hasQueue = player.mediaItemCount > 0,
        )
    }

    fun play(items: List<MediaItem>, startIndex: Int = 0) {
        controller?.run {
            setMediaItems(items, startIndex, 0)
            // Left off deliberately: ExoPlayer's shuffle reorders tracks, and
            // albums are shuffled by permuting the playlist instead.
            shuffleModeEnabled = false
            prepare()
            play()
        }
    }

    fun enqueue(items: List<MediaItem>) {
        controller?.run {
            addMediaItems(items)
            if (!isPlaying && mediaItemCount == items.size) {
                prepare(); play()
            }
        }
    }

    fun togglePlayPause() = controller?.run { if (isPlaying) pause() else play() } ?: Unit
    fun nextTrack() = controller?.seekToNextMediaItem() ?: Unit
    fun previousTrack() = controller?.seekToPreviousMediaItem() ?: Unit
    fun seekTo(ms: Long) = controller?.seekTo(ms) ?: Unit

    /** Separate from next-track, as it is everywhere else in this project. */
    fun nextAlbum() = controller?.let { player ->
        AlbumQueue.nextAlbumStart(player)?.let { player.seekTo(it, 0) }
    } ?: Unit

    fun previousAlbum() = controller?.let { player ->
        AlbumQueue.previousAlbumStart(player)?.let { player.seekTo(it, 0) }
    } ?: Unit

    fun release() {
        controller?.release()
        controller = null
    }
}
