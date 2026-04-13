import org.gradle.internal.os.OperatingSystem
import java.io.ByteArrayOutputStream
import java.io.File
import java.time.LocalDateTime
import java.time.format.DateTimeFormatter

plugins {
    base
}

val workspaceRoot = layout.projectDirectory.dir("../..").asFile
val generatedDir = layout.projectDirectory.dir("app/src/main/java").asFile
val generatedJniLibsDir = layout.projectDirectory.dir("generated/jniLibs").asFile
val localToolsDir = layout.projectDirectory.dir(".tools").asFile

data class AndroidAbi(
    val abi: String,
)

fun androidAbisForBuild(): List<AndroidAbi> {
    val configured = (findProperty("bmux.android.abis") as String?)
        ?.split(',')
        ?.map { it.trim() }
        ?.filter { it.isNotEmpty() }
        ?.toSet()
        ?: setOf("arm64-v8a")

    val all = listOf(
        AndroidAbi(abi = "arm64-v8a"),
        AndroidAbi(abi = "x86_64"),
        AndroidAbi(abi = "armeabi-v7a"),
    )

    return all.filter { it.abi in configured }
}

fun Project.runChecked(
    command: List<String>,
    workingDir: File,
    environment: Map<String, String> = emptyMap(),
) {
    val process = ProcessBuilder(command)
        .directory(workingDir)
        .redirectErrorStream(true)
        .apply {
            val env = environment()
            for ((key, value) in environment) {
                env[key] = value
            }
        }
        .start()
    val output = process.inputStream.bufferedReader().readText()
    val exitCode = process.waitFor()
    if (exitCode != 0) {
        throw GradleException("Command failed (${command.joinToString(" ")}):\n$output")
    }
}

fun resolveAndroidSdkRoot(): File {
    val fromEnv = listOfNotNull(
        System.getenv("ANDROID_SDK_ROOT"),
        System.getenv("ANDROID_HOME"),
    )
        .map(::File)
        .firstOrNull { it.exists() }
    if (fromEnv != null) {
        return fromEnv
    }

    val localProperties = layout.projectDirectory.file("local.properties").asFile
    if (localProperties.exists()) {
        val props = java.util.Properties()
        localProperties.inputStream().use(props::load)
        val sdkDir = props.getProperty("sdk.dir")
        if (!sdkDir.isNullOrBlank()) {
            val sdkRoot = File(sdkDir)
            if (sdkRoot.exists()) {
                return sdkRoot
            }
        }
    }

    throw GradleException("Android SDK not found. Set ANDROID_SDK_ROOT/ANDROID_HOME or sdk.dir.")
}

val buildAndroidFfiLibs = tasks.register("buildAndroidFfiLibs") {
    group = "bmux"
    description = "Builds bmux_mobile_ffi Android .so files for configured ABIs"

    doLast {
        val abis = androidAbisForBuild()
        if (abis.isEmpty()) {
            throw GradleException("No Android ABIs selected. Set -Pbmux.android.abis=arm64-v8a,x86_64")
        }

        if (
            System.getenv("ANDROID_SDK_ROOT").isNullOrBlank() &&
            System.getenv("ANDROID_HOME").isNullOrBlank()
        ) {
            throw GradleException("ANDROID_SDK_ROOT (or ANDROID_HOME) must be set for cargo-ndk")
        }

        val hasCargoNdk = runCatching {
            runChecked(
                command = listOf("cargo", "ndk", "--version"),
                workingDir = workspaceRoot,
            )
        }.isSuccess
        if (!hasCargoNdk) {
            throw GradleException(
                "cargo-ndk is required for Android FFI builds. Enter this repo via 'nix develop' or install cargo-ndk.",
            )
        }

        generatedJniLibsDir.mkdirs()

        for (target in abis) {
            runChecked(
                command = listOf(
                    "cargo",
                    "ndk",
                    "-t",
                    target.abi,
                    "-o",
                    generatedJniLibsDir.absolutePath,
                    "-P",
                    "29",
                    "build",
                    "-p",
                    "bmux_mobile_ffi",
                ),
                workingDir = workspaceRoot,
            )

            val outputLib = generatedJniLibsDir
                .resolve(target.abi)
                .resolve("libbmux_mobile_ffi.so")
            if (!outputLib.exists()) {
                throw GradleException("Expected output library not found at ${outputLib.absolutePath}")
            }
        }
    }
}

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

project(":app") {
    tasks.matching { it.name == "preBuild" }.configureEach {
        dependsOn(buildAndroidFfiLibs)
    }
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
