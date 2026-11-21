package lan.albumplayer.android.ui

import android.app.Application
import android.net.ConnectivityManager
import android.net.Network
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import lan.albumplayer.android.data.AlbumDetail
import lan.albumplayer.android.data.AlbumSummary
import lan.albumplayer.android.data.ConnectionErrors
import lan.albumplayer.android.data.NetworkStatus
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

    /** The search the grid is showing, so a retry repeats it. */
    private var lastSearch: String? = null

    /**
     * Re-checks the connection whenever the phone's default network changes.
     *
     * Tailscale coming up or going down replaces the default network, so this
     * is what makes the banner appear the moment the tunnel drops — rather than
     * when you next tap something — and clears it, by retrying, once it is back.
     */
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = recheck()
        override fun onLost(network: Network) = recheck()
    }

    private fun recheck() {
        viewModelScope.launch {
            if (!_ui.value.signedIn) return@launch
            val repo = repository ?: return@launch
            val blocked = ConnectionErrors.preflight(NetworkStatus.read(getApplication()), repo.baseUrl)
            when {
                blocked != null -> _ui.value = _ui.value.copy(error = explain(blocked, repo.baseUrl))
                _ui.value.error != null -> retry()
            }
        }
    }
    private val connectivity = app.getSystemService(ConnectivityManager::class.java)

    private val _ui = MutableStateFlow(UiState())
    val ui: StateFlow<UiState> = _ui

    val player = PlayerConnection(app).apply {
        onTrackFinished = { trackId, msPlayed -> recordPlay(trackId, msPlayed) }
        onAlbumChanged = { albumId -> startSession(albumId) }
    }

    init {
        runCatching { connectivity?.registerDefaultNetworkCallback(networkCallback) }
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
                        else -> describe(e, server.trim().trimEnd('/'))
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
        lastSearch = search
        viewModelScope.launch {
            _ui.value = _ui.value.copy(loading = true)
            // Known not to work (offline, or Tailscale off for a tailnet
            // address): say so at once rather than after a connect timeout.
            ConnectionErrors.preflight(NetworkStatus.read(getApplication()), repo.baseUrl)?.let {
                _ui.value = _ui.value.copy(loading = false, error = explain(it, repo.baseUrl))
                return@launch
            }
            try {
                val albums = repo.api.albums(search = search?.ifBlank { null })
                _ui.value = _ui.value.copy(albums = albums, loading = false, error = null)
            } catch (e: Unauthorized) {
                // The saved token is dead; keeping it would only repeat this on
                // every launch.
                settings.clearToken()
                token = null
                _ui.value = _ui.value.copy(
                    signedIn = false,
                    loading = false,
                    albums = emptyList(),
                    error = "Your session expired. Sign in again.",
                )
            } catch (e: Exception) {
                // Keep whatever albums are already showing; an error above a
                // stale grid is more use than an empty one.
                _ui.value = _ui.value.copy(loading = false, error = describe(e, repo.baseUrl))
            }
        }
    }

    fun retry() = loadAlbums(lastSearch)

    private fun describe(e: Throwable, serverUrl: String): String =
        explain(ConnectionErrors.classify(e), serverUrl)

    private fun explain(failure: ConnectionErrors.Failure, serverUrl: String): String =
        ConnectionErrors.explain(failure, NetworkStatus.read(getApplication()), serverUrl).text

    fun openAlbum(id: Long) {
        val repo = repository ?: return
        viewModelScope.launch {
            try {
                _ui.value = _ui.value.copy(openAlbum = repo.api.album(id), error = null)
            } catch (e: Exception) {
                _ui.value = _ui.value.copy(error = describe(e, repo.baseUrl))
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
                _ui.value = _ui.value.copy(error = describe(e, repo.baseUrl))
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
        runCatching { connectivity?.unregisterNetworkCallback(networkCallback) }
        player.release()
        super.onCleared()
    }
}
