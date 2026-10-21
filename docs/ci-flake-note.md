# A CI job that reports every test passing and still fails

Seen twice on `gridxcel-rs`, both times on `test (serde)`, both times on exactly one of the three
operating systems at a time — the same commit and the same feature set pass on the other two, and a
re-run of the failed job passes.

## What the log shows

Every suite reports `ok`, and the last line is the `Doc-tests` summary:

```
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.73s
```

There is no panic, no assertion failure, no `error: test failed`, and no non-zero exit printed
anywhere in the job log. `--log-failed` returns only lines that say `ok`.

## What was checked

- **Not a flaky test.** The log would contain the failure: `cargo test` prints `FAILED`, the test
  name, and the panic message for any test that fails, and none of those strings appear.
- **Not a shared scratch file.** `tests/common/mod.rs` names every scratch file
  `{name}-{pid}-{counter}.tmp`, so two test binaries in one job cannot collide.
- **Not the memory-threshold tests.** `peak_allocation`, `amplification` and `payload_budget`
  assert on a process-wide counting allocator, which is genuinely sensitive to a sibling test
  allocating concurrently — but those are separate test binaries, and each binary's own tests run
  in one process. Their thresholds (4–400 MiB) are far above any incidental allocation.
- **Not the code.** Re-running the identical job with no change passed.

## Conclusion

The runner reported failure while the work it ran succeeded. Retrying the job is the correct
response, and was done. Worth noting rather than dismissing, because "it passed on re-run" is also
what a genuinely flaky test looks like — the distinction is that a flaky test leaves evidence in
the log, and this leaves none.

If it happens a third time, the next thing to check is the job's exit code propagation (a failing
post-step or an OOM kill of the runner would also produce this shape), since the test output itself
is clean.
