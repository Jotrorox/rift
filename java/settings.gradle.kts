pluginManagement {
    includeBuild("build-logic")
    repositories {
        gradlePluginPortal()
    }
}

plugins {
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
}

dependencyResolutionManagement {
    repositoriesMode = RepositoriesMode.FAIL_ON_PROJECT_REPOS
    repositories {
        mavenCentral()
        maven("https://repo.papermc.io/repository/maven-public/") {
            content {
                includeGroupAndSubgroups("io.papermc")
                includeGroup("net.md-5")
                includeGroup("com.mojang")
            }
        }
        maven("https://jitpack.io") {
            content { includeGroup("com.github.MilkBowl") }
        }
        maven("https://repo.extendedclip.com/releases/") {
            content { includeGroup("me.clip") }
        }
    }
}

rootProject.name = "rift-java"

// Directories stay short; project names are the published artifact IDs.
mapOf(
    "messaging" to "rift-messaging",
    "api" to "rift-permissions-api",
    "client" to "rift-permissions-client",
    "test" to "rift-permissions-test",
    "paper" to "rift-permissions-paper",
).forEach { (directory, artifact) ->
    include(artifact)
    project(":$artifact").projectDir = file(directory)
}
