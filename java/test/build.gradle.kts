plugins {
    id("rift.library")
}

description = "In-memory Rift permissions for plugin unit tests, with a MockBukkit helper"

dependencies {
    api(project(":rift-permissions-api"))
    implementation(project(":rift-permissions-client"))
    // Only MockBukkitPermissions touches these; plugins' tests bring their own.
    compileOnly(libs.paper.api)
    compileOnly(libs.mockbukkit)

    testImplementation(libs.paper.api)
    testImplementation(libs.mockbukkit)
}
