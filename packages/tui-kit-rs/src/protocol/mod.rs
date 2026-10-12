//! Protocol Definitions and Styling Types
//!
//! This module provides protocol-level type definitions and styling utilities
//! for text rendering in the TUI. It defines the shared vocabulary for how
//! styled text is represented and rendered across different components.
//!
//! # Overview
//!
//! The protocol module serves as a bridge between raw text content and
//! terminal-rendered output. It provides:
//!
//! - **Style Definitions**: Types for colors, attributes, and text decoration
//! - **Span Types**: Styled text segments that can be combined into lines
//! - **Conversion Utilities**: Transform protocol types to ratatui rendering types
//!
//! # Design Philosophy
//!
//! The protocol layer is intentionally minimal and focused on:
//!
//! 1. **Portability**: Types that could be serialized for IPC if needed
//! 2. **Simplicity**: Only essential styling information, no rendering logic
//! 3. **Composability**: Small types that combine into larger structures
//!
//! # Rust Concepts
//!
//! - **Type Aliases**: Provides semantic names for common types
//! - **From/Into Traits**: Ergonomic conversions between protocol and ratatui types
//! - **Builder Pattern**: Fluent API for constructing styled text

mod styles;

pub use styles::*;
