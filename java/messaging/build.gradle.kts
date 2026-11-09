plugins {
    id("rift.library")
}

description = "Client for Rift's QUIC messaging bus (protocol rift-messaging/1)"

dependencies {
    implementation(libs.kwik)
}
