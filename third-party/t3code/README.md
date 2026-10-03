# T3 Code reliability adaptations

Source: https://github.com/pingdotgg/t3code

Reviewed revision: `4f7760e6a0037b06917adaae1b8a220e3ee6e5cc`.

Maestro adapts the buffer budgeting, bounded diagnostic copies, provider capability checks, connection lifecycle coverage, lazy diff worker lifetime, and checkpoint overlap scenarios into its existing owners. The upstream application and runtime are not imported. The original MIT copyright and license are retained in `LICENSE` for these adaptations.

The native session runtime remains Rust. Checkpoint restoration retains Maestro's per-file content guards; the new overlap tests protect later edits without introducing a blanket shared-workspace ban.
