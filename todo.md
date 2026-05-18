# TODO: If Expression Implementation

- [done] Pass `try_into_type.clone()` to the `else` block in `build_if`.
  The then branch already receives the expected type, but the else branch currently uses `None`, so coercion can fail for cases like `takes_i8(if true then { = 1; } else { = 2; })`.

- [done] Avoid returning `Err` from `build_if` while the builder is positioned in a block that already has a terminator.
  Current missing-else and mixed-type cases can emit `jump exit_block(...)`, return an error, and then caller recovery tries to emit into a filled block.

- [done] Validate branch result types before emitting value jumps, or otherwise make error recovery terminate/switch cleanly.
  A two-phase structure is safest: build branches, collect reachable branch end blocks and values, infer/check result type, append exit block params, then emit jumps.

- [done] Use coercion rules for branch type checks.
  When an expected result type exists, compare branch types with `can_coerce_type(&branch_type, &expected_type)` instead of exact equality.

- [done] Distinguish all-diverging ifs from missing-else fallthrough.
  `block_types.is_empty()` may mean every reachable branch diverges, but it can also mean the condition can fall through because there is no else branch.

- [done] Remove temporary unused imports once the implementation settles.
  Current warnings mention `clap::error` and `BlockArg`.
