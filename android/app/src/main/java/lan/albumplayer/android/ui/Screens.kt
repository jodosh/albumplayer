package lan.albumplayer.android.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import coil.compose.AsyncImage
import lan.albumplayer.android.data.AlbumDetail
import lan.albumplayer.android.data.AlbumSummary
import lan.albumplayer.android.data.Repository

/** `h:mm:ss` or `m:ss`, as everywhere else in the project. */
fun formatDuration(ms: Long): String {
    val total = ms / 1000
    val h = total / 3600
    val m = (total % 3600) / 60
    val s = total % 60
    return if (h > 0) "%d:%02d:%02d".format(h, m, s) else "%d:%02d".format(m, s)
}

/**
 * A cover, or a generated tile when there is none.
 *
 * Most of a ripped library has no artwork, so the fallback has to be a real
 * design rather than a grey box: a colour derived from the album, with its
 * initials.
 */
@Composable
fun Cover(
    albumId: Long,
    title: String,
    artist: String,
    hasCover: Boolean,
    repository: Repository?,
    modifier: Modifier = Modifier,
) {
    val hue = remember(title, artist) {
        var h = 0
        for (c in (artist + title)) h = (h * 31 + c.code) % 360
        h.toFloat()
    }
    val initials = remember(title) {
        title.replace(Regex("^(the|a|an)\\s+", RegexOption.IGNORE_CASE), "")
            .split(Regex("\\s+"))
            .filter { it.any(Char::isLetterOrDigit) }
            .take(2)
            .mapNotNull { it.firstOrNull()?.uppercaseChar() }
            .joinToString("")
            .ifEmpty { "♪" }
    }
    val tint = Color.hsl(hue, 0.38f, 0.26f)

    Box(
        modifier
            .aspectRatio(1f)
            .clip(RoundedCornerShape(6.dp))
            .background(tint),
        contentAlignment = Alignment.Center,
    ) {
        if (hasCover && repository != null) {
            AsyncImage(
                model = repository.coverUrl(albumId),
                contentDescription = null,
                contentScale = ContentScale.Crop,
                modifier = Modifier.fillMaxSize(),
            )
        } else {
            Text(
                initials,
                fontSize = 22.sp,
                fontWeight = FontWeight.SemiBold,
                color = Color.hsl(hue, 0.30f, 0.86f),
            )
        }
    }
}

@Composable
fun LoginScreen(
    initialServer: String,
    error: String?,
    busy: Boolean,
    onSignIn: (String, String) -> Unit,
) {
    var server by remember { mutableStateOf(initialServer) }
    var password by remember { mutableStateOf("") }

    Column(
        Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.Center,
    ) {
        Text("AlbumPlayer", fontSize = 26.sp, fontWeight = FontWeight.Bold)
        Text(
            "Albums, not shuffle.",
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(bottom = 24.dp),
        )

        OutlinedTextField(
            value = server,
            onValueChange = { server = it },
            label = { Text("Server") },
            placeholder = { Text("http://10.0.0.2:8080") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(12.dp))
        OutlinedTextField(
            value = password,
            onValueChange = { password = it },
            label = { Text("Password") },
            singleLine = true,
            visualTransformation = PasswordVisualTransformation(),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
            modifier = Modifier.fillMaxWidth(),
        )

        if (error != null) {
            Spacer(Modifier.height(12.dp))
            Text(error, color = MaterialTheme.colorScheme.error)
        }

        Spacer(Modifier.height(20.dp))
        Button(
            onClick = { onSignIn(server, password) },
            enabled = !busy && server.isNotBlank() && password.isNotBlank(),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(if (busy) "Signing in…" else "Sign in")
        }
    }
}

@Composable
fun AlbumGrid(
    albums: List<AlbumSummary>,
    repository: Repository?,
    onOpen: (AlbumSummary) -> Unit,
    modifier: Modifier = Modifier,
) {
    LazyVerticalGrid(
        columns = GridCells.Adaptive(minSize = 148.dp),
        contentPadding = PaddingValues(12.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp),
        modifier = modifier,
    ) {
        items(albums, key = { it.id }) { album ->
            Column(Modifier.clickable { onOpen(album) }) {
                Cover(album.id, album.title, album.artist, album.hasCover, repository)
                Spacer(Modifier.height(6.dp))
                Text(
                    album.title,
                    fontSize = 13.sp,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    lineHeight = 16.sp,
                )
                Text(
                    album.artist + (album.year?.let { " · $it" } ?: ""),
                    fontSize = 11.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

@Composable
fun AlbumScreen(
    album: AlbumDetail,
    repository: Repository?,
    nowPlayingTrackId: Long?,
    onBack: () -> Unit,
    onPlay: (Int) -> Unit,
    onEnqueue: () -> Unit,
) {
    LazyColumn(Modifier.fillMaxSize()) {
        item {
            Column(Modifier.padding(16.dp)) {
                TextButton(onClick = onBack, contentPadding = PaddingValues(0.dp)) {
                    Text("← Library")
                }
                Spacer(Modifier.height(8.dp))
                Row {
                    Cover(
                        album.id, album.title, album.artist, album.hasCover, repository,
                        modifier = Modifier.width(132.dp),
                    )
                    Spacer(Modifier.width(16.dp))
                    Column {
                        Text(album.title, fontSize = 20.sp, fontWeight = FontWeight.Bold)
                        Text(album.artist, color = MaterialTheme.colorScheme.onSurface)
                        Spacer(Modifier.height(6.dp))
                        Text(
                            buildString {
                                album.year?.let { append("$it · ") }
                                append("${album.trackCount} tracks · ")
                                append(formatDuration(album.durationMs))
                                if (album.discCount > 1) append(" · ${album.discCount} discs")
                            },
                            fontSize = 12.sp,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        Text(
                            album.gainDb?.let { "ReplayGain %+.1f dB".format(it) }
                                ?: "no ReplayGain",
                            fontSize = 12.sp,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
                Spacer(Modifier.height(14.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(onClick = { onPlay(0) }) { Text("Play album") }
                    OutlinedButton(onClick = onEnqueue) { Text("Add to queue") }
                }
            }
        }

        itemsIndexed(album.tracks) { index, track ->
            Row(
                Modifier
                    .fillMaxWidth()
                    .clickable { onPlay(index) }
                    .padding(horizontal = 20.dp, vertical = 10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    if (track.trackNo > 0) "${track.trackNo}" else "·",
                    modifier = Modifier.width(28.dp),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 13.sp,
                )
                Text(
                    track.title,
                    modifier = Modifier.weight(1f),
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    color = if (track.id == nowPlayingTrackId) {
                        MaterialTheme.colorScheme.primary
                    } else {
                        MaterialTheme.colorScheme.onSurface
                    },
                )
                Text(
                    formatDuration(track.durationMs),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 12.sp,
                )
            }
        }

        item { Spacer(Modifier.height(96.dp)) }
    }
}

/** Compose has no itemsIndexed for LazyColumn lists of arbitrary type by default here. */
private inline fun <T> androidx.compose.foundation.lazy.LazyListScope.itemsIndexed(
    items: List<T>,
    crossinline itemContent: @Composable (Int, T) -> Unit,
) = items(items.size) { index -> itemContent(index, items[index]) }

@Composable
fun NowPlayingBar(
    state: PlaybackState,
    repository: Repository?,
    onToggle: () -> Unit,
    onNextTrack: () -> Unit,
    onPreviousTrack: () -> Unit,
    onNextAlbum: () -> Unit,
    onPreviousAlbum: () -> Unit,
) {
    if (!state.hasQueue) return

    Surface(tonalElevation = 3.dp, color = MaterialTheme.colorScheme.surface) {
        Column(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(Modifier.size(44.dp)) {
                    if (state.artworkUri != null) {
                        AsyncImage(
                            model = state.artworkUri,
                            contentDescription = null,
                            contentScale = ContentScale.Crop,
                            modifier = Modifier.fillMaxSize().clip(RoundedCornerShape(4.dp)),
                        )
                    }
                }
                Spacer(Modifier.width(10.dp))
                Column(Modifier.weight(1f)) {
                    Text(state.title, maxLines = 1, overflow = TextOverflow.Ellipsis, fontSize = 14.sp)
                    Text(
                        "${state.artist} — ${state.albumTitle}",
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        fontSize = 11.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                // Album and track skips stay separate controls, as on the desktop.
                IconButton(onClick = onPreviousAlbum) {
                    Icon(Icons.Filled.FastRewind, "Previous album")
                }
                IconButton(onClick = onPreviousTrack) {
                    Icon(Icons.Filled.SkipPrevious, "Previous track")
                }
                IconButton(onClick = onToggle) {
                    Icon(
                        if (state.isPlaying) Icons.Filled.Pause else Icons.Filled.PlayArrow,
                        if (state.isPlaying) "Pause" else "Play",
                    )
                }
                IconButton(onClick = onNextTrack) {
                    Icon(Icons.Filled.SkipNext, "Next track")
                }
                IconButton(onClick = onNextAlbum) {
                    Icon(Icons.Filled.FastForward, "Next album")
                }
            }
            if (state.durationMs > 0) {
                LinearProgressIndicator(
                    progress = { (state.positionMs.toFloat() / state.durationMs).coerceIn(0f, 1f) },
                    modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
                )
            }
        }
    }
}
