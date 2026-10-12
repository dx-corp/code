//! Shimmer animation effect
//!
//! Thin re-export of [`crate::shimmer`] so call sites under `effects::`
//! share one implementation (Deixic palette, rest/pulse, diagonal sheen).

pub use crate::shimmer::{
    DEIXIC_SOFT, DEIXIC_VIOLET, SHIMMER_FPS, ShimmerConfig, anim_phase_secs,
    diagonal_shimmer_lines, shimmer_frame, shimmer_line, shimmer_spans, shimmer_spans_at_time,
    shimmer_spans_with_config, shine_opacity,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shimmer_produces_spans() {
        let spans = shimmer_spans("Working");
        assert_eq!(spans.len(), 7);
    }

    #[test]
    fn shimmer_empty_string() {
        let spans = shimmer_spans("");
        assert!(spans.is_empty());
    }
}
