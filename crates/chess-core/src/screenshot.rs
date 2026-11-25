//! Turning a screenshot of a chess app (Chess.com, Duolingo, ...) into a
//! position with Claude Haiku. The result states its assumptions, because
//! side to move, castling rights and board orientation are often not
//! visible.

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use shakmaty::{Chess, Position};

use crate::{
    claude::{ClaudeClient, ClaudeError, Effort, Message, Model, Request, Usage},
    import::{parse_fen, parse_pgn},
    tree::GameTree,
};

const SYSTEM: &str = "\
You read chess positions from screenshots of chess apps and websites. Report exactly what is \
visible. Some apps show the board from Black's side: decide the orientation from the rank and \
file labels, or from where the kings and pawns start, before reading squares. If a move list is \
visible, transcribe it as PGN movetext. If the side to move, castling rights or en passant square \
cannot be seen, choose the most likely value and say so in `assumptions`.";

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "fen": {"type": "string", "description": "Full FEN of the position shown"},
            "orientation": {"type": "string", "enum": ["white_at_bottom", "black_at_bottom"]},
            "side_to_move": {"type": "string", "enum": ["white", "black"]},
            "castling": {"type": "string", "description": "FEN castling field, e.g. KQkq or -"},
            "pgn": {"type": "string", "description": "Visible move list as PGN movetext, or empty"},
            "assumptions": {"type": "array", "items": {"type": "string"}},
            "confidence": {"type": "string", "enum": ["high", "medium", "low"]}
        },
        "required": ["fen", "orientation", "side_to_move", "castling", "pgn", "assumptions", "confidence"],
        "additionalProperties": false
    })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ScreenshotReading {
    pub fen: String,
    pub orientation: String,
    pub side_to_move: String,
    pub castling: String,
    pub pgn: String,
    pub assumptions: Vec<String>,
    pub confidence: String,
}

#[derive(Debug)]
pub struct ScreenshotResult {
    pub reading: ScreenshotReading,
    /// The game to load: from the PGN when it is legal and reaches the same
    /// board as the FEN, otherwise from the FEN.
    pub tree: Result<GameTree, String>,
    pub used_pgn: bool,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub enum ScreenshotError {
    #[error("unsupported image type (use PNG, JPEG, GIF or WebP)")]
    UnsupportedImage,
    #[error(transparent)]
    Claude(#[from] ClaudeError),
    #[error("could not read Claude's answer: {0}")]
    Parse(String),
}

/// Detects the image media type from its first bytes.
pub fn media_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

pub fn request(image: &[u8]) -> Result<Request, ScreenshotError> {
    let media = media_type(image).ok_or(ScreenshotError::UnsupportedImage)?;
    let data = base64::engine::general_purpose::STANDARD.encode(image);
    Ok(Request {
        model: Model::Haiku,
        system: SYSTEM.to_string(),
        messages: vec![Message::user_blocks(vec![
            json!({"type": "image", "source": {"type": "base64", "media_type": media, "data": data}}),
            json!({"type": "text", "text": "Read the chess position in this screenshot."}),
        ])],
        effort: Effort::Low,
        max_tokens: 4_000,
        output_schema: Some(schema()),
    })
}

/// Sends the screenshot to Claude once; afterwards only the FEN is used.
pub fn read_screenshot(client: &ClaudeClient, image: &[u8]) -> Result<ScreenshotResult, ScreenshotError> {
    let reply = client.send(&request(image)?)?;
    let reading: ScreenshotReading =
        serde_json::from_str(reply.text.trim()).map_err(|e| ScreenshotError::Parse(e.to_string()))?;
    let (tree, used_pgn) = build_tree(&reading);
    Ok(ScreenshotResult {
        reading,
        tree,
        used_pgn,
        usage: reply.usage,
    })
}

/// Validates the reading with shakmaty and builds the game to load.
pub fn build_tree(reading: &ScreenshotReading) -> (Result<GameTree, String>, bool) {
    let from_fen = parse_fen(&reading.fen).map_err(|e| e.to_string());
    if !reading.pgn.trim().is_empty()
        && let Ok(tree) = parse_pgn(&reading.pgn)
    {
        let same_board = match &from_fen {
            Ok(pos) => tree.position().board() == pos.board(),
            Err(_) => true,
        };
        if !tree.is_empty() && same_board {
            return (Ok(tree), true);
        }
    }
    (from_fen.map(GameTree::new), false)
}

/// Returns the position with the other side to move, if that is legal.
pub fn with_other_side_to_move(pos: &Chess) -> Option<Chess> {
    pos.clone().swap_turn().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(fen: &str, pgn: &str) -> ScreenshotReading {
        ScreenshotReading {
            fen: fen.into(),
            orientation: "white_at_bottom".into(),
            side_to_move: "white".into(),
            castling: "KQkq".into(),
            pgn: pgn.into(),
            assumptions: vec![],
            confidence: "high".into(),
        }
    }

    #[test]
    fn media_types() {
        assert_eq!(media_type(&[0x89, b'P', b'N', b'G', 0, 0]), Some("image/png"));
        assert_eq!(media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(media_type(b"hello"), None);
    }

    #[test]
    fn prefers_matching_pgn() {
        let fen = "rnbqkbnr/pp1ppppp/8/2p5/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2";
        let (tree, used_pgn) = build_tree(&reading(fen, "1. e4 c5"));
        assert!(used_pgn);
        assert_eq!(tree.unwrap().san_history(1), ["e4"]);
        // A move list that does not match the board is ignored.
        let (tree, used_pgn) = build_tree(&reading(fen, "1. d4 d5"));
        assert!(!used_pgn);
        assert!(tree.unwrap().is_empty());
        let (tree, _) = build_tree(&reading("garbage", ""));
        assert!(tree.is_err());
    }

    #[test]
    fn image_request_shape() {
        let png = [0x89, b'P', b'N', b'G', 1, 2, 3];
        let body = request(&png).unwrap().body();
        assert_eq!(body["model"], "claude-haiku-5-5");
        assert_eq!(
            body["messages"][0]["content"][0]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(body["output_config"]["format"]["schema"]["required"][0], "fen");
    }
}
