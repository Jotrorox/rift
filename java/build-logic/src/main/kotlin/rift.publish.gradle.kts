// Publishing for every artifact: a local repository the release workflow
// copies into the Pages site's static Maven layout, and the GitHub Packages
// mirror when running in GitHub Actions.
plugins {
    `maven-publish`
}

publishing {
    publications.withType<MavenPublication>().configureEach {
        pom {
            name = project.name
            description = provider { project.description }
            url = "https://github.com/Jotrorox/rift"
            licenses {
                license {
                    name = "BSD-2-Clause"
                    url = "https://opensource.org/license/bsd-2-clause"
                }
            }
            scm {
                url = "https://github.com/Jotrorox/rift"
                connection = "scm:git:https://github.com/Jotrorox/rift.git"
            }
        }
    }
    repositories {
        maven {
            name = "release"
            url = uri(rootDir.resolve("build/release-repo"))
        }
        providers.environmentVariable("GITHUB_REPOSITORY").orNull?.let { repository ->
            maven {
                name = "GitHubPackages"
                url = uri("https://maven.pkg.github.com/$repository")
                credentials(PasswordCredentials::class)
            }
        }
    }
}
