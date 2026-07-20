---
name: java-writer
description: Java correctness and idiomatic style — the Maven/Gradle verify loop, null-safety and Optional discipline, modern language features (records, sealed types, pattern matching), exception design, and clean API conventions. Load before writing, reviewing, or refactoring Java.
activation:
  markers: [pom.xml, build.gradle, build.gradle.kts, settings.gradle, settings.gradle.kts]
  extensions: [java]
---

# Java Writer

How to write Java that is correct *and* idiomatic: drive every operation through
the project's build tool, compile and test in tightening circles, lean on the
type system (and modern language features) to make invalid states hard to build,
and hold the code to the standard of a well-reviewed library.

## The verify loop

Detect the build tool from the marker file: `pom.xml` → Maven, `build.gradle`
or `build.gradle.kts` → Gradle. **Prefer the wrapper** (`./mvnw`, `./gradlew`)
when present — it pins the exact build-tool version the project expects.

1. Compile after every substantive edit: `./mvnw -q compile` /
   `./gradlew compileJava`. Fast type feedback without running tests.
2. Test nearest the change first, then widen. Maven: `./mvnw -q test
   -Dtest=ClassName#method`. Gradle: `./gradlew test --tests
   'pkg.ClassName.method'`. Then the full module.
3. `./mvnw -q verify` / `./gradlew check` before declaring done — this runs the
   full test phase plus whatever static analysis (Checkstyle, SpotBugs, PMD,
   Error Prone) and integration tests the build wires in. Match CI's bar.
4. Format last, so formatting churn never mixes into review: Spotless
   (`./gradlew spotlessApply`, `./mvnw spotless:apply`) or google-java-format.

Never conclude a change works because it "looks right" — if it has not
compiled, it is not done. Prefer several small compile-clean increments over
one large edit followed by a long error-fixing session.

## Build tools and dependencies

- Add dependencies to `pom.xml` / `build.gradle`, not to the classpath by hand;
  let the build tool resolve transitive versions. Centralize versions (Maven
  `<dependencyManagement>` / a Gradle version catalog `libs.versions.toml`)
  rather than scattering literals.
- Never hand-edit a lockfile (`gradle.lockfile`); regenerate it. CI builds
  reproducibly — a stale lock fails the build.
- Check the target Java release before using a language feature: Maven
  `<maven.compiler.release>` / Gradle `java { toolchain { languageVersion } }`.
  A feature newer than that release will not compile in CI even if your local
  JDK is newer.

## Null-safety: prefer absence you can see

- Return `Optional<T>` from methods that may find nothing; do **not** return
  `null`. Never wrap a field or collection in `Optional` — return an empty
  `List`/`Map` instead of `null` or `Optional<List>`.
- Fail fast on unexpected nulls at boundaries with `Objects.requireNonNull(x,
  "x")`; annotate intentional nullability (`@Nullable`/`@NonNull`) so tooling
  and Error Prone can check it.
- `Optional.map`/`.flatMap`/`.orElseGet` over `.isPresent()` + `.get()`;
  `.get()` reads like an unchecked cast.

## Type design: make invalid states hard to build

- **Records** for immutable data carriers (`record Point(int x, int y) {}`) —
  they give you constructor, accessors, `equals`/`hashCode`/`toString` for free.
  Validate invariants in a compact canonical constructor.
- **Sealed** interfaces/classes for closed hierarchies (`sealed interface Shape
  permits Circle, Square`) plus exhaustive `switch` pattern matching — adding a
  variant then becomes a compile error at every decision point.
- Enums over `int`/`String` constants; prefer a small type (`UserId`) over a
  bare `long` that could be any id. Program to interfaces (`List`, `Map`), not
  concrete implementations, in signatures.
- Keep classes immutable where practical: `final` fields, no setters, defensive
  copies of mutable inputs/outputs.

## Error handling

- Throw specific exceptions, not bare `RuntimeException`/`Exception`; carry the
  cause (`throw new XException("context", e)`) — never swallow it or stringify
  it away. An empty `catch` block is almost always a bug.
- Prefer unchecked exceptions for programming errors; reserve checked exceptions
  for genuinely recoverable conditions the caller must handle.
- `try`-with-resources for anything `AutoCloseable` (streams, connections) — it
  closes correctly even on exception. Never rely on a `finally` to close by hand.

## Concurrency

- Prefer immutability and `java.util.concurrent` (`ConcurrentHashMap`,
  `AtomicInteger`, `ExecutorService`) over `synchronized` blocks and raw
  threads. Submit work to an executor; don't `new Thread()` per task.
- Guard shared mutable state consistently or make it immutable. Document the
  threading contract of a class that isn't obviously thread-safe.

## Naming and API conventions

- Casing: `UpperCamelCase` types, `lowerCamelCase` methods/fields,
  `SCREAMING_SNAKE_CASE` constants, lowercase dotted packages. Class names are
  nouns, methods are verbs; booleans read as questions (`isEmpty`, `hasNext`).
- Keep fields `private`; expose intent-revealing methods over raw data. Favor
  the narrowest access level that works (`package-private` before `public`).
- Javadoc public API with `@param`/`@return`/`@throws`; document *why*, not the
  obvious *what*.
- Prefer the Stream API (`filter`/`map`/`collect`) where it reads clearly; a
  plain `for` loop when side effects dominate. Don't collect just to iterate
  again. Fix static-analysis findings at the root rather than suppressing them.
- New tests mirror the source package under `src/test/java`; use JUnit 5
  (`@Test`, `assertThrows`, parameterized tests) and match the surrounding
  code's conventions over any external ideal.
