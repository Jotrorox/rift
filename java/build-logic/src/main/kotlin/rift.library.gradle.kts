// A published library: jar, sources and Javadoc.
plugins {
    id("rift.java")
    id("rift.publish")
}

java {
    withSourcesJar()
    withJavadocJar()
}

publishing {
    publications.create<MavenPublication>("maven") {
        from(components["java"])
    }
}
