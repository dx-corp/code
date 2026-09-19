use std::time::SystemTime;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use maestro_tui::components::ChatView;
use maestro_tui::state::{AppState, Message, MessageKind, MessageRole};
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use std::hint::black_box;

const MESSAGE_COUNT: usize = 1_000;
const AREA: Rect = Rect::new(0, 0, 100, 40);

fn transcript() -> AppState {
    let mut state = AppState::default();
    state.zen_mode = true;
    state.messages = (0..MESSAGE_COUNT)
        .map(|index| Message {
            id: format!("message-{index}"),
            role: MessageRole::Assistant,
            kind: MessageKind::Regular,
            content: format!(
                "## Result {index}\n\nProcessed `src/module_{index}.rs` successfully.\n\n- cached output\n- deterministic wrapping\n- **complete**"
            ),
            thinking: String::new(),
            streaming: index + 1 == MESSAGE_COUNT,
            tool_calls: vec![],
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        })
        .collect();
    state
}

fn render(state: &AppState) {
    let mut buffer = Buffer::empty(AREA);
    ChatView::new(state).render(AREA, &mut buffer);
    black_box(buffer);
}

fn bench_message_layout(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_layout/1000_messages");

    group.bench_function("cold", |b| {
        b.iter_batched(
            transcript,
            |state| render(black_box(&state)),
            BatchSize::SmallInput,
        );
    });

    let steady = transcript();
    render(&steady);
    group.bench_function("steady", |b| b.iter(|| render(black_box(&steady))));

    let mut streaming = transcript();
    render(&streaming);
    let mut grow = true;
    group.bench_function("streaming_tail", |b| {
        b.iter(|| {
            if grow {
                streaming.messages.last_mut().unwrap().content.push('x');
            } else {
                streaming.messages.last_mut().unwrap().content.pop();
            }
            grow = !grow;
            render(black_box(&streaming));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_message_layout);
criterion_main!(benches);
