# Agent guide

Pressroom is the gatekeeper for the machines agents run work on. Read `README.md`, then
`contracts/local-v1.md`. The design and its decisions live in the Galley repository
(`docs/design/2026-11-15-device-capacity.md`, `DECISIONS.md`).

- **The contract is the product.** Change `contracts/` first, then the code, then the
  test that shows it. Never loosen a rule silently.
- **No dependencies** without an entry in `docs/references.md` (licence and version). No
  GPL code.
- **Fail towards keeping.** Where the code cannot tell whether something is in use, it
  says it is.
- **Privacy.** No secrets, keys, host names or addresses of real machines in this
  repository. Use made-up names in examples and tests.
- **Start heavy work through pressroom** once a daemon runs on the machine you are on.
- **Commits.** Explicit paths only; never `git add -A`.
