package lan.albumplayer.android.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

// The same palette as the desktop and web clients, so the three do not feel
// like different products.
private val Accent = Color(0xFF5EC8F2)
private val Background = Color(0xFF0E1114)
private val Surface = Color(0xFF191D22)
private val Dim = Color(0xFF8B949E)

private val Dark = darkColorScheme(
    primary = Accent,
    onPrimary = Color(0xFF06121B),
    background = Background,
    onBackground = Color(0xFFE8EAED),
    surface = Surface,
    onSurface = Color(0xFFE8EAED),
    onSurfaceVariant = Dim,
    surfaceVariant = Color(0xFF232A31),
)

@Composable
fun AlbumPlayerTheme(content: @Composable () -> Unit) {
    // A listening app is usually open in the evening; it stays dark either way.
    @Suppress("UNUSED_EXPRESSION") isSystemInDarkTheme()
    MaterialTheme(colorScheme = Dark) {
        // Text takes its colour from LocalContentColor, which defaults to black
        // unless a Surface supplies it. Screens shown outside the Scaffold —
        // the login form — would otherwise render near-invisibly on the dark
        // background, so the whole tree sits on one.
        Surface(
            modifier = Modifier.fillMaxSize(),
            color = MaterialTheme.colorScheme.background,
            content = content,
        )
    }
}

@Suppress("unused")
private val UnusedLight = lightColorScheme()
