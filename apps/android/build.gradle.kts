import org.gradle.internal.os.OperatingSystem

plugins {
    base
}

val workspaceRoot = layout.projectDirectory.dir("../..").asFile
val generatedDir = layout.projectDirectory.dir("generated/uniffi").asFile
val localToolsDir = layout.projectDirectory.dir(".tools").asFile

val installUniffiBindgen = tasks.register<Exec>("installUniffiBindgen") {
    group = "bmux"
    description = "Installs uniffi-bindgen locally for wrapper task usage"
    workingDir = workspaceRoot
    commandLine(
        "cargo",
        "install",
        "--locked",
        "--root",
        localToolsDir.absolutePath,
        "uniffi",
        "--version",
        "0.31.0",
        "--features",
        "cli",
    )
}

val cargoBuildMobileFfi = tasks.register<Exec>("cargoBuildMobileFfi") {
    group = "bmux"
    description = "Builds bmux_mobile_ffi for UniFFI binding generation"
    workingDir = workspaceRoot
    commandLine("cargo", "build", "-p", "bmux_mobile_ffi")
}

tasks.register<Exec>("generateUniffiKotlinBindings") {
    group = "bmux"
    description = "Generates Kotlin bindings from bmux_mobile_ffi via UniFFI"
    dependsOn(installUniffiBindgen)
    dependsOn(cargoBuildMobileFfi)

    doFirst {
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

        val bindgenName = if (os.isWindows) "uniffi-bindgen.exe" else "uniffi-bindgen"
        val bindgenPath = localToolsDir
            .resolve("bin")
            .resolve(bindgenName)
            .absolutePath

        if (!file(bindgenPath).exists()) {
            throw GradleException("Expected uniffi-bindgen not found at $bindgenPath")
        }

        workingDir = workspaceRoot
        commandLine(
            bindgenPath,
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
