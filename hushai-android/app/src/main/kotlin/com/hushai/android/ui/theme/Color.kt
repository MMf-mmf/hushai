package com.hushai.android.ui.theme

import androidx.compose.ui.graphics.Color

/**
 * Uber-inspired monochrome palette: a near-black/white/gray ramp with two
 * functional accents. The whole app draws its colors from [HushaiTheme] (which
 * maps these into a Material3 [androidx.compose.material3.ColorScheme]) — avoid
 * hardcoding `Color(...)` in composables so contrast stays consistent.
 */

// Mono ramp (light → dark). Secondary text uses Mono800 so it stays legible on white.
val White = Color(0xFFFFFFFF)
val Mono100 = Color(0xFFF6F6F6) // card / elevated surface fill
val Mono300 = Color(0xFFE2E2E2) // hairline borders, dividers, switch-off track
val Mono600 = Color(0xFF757575) // muted/disabled text
val Mono800 = Color(0xFF545454) // secondary text — passes contrast on white
val Black = Color(0xFF000000)   // primary actions + primary text

// Functional accents.
val CaptureGreen = Color(0xFF06A957) // "Capturing" / ON indicator
val UberRed = Color(0xFFE11900)      // errors / destructive
