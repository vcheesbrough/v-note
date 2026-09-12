package link.desync.vnote

/**
 * Marks an `androidTest` class as a **measurement fixture** rather than a test:
 * something run by hand against a real device to produce numbers, never part of
 * the CI suite.
 *
 * [scripts/run-android-instrumented-ci.sh] excludes annotated classes with
 * `-e notAnnotation`. Doing it this way keeps CI's existing rule — every
 * collected test must actually run and pass, no skips — instead of teaching the
 * runner to tolerate skipped tests, which would let a test that silently stops
 * running keep CI green.
 */
@Retention(AnnotationRetention.RUNTIME)
@Target(AnnotationTarget.CLASS)
annotation class MeasurementFixture
