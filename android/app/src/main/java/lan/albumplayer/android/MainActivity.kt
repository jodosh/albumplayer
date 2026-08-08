package lan.albumplayer.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.*
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Shuffle
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.delay
import lan.albumplayer.android.ui.*

class MainActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent { AlbumPlayerTheme { App() } }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun App(model: MainViewModel = viewModel()) {
    val ui by model.ui.collectAsStateWithLifecycle()
    val playback by model.player.state.collectAsStateWithLifecycle()
    var search by remember { mutableStateOf("") }

    // The transport clock is polled rather than pushed; the session reports
    // state changes but not every passing second.
    LaunchedEffect(playback.hasQueue) {
        while (playback.hasQueue) {
            model.player.refreshPosition()
            delay(500)
        }
    }

    if (!ui.signedIn) {
        LoginScreen(
            initialServer = ui.server,
            error = ui.error,
            busy = ui.loading,
            onSignIn = model::signIn,
        )
        return
    }

    Scaffold(
        topBar = {
            Column {
                TopAppBar(
                    title = { Text("AlbumPlayer") },
                    actions = {
                        IconButton(onClick = model::shuffleAlbums) {
                            Icon(Icons.Filled.Shuffle, "Shuffle albums")
                        }
                        TextButton(onClick = model::signOut) { Text("Sign out") }
                    },
                )
                if (ui.openAlbum == null) {
                    OutlinedTextField(
                        value = search,
                        onValueChange = {
                            search = it
                            model.loadAlbums(it)
                        },
                        placeholder = { Text("Search albums and artists") },
                        singleLine = true,
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(horizontal = 12.dp, vertical = 4.dp),
                    )
                }
            }
        },
        bottomBar = {
            NowPlayingBar(
                state = playback,
                repository = model.repo(),
                onToggle = model.player::togglePlayPause,
                onNextTrack = model.player::nextTrack,
                onPreviousTrack = model.player::previousTrack,
                onNextAlbum = model.player::nextAlbum,
                onPreviousAlbum = model.player::previousAlbum,
            )
        },
    ) { padding ->
        Box(Modifier.padding(padding)) {
            val album = ui.openAlbum
            when {
                album != null -> AlbumScreen(
                    album = album,
                    repository = model.repo(),
                    nowPlayingTrackId = playback.trackId,
                    onBack = model::closeAlbum,
                    onPlay = { index -> model.playAlbum(album, index) },
                    onEnqueue = { model.enqueueAlbum(album) },
                )

                ui.loading && ui.albums.isEmpty() ->
                    Box(Modifier.fillMaxSize(), Alignment.Center) { CircularProgressIndicator() }

                else -> AlbumGrid(
                    albums = ui.albums,
                    repository = model.repo(),
                    onOpen = { model.openAlbum(it.id) },
                )
            }
        }
    }
}
