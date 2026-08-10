package lan.albumplayer.android.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import lan.albumplayer.android.data.AlbumDetail
import lan.albumplayer.android.data.AlbumSummary
import lan.albumplayer.android.data.LoginRequest
import lan.albumplayer.android.data.PlayRequest
import lan.albumplayer.android.data.Repository
import lan.albumplayer.android.data.SessionRequest
import lan.albumplayer.android.data.Settings
import lan.albumplayer.android.data.Unauthorized
import lan.albumplayer.android.playback.AlbumQueue

data class UiState(
    val signedIn: Boolean = false,
    val loading: Boolean = false,
    val albums: List<AlbumSummary> = emptyList(),
    val openAlbum: AlbumDetail? = null,
    val error: String? = null,
    val server: String = "",
)

class MainViewModel(app: Application) : AndroidViewModel(app) {

    private val settings = Settings(app)
    private var token: String? = null
    private var repository: Repository? = null

    /** The open listening session on the server, so history lands in one place. */
    private var sessionId: Long? = null

    private val _ui = MutableStateFlow(UiState())
    val ui: StateFlow<UiState> = _ui

    val player = PlayerConnection(app).apply {
        onTrackFinished = { trackId, msPlayed -> recordPlay(trackId, msPlayed) }
        onAlbumChanged = { albumId -> startSession(albumId) }
    }

    init {
        viewModelScope.launch {
            val server = settings.currentServer()
            token = settings.currentToken()
            if (server != null) {
                repository = Repository(server) { token }
                _ui.value = _ui.value.copy(server = server)
                if (token != null) {
                    _ui.value = _ui.value.copy(signedIn = true)
                    loadAlbums()
                }
            }
        }
    }

    fun repo(): Repository? = repository

    fun signIn(server: String, password: String) {
        viewModelScope.launch {
            _ui.value = _ui.value.copy(loading = true, error = null)
            try {
                settings.setServer(server)
                val cleaned = server.trim().trimEnd('/')
                val fresh = Repository(cleaned) { token }
                val response = fresh.api.login(LoginRequest(password))
                token = response.token
                settings.setToken(response.token)
                repository = fresh
                _ui.value = _ui.value.copy(signedIn = true, server = cleaned)
                loadAlbums()
            } catch (e: Exception) {
                _ui.value = _ui.value.copy(
                    loading = false,
                    error = when (e) {
                        is Unauthorized -> "Wrong password"
                        else -> e.message ?: "Could not reach the server"
                    },
                )
            }
        }
    }

    fun signOut() {
        viewModelScope.launch {
            settings.clearToken()
            token = null
            _ui.value = UiState(server = _ui.value.server)
        }
    }

    fun loadAlbums(search: String? = null) {
        val repo = repository ?: return
        viewModelScope.launch {
            _ui.value = _ui.value.copy(loading = true, error = null)
            try {
                val albums = repo.api.albums(search = search?.ifBlank { null })
                _ui.value = _ui.value.copy(albums = albums, loading = false)
            } catch (e: Unauthorized) {
                _ui.value = _ui.value.copy(signedIn = false, loading = false)
            } catch (e: Exception) {
                _ui.value = _ui.value.copy(loading = false, error = e.message)
            }
        }
    }

    fun openAlbum(id: Long) {
        val repo = repository ?: return
        viewModelScope.launch {
            try {
                _ui.value = _ui.value.copy(openAlbum = repo.api.album(id))
            } catch (e: Exception) {
                _ui.value = _ui.value.copy(error = e.message)
            }
        }
    }

    fun closeAlbum() {
        _ui.value = _ui.value.copy(openAlbum = null)
    }

    fun playAlbum(album: AlbumDetail, startIndex: Int = 0) {
        val repo = repository ?: return
        viewModelScope.launch {
            // Fetching the cover touches the network, so it happens off the
            // main thread; the bytes ride along in the metadata so surfaces
            // that cannot fetch a URL still show art.
            val items = withContext(Dispatchers.IO) {
                AlbumQueue.mediaItemsWithArtwork(album, repo)
            }
            player.play(items, startIndex)
        }
    }

    fun enqueueAlbum(album: AlbumDetail) {
        val repo = repository ?: return
        viewModelScope.launch {
            val items = withContext(Dispatchers.IO) {
                AlbumQueue.mediaItemsWithArtwork(album, repo)
            }
            player.enqueue(items)
        }
    }

    /** Queue the library with the albums shuffled, never their tracks. */
    fun shuffleAlbums() {
        val repo = repository ?: return
        viewModelScope.launch {
            _ui.value = _ui.value.copy(loading = true)
            try {
                // Fetching every tracklist would be hundreds of requests, so
                // take a generous slice and shuffle that.
                val chosen = _ui.value.albums.shuffled().take(40)
                val detailed = chosen.map { repo.api.album(it.id) }
                // Across many albums the covers would add up past what a Binder
                // transaction carries, so the queue keeps the URI alone here.
                player.play(AlbumQueue.flatten(detailed, repo))
            } catch (e: Exception) {
                _ui.value = _ui.value.copy(error = e.message)
            } finally {
                _ui.value = _ui.value.copy(loading = false)
            }
        }
    }

    private fun startSession(albumId: Long) {
        val repo = repository ?: return
        viewModelScope.launch {
            runCatching {
                sessionId?.let { repo.api.endSession(it) }
                sessionId = repo.api.startSession(SessionRequest(albumId)).sessionId
            }
        }
    }

    private fun recordPlay(trackId: Long, msPlayed: Long) {
        val repo = repository ?: return
        if (msPlayed <= 0) return
        viewModelScope.launch {
            // History is best effort: never interrupt playback for it.
            runCatching { repo.api.recordPlay(PlayRequest(trackId, sessionId, msPlayed)) }
        }
    }

    override fun onCleared() {
        player.release()
        super.onCleared()
    }
}
