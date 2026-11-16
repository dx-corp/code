//! The final check receives what the person asked this turn.

use super::*;
use std::sync::{Arc, Mutex};

/// A check that decides what to show from the person's request (the page a
/// Slack turn asked for) gets this turn's message and steer, never an
/// earlier turn's.
#[tokio::test]
async fn the_final_check_is_given_this_turns_request() {
    #[derive(Default)]
    struct Capture(Mutex<Vec<String>>);
    impl dex_loop::FinalCheck for Capture {
        fn check<'a>(
            &'a self,
            _: &'a dex_loop::ThreadId,
            asked: &'a str,
            reply: String,
            _: &'a [dex_loop::TurnOutput],
            _: &'a [ToolName],
        ) -> futures_util::future::BoxFuture<'a, dex_loop::FinalReply> {
            self.0.lock().expect("capture").push(asked.to_owned());
            Box::pin(async move { reply.into() })
        }
    }
    let log = FakeLog::default();
    log.start_turn("old", "is Acme's SOC 2 report expired?");
    log.host_append(Event::final_text("No."));
    let mut ctx = log.start_turn("current", "show me the vendors page");
    let model = FakeModel::new(vec![vec![text("Here it is.")]]);
    let tools = FakeTools::new(vec![]);
    let capture = Arc::new(Capture::default());
    let engine = support::engine(&log, &model, &tools, budget()).with_final_check(capture.clone());
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        *capture.0.lock().expect("capture"),
        ["show me the vendors page"]
    );
}
