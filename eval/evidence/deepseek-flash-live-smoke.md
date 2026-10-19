# Bonsai vs DeepSeek Harness: live DeepSeek Flash smoke test

## Conclusion

**Both harnesses completed the same live file-edit task correctly.** Independent
verification confirmed the exact output bytes, rather than trusting the models'
completion messages. The test establishes that both harnesses can use DeepSeek
Flash for a real read–write–verify tool loop.

DeepSeek Harness (DSH) used fewer requests and tokens in this sample. Bonsai
recovered from a macOS command-portability error but spent two additional model
requests doing so. This is an actionable observation about the recorded
trajectory—not evidence that either harness is generally faster, cheaper, or
better at coding.

## Task and method

The same prompt was supplied to both harnesses:

> Read task.txt, replace its complete contents with exactly "value=after"
> followed by a newline, read it again, and report briefly. Do not modify other
> files or delegate.

Each workspace started with `task.txt` containing `value=before\n`. An external
runner checked that the final file contained exactly `value=after\n` (12 bytes).
The recorded pass condition covers that file, not a full workspace-diff audit.

- Model: `deepseek-flash`; thinking disabled for both.
- Environment: macOS ARM64, separate workspace and application-home directories.
- Process timeout: 120 seconds for each harness; neither timed out.
- Bonsai: version 0.4.2 release binary, direct DeepSeek OpenAI Chat transport,
  self-review disabled, yolo autonomy, recovery isolation off, eight-turn cap.
- DSH: checkout `687f282e86`, package version `0.2.1-rc.2`, shipped headless CLI
  launched from source, direct DeepSeek Messages transport, 4,096-token
  per-request output cap, workspace configured through a profile overlay.
- System prompts and tool inventories remained harness-native. This compares
  product behavior under the same user task, not identical wire requests.

## Measured results

| Metric | Bonsai | DeepSeek Harness |
| --- | ---: | ---: |
| Exact final file bytes | PASS | PASS |
| Exit code | 0 | 0 |
| End-to-end process time | 6.465 s | 5.908 s |
| Model requests | 6 | 4 |
| Input tokens, including cached input | 70,067 | 45,896 |
| Cached input tokens | 54,784 | 34,048 |
| Uncached input tokens | 15,283 | 11,848 |
| Output tokens | 276 | 263 |
| Total tokens | 70,343 | 46,159 |
| Application-reported cost | $0.002625 | Not emitted in headless events |

Token counts are cumulative across requests, including repeated conversation
history. DSH reports uncached input and cache reads separately; its input total
above adds both. Bonsai's prompt-token total already includes cache reads.
Bonsai's cost is its own reported estimate, not a reconciled provider invoice;
there is no comparable DSH cost measurement here.

## Recorded trajectories

### Bonsai

1. Read `task.txt`.
2. Write the requested replacement.
3. Attempt `cat -A task.txt`; fail because macOS `cat` does not support `-A`.
4. Recover with `od -c task.txt`, confirming the trailing newline.
5. Read `task.txt` again.
6. Return the final answer.

The recovery succeeded and the external byte check passed. Bonsai's completion
report nevertheless retains a failed-tool caveat and marks automatic test
verification as skipped/incomplete because that lane was disabled. A successful
file result must not be confused with a completely clean tool trajectory.

### DeepSeek Harness

1. Read `task.txt`.
2. Write the requested replacement.
3. Read `task.txt` again.
4. Return the final answer.

All three reported tool operations completed successfully and the external byte
check passed.

## Setup failures and timing boundaries

The initial DSH launch exited before task execution because generated host
modules and the native macOS module were missing. Dependency installation alone
was insufficient. Building the native module and running the host-library build
resolved startup; the subsequent live task completed successfully.

The measured successful DSH time excludes installation and build time. Bonsai's
successful measurement came from the earlier launch and was not repeated after
DSH setup was repaired. Consequently, the successful measurements were not
simultaneous. The initial startup failure is a setup outcome, not a model task
failure, and is retained separately in the evidence.

## What follows from these runs

1. **The live integration works in both harnesses.** Both reached the expected
   file state through real provider calls and real tools.
2. **Unnecessary verification steps can amplify token usage.** In this trace,
   Bonsai's shell detour increased the trajectory from four to six requests.
   Repeated prompt/history tokens account for much more usage than final output.
3. **Portability matters even for tiny tasks.** Prefer native read tools or
   portable verification commands when inspecting bytes on macOS and Linux.
   This run identifies a candidate improvement; no product fix is claimed here.
4. **Recovery and cleanliness are separate outcomes.** Bonsai recovered
   correctly, but its failed-tool caveat is still relevant evidence.
5. **DSH source setup requires generated artifacts.** A working `--help` command
   did not prove that the full task runtime was ready.

## Limits and recommended next experiment

This is one small task per harness, with no repeated trials or statistical
analysis. It does not evaluate multi-file coding, test repair, long-context
handling, retries, cancellation, or the new provider-stream capture/replay
feature. No stream fixture was captured during these runs.

The protocols, prompts, tools, cache state, launch paths, and budget policies
differ. The observed 0.557-second gap is not a defensible general speed ranking.
Likewise, token totals alone do not establish a general cost ranking.

Before making performance claims, repeat a small multi-task set with alternating
execution order, identical starting fixtures, external outcome graders, matched
time/cost budgets, and several trials. Report task success, median/tail time,
request counts, cached/uncached input, output, and comparable provider costs.
Keep setup failures separate from task failures and preserve failed-tool/recovery
counts. These are recommendations only; no additional paid runs were performed
for this document.

## Evidence and reproducibility

Local artifacts (under ignored `target/`; not durable or checked in):

- Bonsai result and output:
  `target/comparison/live-1792349723697/bonsai/result.json`,
  `target/comparison/live-1792349723697/bonsai/stdout.jsonl`.
- Bonsai per-request usage and tool outcomes:
  `target/comparison/live-1792349723697/bonsai/home/bonsai.db`.
- Initial DSH startup failure:
  `target/comparison/live-1792349723697/dsh/stderr.log`.
- Successful DSH result and events:
  `target/comparison/live-1792349856147/dsh/result.json`,
  `target/comparison/live-1792349856147/dsh/stdout.jsonl`.
- Local launcher: `target/comparison/live-smoke.mjs`.

The measurements and relevant trajectory details are transcribed here so the
conclusions remain understandable if build artifacts are cleaned. This document
does not by itself provide a fully archived, independently reproducible benchmark.
Credentials were loaded in memory from the project `.env`; captured stdout and
stderr replaced exact occurrences of the key before being saved. No key is
included in this report.
