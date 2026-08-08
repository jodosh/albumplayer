pluginManagement {
    repositories {
        google { content { includeGroupByRegex("com\\.android.*|com\\.google.*|androidx.*") } }
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "AlbumPlayer"
include(":app")
