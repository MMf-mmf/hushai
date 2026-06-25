// Root build script. Plugin versions are declared here once (apply false) and
// applied in :app. Kotlin + the Compose compiler plugin must share a version.
plugins {
    id("com.android.application") version "8.7.3" apply false
    id("org.jetbrains.kotlin.android") version "2.0.21" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.0.21" apply false
    id("com.squareup.wire") version "4.9.9" apply false
}
