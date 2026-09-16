// Build logic the app build script calls at configuration time, kept here so it can be
// unit-tested (#327). CI runs the tests with `:buildSrc:test` in Dockerfile.android.
plugins {
    `kotlin-dsl`
}

repositories {
    mavenCentral()
}

dependencies {
    testImplementation("junit:junit:4.13.2")
}
