package lan.albumplayer.android.data

import kotlinx.serialization.json.Json
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import retrofit2.Retrofit
import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory
import java.io.IOException
import java.util.concurrent.TimeUnit

/**
 * Raised when the server rejects our credentials.
 *
 * Extends [IOException] deliberately. OkHttp requires an interceptor to throw
 * only IOException; anything else escapes the dispatcher thread as a fatal
 * error and takes the whole app down — so a mistyped password would crash
 * rather than say "wrong password".
 */
class Unauthorized : IOException("unauthorized")

/**
 * Talks to one server.
 *
 * Rebuilt whenever the address changes, since Retrofit fixes its base URL at
 * construction.
 */
class Repository(
    val baseUrl: String,
    private val tokenProvider: () -> String?,
) {
    private val json = Json { ignoreUnknownKeys = true }

    private val client = OkHttpClient.Builder()
        .addInterceptor(Interceptor { chain ->
            val token = tokenProvider()
            val request = if (token != null) {
                chain.request().newBuilder()
                    .header("Authorization", "Bearer $token")
                    .build()
            } else {
                chain.request()
            }
            val response = chain.proceed(request)
            if (response.code == 401) {
                response.close()
                throw Unauthorized()
            }
            response
        })
        // A homelab on the far end of a VPN can be slow to answer; a track is a
        // long read and must not be cut off mid-stream.
        .connectTimeout(15, TimeUnit.SECONDS)
        .readTimeout(60, TimeUnit.SECONDS)
        .build()

    val api: AlbumPlayerApi = Retrofit.Builder()
        .baseUrl("$baseUrl/")
        .client(client)
        .addConverterFactory(json.asConverterFactory("application/json".toMediaType()))
        .build()
        .create(AlbumPlayerApi::class.java)

    /**
     * Media URLs carry the token in the query string.
     *
     * ExoPlayer and the image loader issue their own requests without going
     * through the interceptor above, so the credential has to travel in the URL
     * for those.
     */
    fun coverUrl(albumId: Long): String =
        "$baseUrl/api/albums/$albumId/cover?token=${tokenProvider().orEmpty()}"

    fun streamUrl(trackId: Long): String =
        "$baseUrl/api/tracks/$trackId/stream?token=${tokenProvider().orEmpty()}"
}
