package lan.albumplayer.android.data

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class AlbumSummary(
    val id: Long,
    val title: String,
    val artist: String,
    val year: Int? = null,
    @SerialName("track_count") val trackCount: Int = 0,
    @SerialName("disc_count") val discCount: Int = 1,
    @SerialName("duration_ms") val durationMs: Long = 0,
    @SerialName("is_compilation") val isCompilation: Boolean = false,
    @SerialName("play_count") val playCount: Int = 0,
    @SerialName("has_cover") val hasCover: Boolean = false,
)

@Serializable
data class Track(
    val id: Long,
    @SerialName("disc_no") val discNo: Int = 1,
    @SerialName("track_no") val trackNo: Int = 0,
    val title: String,
    val artist: String,
    @SerialName("duration_ms") val durationMs: Long = 0,
    val codec: String? = null,
)

@Serializable
data class AlbumDetail(
    val id: Long,
    val title: String,
    val artist: String,
    val year: Int? = null,
    @SerialName("track_count") val trackCount: Int = 0,
    @SerialName("disc_count") val discCount: Int = 1,
    @SerialName("duration_ms") val durationMs: Long = 0,
    @SerialName("is_compilation") val isCompilation: Boolean = false,
    @SerialName("play_count") val playCount: Int = 0,
    @SerialName("has_cover") val hasCover: Boolean = false,
    /** Album ReplayGain in dB: tagged if the files carry it, measured otherwise. */
    @SerialName("gain_db") val gainDb: Double? = null,
    val peak: Double? = null,
    val tracks: List<Track> = emptyList(),
)

@Serializable
data class LoginRequest(val password: String)

@Serializable
data class LoginResponse(
    val token: String,
    @SerialName("expires_in_secs") val expiresInSecs: Long,
)

@Serializable
data class SessionRequest(@SerialName("album_id") val albumId: Long)

@Serializable
data class SessionResponse(@SerialName("session_id") val sessionId: Long)

@Serializable
data class PlayRequest(
    @SerialName("track_id") val trackId: Long,
    @SerialName("session_id") val sessionId: Long? = null,
    @SerialName("ms_played") val msPlayed: Long,
)
