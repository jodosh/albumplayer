package lan.albumplayer.android.playback

import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.Player
import lan.albumplayer.android.data.AlbumDetail
import lan.albumplayer.android.data.Repository

/**
 * Turning albums into an ExoPlayer playlist, album-first.
 *
 * ExoPlayer's own shuffle reorders *tracks*, which is precisely the behaviour
 * this player exists to avoid. It stays switched off; shuffling happens here by
 * permuting whole albums and flattening them in that order, so each record
 * still plays start to finish.
 */
object AlbumQueue {

    /** Extras key marking which album a queue entry belongs to. */
    const val ALBUM_ID = "albumId"

    /**
     * Build the queue entries for one album.
     *
     * `artwork` is the cover as bytes, when it has been fetched. Carrying it in
     * the metadata is what makes art appear on the small Android Auto tiles,
     * which are drawn by a different process that cannot always fetch the URI
     * for itself. Passing null still leaves the URI, which the full-screen
     * player resolves in-process.
     */
    fun mediaItems(
        album: AlbumDetail,
        repository: Repository,
        artwork: ByteArray? = null,
    ): List<MediaItem> =
        album.tracks.map { track ->
            val extras = android.os.Bundle().apply { putLong(ALBUM_ID, album.id) }
            MediaItem.Builder()
                .setMediaId(track.id.toString())
                .setUri(repository.streamUrl(track.id))
                .setMediaMetadata(
                    MediaMetadata.Builder()
                        .setTitle(track.title)
                        .setArtist(track.artist)
                        .setAlbumTitle(album.title)
                        .setAlbumArtist(album.artist)
                        .setTrackNumber(track.trackNo)
                        .setDiscNumber(track.discNo)
                        .setArtworkUri(
                            if (album.hasCover) {
                                android.net.Uri.parse(repository.coverUrl(album.id))
                            } else {
                                null
                            }
                        )
                        .also { builder ->
                            if (artwork != null) {
                                builder.setArtworkData(
                                    artwork,
                                    MediaMetadata.PICTURE_TYPE_FRONT_COVER,
                                )
                            }
                        }
                        .setExtras(extras)
                        .build()
                )
                .build()
        }

    fun flatten(albums: List<AlbumDetail>, repository: Repository): List<MediaItem> =
        albums.flatMap { mediaItems(it, repository) }

    /**
     * As [mediaItems], fetching the cover first so the car's tiles have one.
     *
     * Skipped for very long tracklists: the metadata crosses a Binder
     * transaction with a size limit, and a cover repeated hundreds of times
     * would exceed it and break playback altogether.
     *
     * Blocking: call from a background thread.
     */
    fun mediaItemsWithArtwork(album: AlbumDetail, repository: Repository): List<MediaItem> {
        val artwork = if (album.hasCover && album.tracks.size <= Artwork.MAX_TRACKS_TO_EMBED) {
            Artwork.thumbnail(repository, album.id)
        } else {
            null
        }
        return mediaItems(album, repository, artwork)
    }

    /** Shuffle the albums, never the tracks inside them. */
    fun shuffledAlbums(albums: List<AlbumDetail>): List<AlbumDetail> = albums.shuffled()

    private fun albumIdAt(player: Player, index: Int): Long? =
        player.getMediaItemAt(index).mediaMetadata.extras?.getLong(ALBUM_ID)

    /**
     * Index of the first track of the album after the one playing.
     *
     * Returns null at the last album, so the caller can decide whether that
     * means stop or wrap.
     */
    fun nextAlbumStart(player: Player): Int? {
        val count = player.mediaItemCount
        if (count == 0) return null
        val current = albumIdAt(player, player.currentMediaItemIndex) ?: return null
        for (i in player.currentMediaItemIndex until count) {
            if (albumIdAt(player, i) != current) return i
        }
        return null
    }

    /**
     * Where "previous album" should land.
     *
     * Partway into a record it restarts that record, which is what the control
     * means to someone listening; only at the very start does it step back.
     */
    fun previousAlbumStart(player: Player): Int? {
        val index = player.currentMediaItemIndex
        val current = albumIdAt(player, index) ?: return null

        var start = index
        while (start > 0 && albumIdAt(player, start - 1) == current) start--

        val restarting = index != start || player.currentPosition > RESTART_THRESHOLD_MS
        if (restarting) return start
        if (start == 0) return 0

        val previous = albumIdAt(player, start - 1) ?: return start
        var previousStart = start - 1
        while (previousStart > 0 && albumIdAt(player, previousStart - 1) == previous) previousStart--
        return previousStart
    }

    /** Treat a few seconds in as "still at the start", as every player does. */
    private const val RESTART_THRESHOLD_MS = 3_000
}
