//! An adversarial model against the real host: seeded scripts mix gated
//! writes, replays under stale, bogus, swapped or mutated confirmation ids,
//! questions bound to the wrong call, ungated reads and random decisions.
//!
//! Whatever the script, a gated write runs at most once per Confirm answer
//! bound to that exact command, never without one, and never in a headless
//! turn. The oracle reads only the log and the disk, not the policy.
//!
//! `DEX_HOST_FUZZ_CASES` (default 48) and `DEX_HOST_FUZZ_SEED` widen a run;
//! a failure names the seed and case that reproduce it.

use super::*;

struct Fuzz(u64);

impl Fuzz {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Where a script takes the confirmation id it sends.
#[derive(Clone, Copy, Debug)]
enum Id {
    /// The newest unexecuted preview.
    Preview,
    /// The preview the last `user.ask` was bound to.
    Asked,
    /// Any call id already in the evidence, executed or not.
    Seen,
    Bogus,
}

impl Id {
    fn resolve(self, ctx: &Context, salt: u64) -> String {
        let evidence = ctx.tool_evidence();
        let found = match self {
            Self::Preview => evidence
                .iter()
                .rev()
                .find(|record| ctx.is_unexecuted_preview(&record.call.id))
                .map(|record| record.call.id.as_str().to_owned()),
            Self::Asked => ctx
                .history()
                .iter()
                .rev()
                .find_map(|entry| match &entry.message {
                    dex_loop::Message::Assistant { calls, .. } => calls
                        .iter()
                        .find(|call| call.tool.as_str() == USER_ASK)
                        .and_then(|call| call.args[CONFIRMATION_FIELD]["proposal_call_id"].as_str())
                        .map(str::to_owned),
                    _ => None,
                }),
            Self::Seen if !evidence.is_empty() => Some(
                evidence[(salt as usize) % evidence.len()]
                    .call
                    .id
                    .as_str()
                    .to_owned(),
            ),
            Self::Seen | Self::Bogus => None,
        };
        found.unwrap_or_else(|| format!("bogus-{salt}"))
    }
}

#[derive(Clone, Debug)]
enum Move {
    /// A gated write with no confirmation.
    Write(usize),
    /// A gated write carrying a confirmation id, optionally with an extra
    /// argument the person never saw.
    Replay(usize, Id, bool),
    /// `user.ask` bound to some call.
    Ask(Id),
    /// An ungated read.
    Read,
}

fn arbitrary_move(fuzz: &mut Fuzz) -> Move {
    let id = [Id::Preview, Id::Asked, Id::Seen, Id::Bogus][fuzz.below(4)];
    match fuzz.below(7) {
        0 | 1 => Move::Write(fuzz.below(2)),
        2 | 3 => Move::Replay(fuzz.below(2), id, fuzz.below(4) == 0),
        4 | 5 => Move::Ask(id),
        _ => Move::Read,
    }
}

fn marker(workspace: &Path, which: usize) -> std::path::PathBuf {
    workspace.join(format!("marker-{which}"))
}

/// Appends a line, so the file counts how often the write ran.
fn write_args(workspace: &Path, which: usize) -> Value {
    json!({"command": format!("echo ran >> {}", marker(workspace, which).display())})
}

fn step(workspace: &Path, play: Move, salt: u64) -> Step {
    let workspace = workspace.to_path_buf();
    Box::new(move |ctx| {
        let (name, args) = match &play {
            Move::Write(which) => ("bash".to_owned(), write_args(&workspace, *which)),
            Move::Replay(which, id, extra) => {
                let mut args = write_args(&workspace, *which);
                args[CONFIRMATION_FIELD] = Value::String(id.resolve(ctx, salt));
                if *extra {
                    args["timeout"] = json!(30);
                }
                ("bash".to_owned(), args)
            }
            Move::Ask(id) => (
                USER_ASK.to_owned(),
                json!({
                    "question": "Run it?",
                    CONFIRMATION_FIELD: {"proposal_call_id": id.resolve(ctx, salt)},
                }),
            ),
            Move::Read => (
                "read".to_owned(),
                json!({"path": workspace.join("notes.txt").display().to_string()}),
            ),
        };
        vec![Ok(ModelChunk::ToolCall {
            name: ToolName::new(&name),
            args,
        })]
    })
}

fn cases() -> u64 {
    std::env::var("DEX_HOST_FUZZ_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(48)
}

#[tokio::test]
async fn no_script_runs_a_gated_write_beyond_what_the_person_confirmed() {
    let seed = std::env::var("DEX_HOST_FUZZ_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0xC0F1_A11E_u64);
    for case in 0..cases() {
        let mut fuzz = Fuzz((seed ^ (case + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1);
        let headless = fuzz.below(5) == 0;
        let moves: Vec<Move> = (0..2 + fuzz.below(9))
            .map(|_| arbitrary_move(&mut fuzz))
            .collect();
        let decisions: Vec<bool> = (0..moves.len()).map(|_| fuzz.below(2) == 0).collect();
        let context = format!(
            "DEX_HOST_FUZZ_SEED={seed} case {case}: headless={headless} moves={moves:?} \
             confirms={decisions:?}"
        );

        let workspace = TempDir::new().expect("workspace");
        std::fs::write(workspace.path().join("notes.txt"), "shopping list").expect("seed");
        let mut steps: Vec<Step> = moves
            .iter()
            .map(|play| step(workspace.path(), play.clone(), fuzz.next()))
            .collect();
        steps.push(answer("done"));
        let tools = host_tools(workspace.path(), ApprovalMode::Selective);
        let mode = if headless {
            TurnMode::Headless
        } else {
            TurnMode::Interactive
        };
        let turn = Turn::start(tools, mode, steps).await;

        let mut decisions = decisions.into_iter();
        let mut exit = turn.run().await;
        for _ in 0..moves.len() + 2 {
            let Exit::Asked(asked) = &exit else { break };
            let decision = if decisions.next().unwrap_or(false) {
                ConfirmationDecision::Confirm
            } else {
                ConfirmationDecision::Decline
            };
            turn.decide(asked, decision).await;
            exit = turn.run().await;
        }
        assert!(
            !matches!(exit, Exit::Asked(_)),
            "the turn kept asking: {context}"
        );

        let events = turn.events().await;
        for which in 0..2 {
            let digest = args_digest(&write_args(workspace.path(), which));
            let confirmed = events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        Event::Answer {
                            confirmation_decision: ConfirmationDecision::Confirm,
                            args_digest,
                            ..
                        } if *args_digest == digest
                    )
                })
                .count();
            let ran = std::fs::read_to_string(marker(workspace.path(), which))
                .map(|text| text.lines().count())
                .unwrap_or(0);
            assert!(
                ran <= confirmed,
                "marker-{which} ran {ran} times on {confirmed} confirmations: {context}"
            );
            if headless {
                assert_eq!(ran, 0, "a headless turn ran a gated write: {context}");
            }
        }
    }
}
