plugins {
    id("rift.library")
    `jvm-test-suite`
}

description = "Rift permissions client: replica, resolver, JSON codec and bus integration"

dependencies {
    api(project(":rift-permissions-api"))
    api(project(":rift-messaging"))
}

testing {
    suites {
        // Runs against a real `rift` process; skipped when the binary is absent.
        register<JvmTestSuite>("integrationTest") {
            dependencies {
                implementation(project())
            }
            targets.configureEach {
                testTask.configure {
                    val binary = providers.environmentVariable("RIFT_BINARY")
                        .orElse(rootDir.resolve("../target/debug/rift").absolutePath)
                    inputs.property("riftBinary", binary)
                    systemProperty("rift.binary", binary.get())
                    outputs.upToDateWhen { false }
                }
            }
        }
    }
}
