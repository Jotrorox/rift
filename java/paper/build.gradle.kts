plugins {
    id("rift.java")
    id("rift.publish")
    alias(libs.plugins.shadow)
}

description = "RiftPerms: the Paper, Folia and Purpur adapter for Rift permissions"

dependencies {
    implementation(project(":rift-permissions-client"))
    compileOnly(libs.paper.api)
    compileOnly(libs.vault.api) { isTransitive = false }
    compileOnly(libs.placeholderapi) { isTransitive = false }

    testImplementation(libs.paper.api)
    testImplementation(libs.mockbukkit)
}

tasks.processResources {
    val version = project.version.toString()
    inputs.property("version", version)
    filesMatching("plugin.yml") { expand("version" to version) }
}

tasks.shadowJar {
    archiveBaseName = "RiftPerms"
    archiveClassifier = ""
    // The API stays at its own package so plugins compiled against
    // rift-permissions-api resolve it from this jar; everything else moves.
    val lib = "io.github.jotrorox.rift.permissions.paper.lib"
    relocate("io.github.jotrorox.rift.permissions.client", "$lib.client")
    relocate("io.github.jotrorox.rift.messaging", "$lib.messaging")
    relocate("tech.kwik", "$lib.kwik")
    relocate("at.favre.lib", "$lib.favre")
    relocate("io.whitfin", "$lib.whitfin")
    mergeServiceFiles()
}

tasks.jar {
    archiveClassifier = "plain"
}

tasks.assemble {
    dependsOn(tasks.shadowJar)
}

publishing {
    publications.create<MavenPublication>("maven") {
        from(components["shadow"])
    }
}
