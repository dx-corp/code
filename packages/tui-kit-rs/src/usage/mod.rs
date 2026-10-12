//! Usage & Cost Tracking Module
//!
//! This module provides comprehensive token usage and cost tracking across
//! sessions, with support for model-specific pricing, alerts, and summaries.
//!
//! # Features
//!
//! - **Real-time tracking**: Accumulates usage as responses stream in
//! - **Model pricing**: Configurable per-model token costs
//! - **Session stats**: Track usage per session with persistence
//! - **Alerts**: Configurable thresholds for cost warnings
//! - **Export**: Generate reports in various formats
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_tui_kit::usage::{UsageTracker, ModelPricing};
//!
//! let mut tracker = UsageTracker::new();
//! tracker.add_turn(usage);
//!
//! println!("Total cost: ${:.4}", tracker.total_cost());
//! println!("Summary: {}", tracker.summary());
//! ```

mod pricing;
mod tracker;

pub use pricing::{DEFAULT_PRICING, ModelPricing, PricingTier};
pub use tracker::{CostAlert, SessionUsage, TurnUsage, UsageExport, UsageStats, UsageTracker};
