plugins {
    id("rift.library")
}

description = "Rift control client: status, maintenance, drain, transfer, reload, nodes and events over the messaging bus"

dependencies {
    api(project(":rift-messaging"))
    // For its JSON value model, which execute() and the raw replies expose.
    api(project(":rift-permissions-client"))
}
