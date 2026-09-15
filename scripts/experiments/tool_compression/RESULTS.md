# Native compiler projection: repair pilot

Do not enable this representation as default compression. In eight matched
synthetic Rust repairs, both ordinary compiler output and the native delta view
passed all hidden tests. The richer native view cost more.

| Metric | Baseline | Native delta |
| --- | ---: | ---: |
| Hidden-test repair passes | 8/8 | 8/8 |
| Total prompt tokens including cache reads | 229,049 | 489,079 |
| Reported model cost | $0.099124 | $0.261002 |
| Generated compiler-view bytes | 84,392 | 376,842 |
| Truncated check outputs | 0/16 | 8/18 |

The candidate used 2.14 times the prompt tokens and 2.63 times the reported model
cost. It retains child locations and suggestions in repeated objects, and
reprints removals when all errors disappear. This differs from the earlier,
more lossy Python diagnostic-comparison pilot; that pilot's savings do not prove
a benefit for this native implementation.

Both arms used the same frozen native binary, GLM-5.3 model, requirements, and
initial source. Four unsigned-arithmetic requirements were tested at two sizes
of repeated type diagnostics. The final library was compiled separately from
controller-owned hidden tests, which verified expected test names and counts.
Every selected run checked the original source before editing and checked again
after editing. Truncation was retained as a treatment outcome.

The cohort comprises seven complete original pairs and both arms of a case-07
replay after its candidate failed identity startup before inference and hit a
cleanup bug. The cleanup bug now has regression coverage. No failed model answer
or repair was selected for retry. All initial attempts were retained. The
original strict full-view-delivery condition was not met; these are descriptive
end-to-end outcomes, not an untruncated-protocol pass or a general coding-quality
estimate. Timing was confounded by provider conditions and local verification.

Run `repair.py` as documented in README.md to produce the manifests, source,
compiler observations, tool receipts, usage, and grading logs for a new paired
experiment. Keep the utility opt-in while testing a more compact representation
or a policy that declines to expand ordinary output.
