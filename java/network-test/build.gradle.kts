plugins {
    id("rift.library")
}

description = "In-memory Rift network for plugin unit tests"

dependencies {
    api(project(":rift-network-api"))
}
