// Shared Java settings: Java 21 bytecode (Paper's floor), strict compiler
// warnings and JUnit for every module.
plugins {
    `java-library`
}

val libs = versionCatalogs.named("libs")

java {
    toolchain.languageVersion = JavaLanguageVersion.of(21)
}

tasks.withType<JavaCompile>().configureEach {
    options.encoding = "UTF-8"
    options.release = 21
    options.compilerArgs.addAll(listOf("-Xlint:all,-serial,-processing", "-Werror"))
}

tasks.withType<Jar>().configureEach {
    val title = project.name
    val version = project.version.toString()
    manifest {
        attributes("Implementation-Title" to title, "Implementation-Version" to version)
    }
}

tasks.withType<Javadoc>().configureEach {
    (options as StandardJavadocDocletOptions).apply {
        encoding = "UTF-8"
        addBooleanOption("Xdoclint:all,-missing", true)
        addBooleanOption("Werror", true)
    }
}

dependencies {
    testImplementation(platform(libs.findLibrary("junit-bom").get()))
    testImplementation(libs.findLibrary("junit-jupiter").get())
    testRuntimeOnly(libs.findLibrary("junit-launcher").get())
}

tasks.withType<Test>().configureEach {
    useJUnitPlatform()
    // The shared corpus lives at the repository root, beside the Rust crates.
    systemProperty("rift.fixtures", rootDir.resolve("../tests/fixtures/permissions").absolutePath)
}
