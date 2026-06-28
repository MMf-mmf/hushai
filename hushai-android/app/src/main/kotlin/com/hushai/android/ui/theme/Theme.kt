package com.hushai.android.ui.theme

import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.Typography
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * Uber-inspired app theme: a deliberately light, high-contrast monochrome look
 * (white surfaces, near-black text, gray for secondary). It is applied app-wide in
 * `MainActivity` and is the single source of truth for color — composables should
 * read [MaterialTheme.colorScheme] rather than hardcoding colors.
 *
 * The light scheme is used unconditionally (system dark mode is ignored on purpose)
 * so the intended Uber look is guaranteed; the previous default-`MaterialTheme`
 * setup produced low-contrast grays and a dim, "all black" feel.
 */
private val HushaiColors = lightColorScheme(
    primary = Black,
    onPrimary = White,
    secondary = Mono800,
    onSecondary = White,
    tertiary = CaptureGreen, // "Capturing"/active accent
    onTertiary = White,
    background = White,
    onBackground = Black,
    surface = White,
    onSurface = Black,
    surfaceVariant = Mono100, // card / elevated fill
    onSurfaceVariant = Mono800, // secondary text on cards — legible on white
    outline = Mono300,
    outlineVariant = Mono300,
    error = UberRed,
    onError = White,
)

// Bold, slightly larger headings give the clean Uber feel; bundled Roboto family
// (no external font files added — custom fonts are out of scope for this change).
private val HushaiTypography = Typography().run {
    copy(
        headlineMedium = headlineMedium.copy(fontWeight = FontWeight.Bold, fontSize = 30.sp),
        headlineSmall = headlineSmall.copy(fontWeight = FontWeight.Bold),
        titleLarge = titleLarge.copy(fontWeight = FontWeight.Bold),
        titleMedium = titleMedium.copy(fontWeight = FontWeight.SemiBold),
        labelLarge = labelLarge.copy(fontWeight = FontWeight.SemiBold),
    )
}

private val HushaiShapes = Shapes(
    small = RoundedCornerShape(8.dp),
    medium = RoundedCornerShape(12.dp),
    large = RoundedCornerShape(16.dp),
)

@Composable
fun HushaiTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = HushaiColors,
        typography = HushaiTypography,
        shapes = HushaiShapes,
        content = content,
    )
}
