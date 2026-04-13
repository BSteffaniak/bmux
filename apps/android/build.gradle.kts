import org.gradle.internal.os.OperatingSystem
import java.io.ByteArrayOutputStream
import java.time.LocalDateTime
import java.time.format.DateTimeFormatter

plugins {
    base
}

val workspaceRoot = layout.projectDirectory.dir("../..").asFile
val generatedDir = layout.projectDirectory.dir("app/src/main/java").asFile
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

tasks.register("packageInternalAlpha") {
    group = "bmux"
    description = "Builds internal alpha APK with generated UniFFI bindings"
    dependsOn("generateUniffiKotlinBindings")
    dependsOn(":app:assembleAlpha")
}

tasks.register("collectAlphaLogs") {
    group = "bmux"
    description = "Collects BmuxAlpha adb log lines into a timestamped file"

    doLast {
        val stdout = ByteArrayOutputStream()
        val stderr = ByteArrayOutputStream()

        val adbCandidates = listOfNotNull(
            System.getenv("ANDROID_SDK_ROOT")?.let { "$it/platform-tools/adb" },
            System.getenv("ANDROID_HOME")?.let { "$it/platform-tools/adb" },
            "${System.getProperty("user.home")}/Library/Android/sdk/platform-tools/adb",
            "adb",
        )
        val adbCommand = adbCandidates.firstOrNull { candidate ->
            candidate == "adb" || file(candidate).exists()
        } ?: "adb"

        val process = ProcessBuilder(adbCommand, "logcat", "-d")
            .directory(workspaceRoot)
            .start()
        stdout.write(process.inputStream.readBytes())
        stderr.write(process.errorStream.readBytes())
        val exitValue = process.waitFor()

        if (exitValue != 0) {
            val errorText = stderr.toString().ifBlank { stdout.toString() }
            throw GradleException(
                "Failed to read adb logcat output. Is an emulator/device attached?\n$errorText",
            )
        }

        val filtered = stdout
            .toString()
            .lineSequence()
            .filter { it.contains("BmuxAlpha") }
            .joinToString(separator = System.lineSeparator())

        val timestamp = LocalDateTime.now().format(DateTimeFormatter.ofPattern("yyyyMMdd-HHmmss"))
        val logsDir = layout.projectDirectory.dir("logs/alpha").asFile
        logsDir.mkdirs()
        val outputFile = logsDir.resolve("bmux-alpha-$timestamp.log")

        outputFile.writeText(
            if (filtered.isBlank()) {
                "No BmuxAlpha lines found in adb logcat output."
            } else {
                filtered
            } + System.lineSeparator(),
        )

        logger.lifecycle("Wrote alpha logs to ${outputFile.absolutePath}")
    }
}
