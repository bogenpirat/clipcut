//! ClipCut internals, exposed as a library so the pure logic can be tested
//! independently of the GUI binary.

pub mod config;
pub mod encode;
pub mod export;
pub mod library;
pub mod marks;
pub mod media;
pub mod player;
pub mod timecode;
