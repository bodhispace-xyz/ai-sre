# Rust Documentation and Test Style

The purpose of documentation is to preserve context that types and code cannot
express clearly. Documentation is not measured by comment count.

## Rust files and modules

- Every hand-written Rust source file starts with a `//!` crate or module brief.
  State why the module exists, what responsibility it owns, and any important
  authority or dependency boundary. Keep the brief short when the boundary is
  simple. Generated Rust files are exempt and must identify their generator.
- Public and safety-relevant items use `///` documentation. Describe the
  contract, invariants, failure behavior, and authority. Add `# Errors`,
  `# Panics`, or `# Safety` sections only when they apply.
- Local implementation comments use `//` and explain rationale, invariants,
  protocol quirks, threat assumptions, or external constraints. Do not restate
  code that is already clear from names and types.
- Prefer consecutive `//` lines for longer local explanations. Use block
  comments only when they make generated or structurally embedded material
  clearer.
- Update or remove comments when behavior changes. A stale comment is a defect.
- Public missing documentation is denied by the Rust compiler. Review still
  checks whether documentation explains the real contract rather than merely
  satisfying the lint.

## Tests

- Test names describe observable behavior and read as specifications.
- Contract, integration, acceptance, and multi-step tests use explicit
  `// Given`, `// When`, and `// Then` sections:
  - `Given` must state the relevant setup and inputs in plain language.
  - `When` must state the single behavior being exercised, not merely repeat
    the next line of code.
  - `Then` must state the externally observable outcome being protected.
- Never use empty ceremonial labels such as `// Given` followed only by code.
  The comment must add context that would be missing from the code itself.
  For example:

  ```rust
  // Given a LogQL expression and the policy-selected Loki datasource.
  let query = GcxQuery::logs("{app=\"api\"}", "loki");

  // When the typed request is lowered to the child-process argument vector.
  let argv = query.argv();

  // Then only the documented logs-query capability is present, never `gcx api`.
  assert_eq!(argv.first().map(String::as_str), Some("logs"));
  ```

  This is useful because the comments explain the safety intent; the following
  code can change during refactoring without weakening the scenario's meaning.
- A short unit test whose setup, action, and assertion are already unmistakable
  may use clear names and blank-line separation instead of ceremonial comments.
- Do not hide important behavior inside generic Given/When/Then helpers. Extract
  helpers only when their names preserve the domain meaning of the scenario.
- Comments do not compensate for unclear code. Prefer domain types, precise
  names, small tests, and one behavioral reason for failure.

## References

- [Rust Style Guide: comments](https://doc.rust-lang.org/style-guide/#comments)
- [Rust API Guidelines: documentation](https://rust-lang.github.io/api-guidelines/documentation.html)
- [Rust Reference: comments and rustdoc](https://doc.rust-lang.org/reference/comments.html)
- [Cucumber: Given, When, Then](https://cucumber.io/docs/gherkin/reference/)
