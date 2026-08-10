package lan.albumplayer.android.playback

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import androidx.collection.LruCache
import lan.albumplayer.android.data.Repository
import java.io.ByteArrayOutputStream
import java.net.HttpURLConnection
import java.net.URL

/**
 * Cover art as *bytes*, for the surfaces that cannot fetch a URL.
 *
 * The full-screen car player renders artwork from `artworkUri`, resolved by
 * Media3 inside this app's process, and works. The small tiles are drawn by the
 * Android Auto process, which fetches that URI itself — a different process,
 * with its own network rules and its own view of the network. A plain-HTTP
 * homelab address is exactly the sort of thing it declines to load, and the art
 * silently disappears.
 *
 * Carrying the image in the metadata removes the question: nothing else has to
 * reach the server.
 *
 * # Why it is downscaled, and why not always
 *
 * Metadata crosses a Binder transaction, which is capped at around a megabyte
 * for everything in flight. A full-size cover repeated across a long tracklist
 * blows through that and playback fails outright — a far worse bug than a
 * missing thumbnail. So the image is shrunk to thumbnail size, and albums with
 * more tracks than [MAX_TRACKS_TO_EMBED] keep the URI alone.
 */
object Artwork {

    /** Enough for a car tile; small enough to repeat across a tracklist. */
    private const val TARGET_PX = 256

    /** JPEG quality for the embedded copy. */
    private const val QUALITY = 80

    /**
     * Above this many tracks, embedding would risk the Binder limit.
     * A 50-track album at roughly 12 KB a cover is about 600 KB.
     */
    const val MAX_TRACKS_TO_EMBED = 50

    /** Covers already fetched, keyed by album. Bounded so it cannot grow forever. */
    private val cache = LruCache<Long, ByteArray>(8 * 1024 * 1024).let { _ ->
        object : LruCache<Long, ByteArray>(8 * 1024 * 1024) {
            override fun sizeOf(key: Long, value: ByteArray) = value.size
        }
    }

    /**
     * Fetch and shrink an album cover. Returns null if there is no art or the
     * download fails — in which case the URI alone still serves the full-screen
     * player.
     *
     * Blocking: call from a background thread.
     */
    fun thumbnail(repository: Repository, albumId: Long): ByteArray? {
        cache.get(albumId)?.let { return it }

        val raw = runCatching { download(repository.coverUrl(albumId)) }.getOrNull() ?: return null
        val shrunk = runCatching { shrink(raw) }.getOrNull() ?: return null
        cache.put(albumId, shrunk)
        return shrunk
    }

    private fun download(url: String): ByteArray? {
        val connection = (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = 10_000
            readTimeout = 15_000
        }
        return try {
            if (connection.responseCode != 200) return null
            connection.inputStream.use { it.readBytes() }
        } finally {
            connection.disconnect()
        }
    }

    /** Decode at a reduced sample size, then re-encode as a small JPEG. */
    private fun shrink(bytes: ByteArray): ByteArray? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)

        val larger = maxOf(bounds.outWidth, bounds.outHeight)
        if (larger <= 0) return null

        // inSampleSize must be a power of two; decoding straight to roughly the
        // target avoids allocating the full-size bitmap at all.
        var sample = 1
        while (larger / (sample * 2) >= TARGET_PX) sample *= 2

        val options = BitmapFactory.Options().apply { inSampleSize = sample }
        val bitmap: Bitmap = BitmapFactory.decodeByteArray(bytes, 0, bytes.size, options)
            ?: return null

        return try {
            ByteArrayOutputStream().use { out ->
                bitmap.compress(Bitmap.CompressFormat.JPEG, QUALITY, out)
                out.toByteArray()
            }
        } finally {
            bitmap.recycle()
        }
    }
}
