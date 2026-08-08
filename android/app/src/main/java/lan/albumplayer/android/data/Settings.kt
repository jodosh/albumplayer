package lan.albumplayer.android.data

import android.content.Context
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map

private val Context.dataStore by preferencesDataStore(name = "albumplayer")

/**
 * Where the server is and how we authenticate to it.
 *
 * The token is stored plainly in the app's private DataStore. That is the same
 * protection Android gives any app's files, and it is what a session token
 * warrants: it expires, it can be revoked by restarting the server, and it is
 * not the password.
 */
class Settings(private val context: Context) {

    private val serverKey = stringPreferencesKey("server")
    private val tokenKey = stringPreferencesKey("token")

    val server: Flow<String?> = context.dataStore.data.map { it[serverKey] }
    val token: Flow<String?> = context.dataStore.data.map { it[tokenKey] }

    suspend fun currentServer(): String? = server.first()
    suspend fun currentToken(): String? = token.first()

    suspend fun setServer(url: String) {
        // A trailing slash would double up when paths are appended.
        val cleaned = url.trim().trimEnd('/')
        context.dataStore.edit { it[serverKey] = cleaned }
    }

    suspend fun setToken(token: String) {
        context.dataStore.edit { it[tokenKey] = token }
    }

    suspend fun clearToken() {
        context.dataStore.edit { it.remove(tokenKey) }
    }
}
