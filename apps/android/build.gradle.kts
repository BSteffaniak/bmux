import org.gradle.internal.os.OperatingSystem

plugins {
    base
}

val workspaceRoot = layout.projectDirectory.dir("../..").asFile
val generatedDir = layout.projectDirectory.dir("generated/uniffi").asFile

val cargoBuildMobileFfi = tasks.register<Exec>("cargoBuildMobileFfi") {
    group = "bmux"
    description = "Builds bmux_mobile_ffi for UniFFI binding generation"
    workingDir = workspaceRoot
    commandLine("cargo", "build", "-p", "bmux_mobile_ffi")
}

tasks.register("generateUniffiKotlinBindings") {
    group = "bmux"
    description = "Generates Kotlin bindings from bmux_mobile_ffi via UniFFI"
    dependsOn(cargoBuildMobileFfi)

    doLast {
        generatedDir.mkdirs()

        val os = OperatingSystem.current()
        val libraryName = when {
            os.isWindows -> "bmux_mobile_ffi.dll"
            os.isMacOsX -> "libbmux_mobile_ffi.dylib"
            else -> "libbmux_mobile_ffi.so"
        }

        val libraryPath = workspaceRoot
            .resolve("target")
            .resolve("debug")
            .resolve(libraryName)
            .absolutePath

        if (!file(libraryPath).exists()) {
            throw GradleException("Expected library not found at $libraryPath")
        }

        exec {
            workingDir = workspaceRoot
            commandLine(
                "uniffi-bindgen",
                "generate",
                "--library",
                libraryPath,
                "--language",
                "kotlin",
                "--out-dir",
                generatedDir.absolutePath,
            )
        }
    }
}
