//! Loading positions from FEN and games from PGN.

use std::ops::ControlFlow;

use pgn_reader::{RawTag, Reader, SanPlus, Skip, Visitor};
use shakmaty::{CastlingMode, Chess, Position, fen::Fen};

use crate::tree::{GameTree, NodeId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImportError {
    #[error("not a valid FEN: {0}")]
    Fen(String),
    #[error("the FEN describes an impossible position: {0}")]
    Position(String),
    #[error("could not read the PGN: {0}")]
    Pgn(String),
    #[error("illegal move {san} after {before}")]
    IllegalMove { san: String, before: String },
    #[error("no FEN or PGN found in the text")]
    Unrecognized,
}

/// Parses and validates a FEN.
pub fn parse_fen(fen: &str) -> Result<Chess, ImportError> {
    let parsed = Fen::from_ascii(fen.trim().as_bytes()).map_err(|e| ImportError::Fen(e.to_string()))?;
    parsed
        .into_position(CastlingMode::Standard)
        .map_err(|e| ImportError::Position(e.to_string()))
}

/// Parses the first game of a PGN, keeping variations.
pub fn parse_pgn(pgn: &str) -> Result<GameTree, ImportError> {
    let mut reader = Reader::new(pgn.as_bytes());
    let mut builder = TreeBuilder::default();
    match reader.read_game(&mut builder) {
        Ok(Some(result)) => result,
        Ok(None) => Err(ImportError::Unrecognized),
        Err(e) => Err(ImportError::Pgn(e.to_string())),
    }
}

/// Accepts either a FEN or a PGN (or bare movetext like `1. e4 e5 2. Nf3`).
pub fn load_text(text: &str) -> Result<GameTree, ImportError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(ImportError::Unrecognized);
    }
    if looks_like_fen(trimmed) {
        return parse_fen(trimmed).map(GameTree::new);
    }
    let tree = parse_pgn(trimmed)?;
    if tree.is_empty() && !trimmed.contains("[FEN") {
        return Err(ImportError::Unrecognized);
    }
    Ok(tree)
}

fn looks_like_fen(s: &str) -> bool {
    let first = s.split_whitespace().next().unwrap_or("");
    !s.contains('\n') && first.matches('/').count() == 7 && !s.contains('[')
}

#[derive(Default)]
struct TreeBuilder {
    error: Option<ImportError>,
}

struct Movetext {
    tree: GameTree,
    current: NodeId,
    /// Where to return to when a variation closes.
    stack: Vec<NodeId>,
}

impl Visitor for TreeBuilder {
    type Tags = (Option<Chess>, Vec<(String, String)>);
    type Movetext = Movetext;
    type Output = Result<GameTree, ImportError>;

    fn begin_tags(&mut self) -> ControlFlow<Self::Output, Self::Tags> {
        ControlFlow::Continue((None, Vec::new()))
    }

    fn tag(&mut self, tags: &mut Self::Tags, name: &[u8], value: RawTag<'_>) -> ControlFlow<Self::Output> {
        let name = String::from_utf8_lossy(name).to_string();
        let value = value.decode_utf8_lossy().to_string();
        if name == "FEN" {
            match parse_fen(&value) {
                Ok(pos) => tags.0 = Some(pos),
                Err(e) => return ControlFlow::Break(Err(e)),
            }
        }
        tags.1.push((name, value));
        ControlFlow::Continue(())
    }

    fn begin_movetext(&mut self, tags: Self::Tags) -> ControlFlow<Self::Output, Self::Movetext> {
        let mut tree = GameTree::new(tags.0.unwrap_or_default());
        tree.headers = tags.1;
        ControlFlow::Continue(Movetext {
            tree,
            current: GameTree::ROOT,
            stack: Vec::new(),
        })
    }

    fn san(&mut self, mt: &mut Self::Movetext, san_plus: SanPlus) -> ControlFlow<Self::Output> {
        let pos = &mt.tree.node(mt.current).position;
        let m = match san_plus.san.to_move(pos) {
            Ok(m) => m,
            Err(_) => {
                let before = mt.tree.numbered_history(mt.current);
                let before = if before.is_empty() {
                    "the start".to_string()
                } else {
                    before
                };
                return ControlFlow::Break(Err(ImportError::IllegalMove {
                    san: san_plus.to_string(),
                    before,
                }));
            }
        };
        match mt.tree.add_move(mt.current, m) {
            Ok(id) => {
                mt.current = id;
                ControlFlow::Continue(())
            }
            Err(e) => ControlFlow::Break(Err(ImportError::Pgn(e.to_string()))),
        }
    }

    fn begin_variation(&mut self, mt: &mut Self::Movetext) -> ControlFlow<Self::Output, Skip> {
        // A variation replaces the move just played, so it starts from that
        // move's parent.
        mt.stack.push(mt.current);
        if let Some(parent) = mt.tree.node(mt.current).parent {
            mt.current = parent;
        }
        ControlFlow::Continue(Skip(false))
    }

    fn end_variation(&mut self, mt: &mut Self::Movetext) -> ControlFlow<Self::Output> {
        if let Some(back) = mt.stack.pop() {
            mt.current = back;
        }
        ControlFlow::Continue(())
    }

    fn end_game(&mut self, mt: Self::Movetext) -> Self::Output {
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        let mut tree = mt.tree;
        tree.to_end();
        Ok(tree)
    }
}

/// Checks that a position has a legal move to analyse.
pub fn ensure_playable(pos: &Chess) -> Result<(), String> {
    if pos.is_checkmate() {
        Err("the side to move is checkmated".into())
    } else if pos.is_stalemate() {
        Err("the side to move is stalemated".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::fen_of;

    #[test]
    fn fen_round_trip() {
        let fen = "r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - 2 3";
        let pos = parse_fen(fen).unwrap();
        assert_eq!(fen_of(&pos), fen);
        let tree = load_text(fen).unwrap();
        assert!(tree.is_empty());
    }

    #[test]
    fn bad_fens() {
        assert!(matches!(parse_fen("not a fen"), Err(ImportError::Fen(_))));
        // Two white kings.
        assert!(matches!(
            parse_fen("4k3/8/8/8/8/8/8/K3K3 w - - 0 1"),
            Err(ImportError::Position(_))
        ));
    }

    #[test]
    fn pgn_with_variations() {
        let pgn = r#"[Event "Test"]
[White "A"]
[Black "B"]

1. e4 c5 (1... e5 2. Nf3 Nc6) 2. Nf3 d6 {comment} 3. d4 cxd4 4. Nxd4 Nf6 5. Nc3 a6 *"#;
        let tree = parse_pgn(pgn).unwrap();
        assert_eq!(
            tree.numbered_history(tree.current()),
            "1. e4 c5 2. Nf3 d6 3. d4 cxd4 4. Nxd4 Nf6 5. Nc3 a6"
        );
        let e4 = tree.node(GameTree::ROOT).children[0];
        assert_eq!(tree.node(e4).children.len(), 2);
        let e5 = tree.node(e4).children[1];
        assert_eq!(tree.node(e5).san.as_deref(), Some("e5"));
        assert_eq!(
            tree.san_history(tree.node(tree.node(e5).children[0]).children[0]),
            ["e4", "e5", "Nf3", "Nc6"]
        );
        assert!(tree.headers.iter().any(|(k, v)| k == "White" && v == "A"));
    }

    #[test]
    fn bare_movetext_and_fen_tag() {
        let tree = load_text("1. d4 d5 2. c4").unwrap();
        assert_eq!(tree.san_history(tree.current()), ["d4", "d5", "c4"]);

        let pgn = "[SetUp \"1\"]\n[FEN \"4k3/8/8/8/8/8/4P3/4K3 w - - 0 1\"]\n\n1. e4 Kd7 *";
        let tree = load_text(pgn).unwrap();
        assert_eq!(tree.san_history(tree.current()), ["e4", "Kd7"]);
        assert!(
            tree.to_pgn()
                .contains("[FEN \"4k3/8/8/8/8/8/4P3/4K3 w - - 0 1\"]")
        );
    }

    #[test]
    fn illegal_move_reports_context() {
        let err = load_text("1. e4 e5 2. Ke3").unwrap_err();
        assert!(
            matches!(err, ImportError::IllegalMove { ref san, .. } if san == "Ke3"),
            "{err}"
        );
    }
}
