// Points this build's public Maven repositories at the forwarded NDS cache and leaves every other
// repository, private or authenticated, as it is. Pass it to one build with --init-script and set
// NDS_CACHE_BASE to the literal loopback forward, for example http://127.0.0.1:18744. Gradle uses
// plain HTTP there only through this opt-in; nothing is added beside the replaced repositories,
// so an unreachable cache fails the build instead of falling back to the public repositories.
val base = System.getenv("NDS_CACHE_BASE") ?: error("NDS_CACHE_BASE is not set")
require(Regex("""http://127\.0\.0\.1:\d{1,5}""").matches(base)) {
    "NDS_CACHE_BASE must be the literal loopback forward http://127.0.0.1:PORT"
}
val routes = mapOf(
    "https://repo.maven.apache.org/maven2" to "maven-central",
    "https://repo1.maven.org/maven2" to "maven-central",
    "https://dl.google.com/dl/android/maven2" to "google-maven",
    "https://maven.google.com" to "google-maven",
    "https://plugins.gradle.org/m2" to "gradle-plugins",
)

fun RepositoryHandler.retarget() = all {
    val repository = this
    if (repository is MavenArtifactRepository) {
        routes[repository.url.toString().trimEnd('/')]?.let { route ->
            repository.setUrl("$base/$route")
            repository.isAllowInsecureProtocol = true
        }
    }
}

beforeSettings {
    pluginManagement.repositories.retarget()
    dependencyResolutionManagement.repositories.retarget()
}
allprojects {
    buildscript.repositories.retarget()
    repositories.retarget()
}
