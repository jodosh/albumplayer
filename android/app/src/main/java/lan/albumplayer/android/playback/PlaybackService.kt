package lan.albumplayer.android.playback

import android.content.Intent
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.session.MediaSession
import androidx.media3.session.MediaSessionService

/**
 * Hosts the player so audio survives the app leaving the foreground.
 *
 * A `MediaSessionService` is what earns the lock-screen and notification
 * controls, the Bluetooth and headset buttons, and Android Auto. Without it,
 * Android suspends the process seconds after the screen goes off and the music
 * stops — which is the difference between a demo and something you would
 * actually listen to.
 */
class PlaybackService : MediaSessionService() {

    private var session: MediaSession? = null

    override fun onCreate() {
        super.onCreate()

        val player = ExoPlayer.Builder(this)
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setContentType(C.AUDIO_CONTENT_TYPE_MUSIC)
                    .setUsage(C.USAGE_MEDIA)
                    .build(),
                // Let the system duck and pause us for calls and other apps.
                /* handleAudioFocus = */ true,
            )
            .setHandleAudioBecomingNoisy(true)
            // ExoPlayer crossfades nothing and gaplessly joins consecutive
            // items by default, which is exactly what an album needs.
            .build()

        session = MediaSession.Builder(this, player).build()
    }

    override fun onGetSession(controllerInfo: MediaSession.ControllerInfo): MediaSession? = session

    override fun onTaskRemoved(rootIntent: Intent?) {
        // Swiping the app away should stop the music rather than leave a
        // notification playing to nobody.
        session?.player?.let { player ->
            if (!player.playWhenReady || player.mediaItemCount == 0) {
                stopSelf()
            }
        }
        super.onTaskRemoved(rootIntent)
    }

    override fun onDestroy() {
        session?.run {
            player.release()
            release()
        }
        session = null
        super.onDestroy()
    }
}
