# Reviewing a pull request

You review a PR written by **another** agent. Do not modify the code or push commits.
Post exactly one review comment with `gh pr comment <number>`.

## Check, in this order

1. **Contract**: does the PR satisfy every acceptance criterion of the linked Linear issue? Anything missing or extra?
2. **Correctness**: logic errors, wrong ordering of lifecycle steps, race conditions, unhandled error paths.
3. **Resource hygiene**: can any path leak a process, netns, TAP, socket or file? Is teardown idempotent?
4. **`unsafe`**: is each block necessary, minimal, and correctly justified by its `// SAFETY:` comment?
5. **Tests**: do they test behavior, or only execute code? Could they pass with a broken implementation? Was any test weakened or ignored?
6. **Architecture**: dependency direction (AGENTS.md section 4), crate boundaries, no system code in `dylos-core`.
7. **Scope**: unrelated changes, drive-by refactors, unjustified dependencies, commands not going through `just`.
8. **Security**: secrets, privilege usage, host side effects outside the lab's namespace, anything inappropriate for a public repo.
9. **Branch hygiene**: the PR targets `main` from an issue branch; no commits made directly on `main`.

## Comment format

```markdown
## Review: <model name>

**Verdict:** ready for human review | changes needed | blocked (needs human decision)

### Must fix
- file:line: problem, and why it matters

### Should fix
- ...

### Questions for the human
- ...

### Where to look first
The 1 to 3 places a human reviewer should read most carefully.
```

Be specific and brief. Do not praise. Do not restate the diff.
