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
  - `Given` establishes state and inputs.
  - `When` performs the single behavior under test.
  - `Then` asserts externally observable results.
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
