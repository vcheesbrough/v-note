/**
 * Derives the APK `versionName` / `versionCode` from the workspace.
 *
 * The root `Cargo.toml` is read directly at configuration time rather than the derived
 * `version.txt`, which `syncVersionFromWorkspace` only writes at execution time — after
 * AGP has already consumed `versionName` (#327).
 */
object WorkspaceVersion {
    // Same rule as scripts/sync-version.sh: the first line starting `version = "…"`.
    private val cargoVersionLine = Regex("""^version = "([^"]*)"""", RegexOption.MULTILINE)

    fun cargoVersion(cargoToml: String): String? =
        cargoVersionLine
            .find(cargoToml)
            ?.groupValues
            ?.get(1)
            ?.trim()
            ?.takeIf { it.isNotEmpty() }

    /**
     * CI injects the computed release tag (e.g. 0.4.1) via `V_NOTE_RELEASE` so versionName
     * matches the deployed image; otherwise the cargo version is used. Fails loudly rather
     * than shipping a placeholder version.
     */
    fun versionName(
        injectedRelease: String?,
        cargoToml: String?,
    ): String =
        injectedRelease?.trim()?.takeIf { it.isNotEmpty() }
            ?: cargoToml?.let(::cargoVersion)
            ?: throw IllegalStateException(
                "Cannot determine the app version: V_NOTE_RELEASE is unset and no " +
                    "`version = \"…\"` line was found in the root Cargo.toml.",
            )

    // Monotonic versionCode from major.minor.patch so in-place upgrades are accepted
    // (e.g. 0.4.1 -> 4001). Pre-release suffixes are ignored for the code.
    fun versionCode(versionName: String): Int =
        versionName.substringBefore('-').split('.').let { parts ->
            val major = parts.getOrNull(0)?.toIntOrNull() ?: 0
            val minor = parts.getOrNull(1)?.toIntOrNull() ?: 0
            val patch = parts.getOrNull(2)?.toIntOrNull() ?: 0
            (major * 1_000_000 + minor * 1_000 + patch).coerceAtLeast(1)
        }
}
