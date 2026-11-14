plugins {
    id("rift.library")
}

description = "Rift network client: the network API over the messaging bus"

dependencies {
    api(project(":rift-network-api"))
    api(project(":rift-messaging"))
    // Only for its JSON value model.
    implementation(project(":rift-permissions-client"))
}
