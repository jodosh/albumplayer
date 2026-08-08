package lan.albumplayer.android.data

import retrofit2.http.Body
import retrofit2.http.GET
import retrofit2.http.POST
import retrofit2.http.Path
import retrofit2.http.Query

/**
 * The server's HTTP interface.
 *
 * The phone is a client exactly like the desktop app: the server owns the
 * library and the play history, so listening on one shows up on the other.
 */
interface AlbumPlayerApi {

    @POST("api/auth/login")
    suspend fun login(@Body body: LoginRequest): LoginResponse

    @GET("api/albums")
    suspend fun albums(
        @Query("sort") sort: String = "artist",
        @Query("search") search: String? = null,
        @Query("limit") limit: Int = 5000,
    ): List<AlbumSummary>

    @GET("api/albums/{id}")
    suspend fun album(@Path("id") id: Long): AlbumDetail

    @POST("api/sessions")
    suspend fun startSession(@Body body: SessionRequest): SessionResponse

    @POST("api/sessions/{id}/end")
    suspend fun endSession(@Path("id") id: Long)

    @POST("api/plays")
    suspend fun recordPlay(@Body body: PlayRequest)
}
