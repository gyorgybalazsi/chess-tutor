//! Runs the engine checks for a position or a move and gathers everything an
//! explanation needs into one report, so a single Claude request suffices.

use serde::Serialize;
use shakmaty::{Chess, Move, Position};

use crate::{
    annotate::{LineFacts, color_name, line_facts},
    engine::{Engine, EngineError, EngineLine, SearchLimit},
    eval::{MATE_CP, MoveClass, Score, classify, cp_loss},
    tree::{fen_of, move_to_uci, numbered_line},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisConfig {
    pub limit: SearchLimit,
    /// Number of candidate moves to compare.
    pub multipv: u32,
    /// Maximum half-moves of each engine line to annotate.
    pub line_plies: usize,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        AnalysisConfig {
            limit: SearchLimit::Depth(18),
            multipv: 3,
            line_plies: 8,
        }
    }
}

/// One engine candidate with its line walked and annotated.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Candidate {
    pub rank: u32,
    pub san: String,
    pub uci: String,
    /// From White's point of view.
    pub eval: Score,
    pub eval_text: String,
    pub depth: u32,
    /// The engine line in SAN with move numbers.
    pub line: String,
    pub line_san: Vec<String>,
    pub facts: LineFacts,
}

/// Engine view of one position.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PositionAnalysis {
    pub fen: String,
    pub side_to_move: &'static str,
    /// Best first. Empty when the game is over.
    pub candidates: Vec<Candidate>,
    /// From White's point of view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval: Option<Score>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_over: Option<String>,
}

impl PositionAnalysis {
    pub fn best(&self) -> Option<&Candidate> {
        self.candidates.first()
    }
}

/// How the played move compares with the engine's choice.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PlayedMove {
    pub san: String,
    pub uci: String,
    pub mover: &'static str,
    /// Evaluation after the move, White's point of view.
    pub eval: Score,
    pub eval_text: String,
    pub classification: MoveClass,
    /// Centipawns lost compared with the engine's best move (mover's view).
    pub cp_loss: i32,
    pub is_engine_choice: bool,
    /// The opponent's best reply according to the engine, usually the reason
    /// a weaker move is weaker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_reply: Option<String>,
    /// The engine's line after the played move, starting with the move itself.
    pub line: String,
    pub facts: LineFacts,
}

/// Everything Claude gets for explaining one move.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct MoveReport {
    /// Moves played to reach the position before the move.
    pub moves_so_far: String,
    pub position_before: PositionAnalysis,
    pub played: PlayedMove,
    /// Whether the position is tactically sharp (mates, big gaps between
    /// candidates, large swings). Used to pick a stronger model.
    pub sharp: bool,
}

/// Analyses a position: MultiPV candidates, each line annotated.
/// `history` holds hashes of earlier positions in the game (for repetition).
pub fn analyse_position(
    engine: &mut Engine,
    pos: &Chess,
    history: &[u64],
    cfg: &AnalysisConfig,
) -> Result<PositionAnalysis, EngineError> {
    let turn = pos.turn();
    let mut out = PositionAnalysis {
        fen: fen_of(pos),
        side_to_move: color_name(turn),
        candidates: Vec::new(),
        eval: None,
        game_over: None,
    };
    if pos.is_checkmate() {
        out.game_over = Some(format!("{} is checkmated", color_name(turn)));
        // A finished game: the winner's side gets the full mate value.
        out.eval = Some(Score::Cp(-MATE_CP).for_color(turn));
        return Ok(out);
    }
    if pos.is_stalemate() {
        out.game_over = Some("stalemate: draw".into());
        out.eval = Some(Score::Cp(0));
        return Ok(out);
    }
    if pos.is_insufficient_material() {
        out.game_over = Some("insufficient material: draw".into());
        out.eval = Some(Score::Cp(0));
        return Ok(out);
    }
    let lines = engine.analyse(pos, cfg.multipv, cfg.limit)?;
    out.candidates = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| candidate(pos, l, i as u32 + 1, history, cfg))
        .collect();
    out.eval = out.candidates.first().map(|c| c.eval);
    Ok(out)
}

fn candidate(
    pos: &Chess,
    l: &EngineLine,
    rank: u32,
    history: &[u64],
    cfg: &AnalysisConfig,
) -> Option<Candidate> {
    let pv: Vec<String> = l.pv.iter().take(cfg.line_plies).cloned().collect();
    let (sans, facts) = line_facts(pos, &pv, history);
    let first = facts.moves.first()?;
    let eval = l.score.for_color(pos.turn());
    Some(Candidate {
        rank,
        san: first.san.clone(),
        uci: first.uci.clone(),
        eval,
        eval_text: eval.display(),
        depth: l.depth,
        line: numbered_line(pos, &sans),
        line_san: sans,
        facts,
    })
}

/// Analyses the move `m` played from `before`, comparing it with the
/// engine's candidates in `before_analysis` (from [`analyse_position`]).
pub fn analyse_move(
    engine: &mut Engine,
    before: &Chess,
    before_analysis: &PositionAnalysis,
    m: Move,
    moves_so_far: &str,
    history: &[u64],
    cfg: &AnalysisConfig,
) -> Result<MoveReport, EngineError> {
    let mover = before.turn();
    let uci = move_to_uci(m);

    // Reuse the MultiPV line when the move is among the candidates, else
    // search the move alone at the same depth so the scores are comparable.
    let line = match before_analysis.candidates.iter().find(|c| c.uci == uci) {
        Some(c) => EngineLine {
            multipv: c.rank,
            depth: c.depth,
            score: c.eval.for_color(mover),
            pv: c.facts.moves.iter().map(|f| f.uci.clone()).collect(),
        },
        None => {
            let mut after = before.clone();
            after.play_unchecked(m);
            if after.is_checkmate() {
                EngineLine {
                    multipv: 1,
                    depth: 0,
                    score: Score::Mate(1),
                    pv: vec![uci.clone()],
                }
            } else if after.is_stalemate() || after.is_insufficient_material() {
                EngineLine {
                    multipv: 1,
                    depth: 0,
                    score: Score::Cp(0),
                    pv: vec![uci.clone()],
                }
            } else {
                engine.analyse_move(before, m, cfg.limit)?
            }
        }
    };

    let pv: Vec<String> = line.pv.iter().take(cfg.line_plies.max(2)).cloned().collect();
    let (sans, facts) = line_facts(before, &pv, history);
    let played_score_mover = line.score;
    let best = before_analysis.best();
    let best_score_mover = best
        .map(|b| b.eval.for_color(mover))
        .unwrap_or(played_score_mover);
    let is_engine_choice = best.is_some_and(|b| b.uci == uci);
    let eval = played_score_mover.for_color(mover);

    let played = PlayedMove {
        san: sans.first().cloned().unwrap_or_else(|| uci.clone()),
        uci,
        mover: color_name(mover),
        eval,
        eval_text: eval.display(),
        classification: classify(best_score_mover, played_score_mover, is_engine_choice),
        cp_loss: cp_loss(best_score_mover, played_score_mover),
        is_engine_choice,
        best_reply: sans.get(1).cloned(),
        line: numbered_line(before, &sans),
        facts,
    };
    let sharp = is_sharp(before_analysis, &played);
    Ok(MoveReport {
        moves_so_far: moves_so_far.to_string(),
        position_before: before_analysis.clone(),
        played,
        sharp,
    })
}

/// A position counts as sharp when a mate is on the board, the best move is
/// much better than the second best (only one move works), or the played
/// move throws away a lot.
pub fn is_sharp(pos: &PositionAnalysis, played: &PlayedMove) -> bool {
    if played.eval.is_mate() || pos.candidates.iter().any(|c| c.eval.is_mate()) {
        return true;
    }
    if let [first, second, ..] = pos.candidates.as_slice() {
        let gap = (first.eval.to_cp() - second.eval.to_cp()).abs();
        if gap >= 150 {
            return true;
        }
    }
    played.cp_loss >= 200
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::find_stockfish, import::parse_fen, tree::uci_to_move};

    fn engine() -> Option<Engine> {
        let path = find_stockfish()?;
        Engine::spawn(&path).ok()
    }

    const CFG: AnalysisConfig = AnalysisConfig {
        limit: SearchLimit::Depth(12),
        multipv: 3,
        line_plies: 6,
    };

    #[test]
    fn finds_mate_in_one_and_classifies() {
        let Some(mut engine) = engine() else {
            eprintln!("Stockfish not found; skipping");
            return;
        };
        // Scholar's mate pattern: Qxf7# is available.
        let pos = parse_fen("r1bqkb1r/pppp1ppp/2n2n2/4p2Q/2B1P3/8/PPPP1PPP/RNB1K1NR w KQkq - 4 4").unwrap();
        let a = analyse_position(&mut engine, &pos, &[], &CFG).unwrap();
        assert_eq!(a.best().unwrap().san, "Qxf7#");
        assert_eq!(a.eval, Some(Score::Mate(1)));

        let m = uci_to_move(&pos, "h5f7").unwrap();
        let r = analyse_move(&mut engine, &pos, &a, m, "", &[], &CFG).unwrap();
        assert_eq!(r.played.classification, MoveClass::Best);
        assert!(r.played.facts.moves[0].checkmate);

        // Missing the mate and hanging the queen instead.
        let m = uci_to_move(&pos, "h5h4").unwrap();
        let r = analyse_move(&mut engine, &pos, &a, m, "", &[], &CFG).unwrap();
        assert_ne!(r.played.classification, MoveClass::Best);
        assert!(r.sharp);
        assert!(r.played.best_reply.is_some());
    }

    #[test]
    fn game_over_positions_need_no_engine() {
        let Some(mut engine) = engine() else { return };
        let mated = parse_fen("rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3").unwrap();
        let a = analyse_position(&mut engine, &mated, &[], &CFG).unwrap();
        assert!(a.candidates.is_empty());
        assert_eq!(a.game_over.as_deref(), Some("White is checkmated"));
    }
}
