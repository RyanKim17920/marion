//! The home screen bare `marion` opens. This module holds its look: one accent ([`theme`]),
//! status as glyph plus ANSI-16 colour, dim for context, and column arithmetic ([`text`]) that
//! loses the least important words first.

pub mod text;
pub mod theme;
