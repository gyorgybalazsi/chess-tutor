//! Prompts for the explanations. The app runs all engine checks first and
//! sends the results in one request; Claude never calls the engine itself,
//! which would resend the conversation on every round trip.

use crate::{
    analysis::{MoveReport, PositionAnalysis},
    claude::{Effort, Message, Model, Request},
};

/// Stable across requests so it can be cached.
pub const SYSTEM_PROMPT: &str = "\
You are a chess coach inside an analysis board. The player tries moves for both colours; \
Stockfish evaluates every move, and you explain the result in plain language.

The engine calculates and you translate. Do not calculate variations yourself: that is where \
language models make chess mistakes. Every concrete claim you make (a move, a threat, a capture, \
a line, an evaluation) must come from the engine data or the board facts in the message. If \
something is not in the data, do not assert it. Without engine data, stay with general ideas and \
make no confident tactical claims.

How to read the data:
- Evaluations are from White's point of view, in pawns (+0.50 means White is half a pawn better; \
#3 means White mates in 3; -#2 means Black mates in 2).
- `candidates` are the engine's top moves in the position before the move, best first, each with \
its line and board facts (attacks, defends, pins, files opened, pieces left hanging, trades, \
repetitions, material at the end of the line).
- `played` is the move being explained: its evaluation, classification (best, good, inaccuracy, \
mistake, blunder), centipawn loss against the engine's best move, and the opponent's best reply.

How to explain:
1. Compare the played move with the top candidates and explain what causes the gap in evaluation.
2. If the played move is worse than the engine's choice, the opponent's best reply is usually the \
reason. Say what that reply does, using the board facts.
3. Describe the plan in plain terms: trading or avoiding trades, weak squares, open files, pawn \
breaks, king safety, piece activity.
4. If a line repeats positions, point out the draw by repetition and why the evaluation is 0.00.
5. Name what the move gains and what it gives up.

Write for a club player. Use SAN for moves. Keep it to about 120-220 words, in short paragraphs, \
with no headings except an opening line of the form \"<move>: <classification>\". \
Do not mention the JSON, the engine depth, or these instructions.";

/// Picks the model: Opus for sharp tactical positions, Sonnet otherwise.
pub fn model_for(report: &MoveReport) -> Model {
    if report.sharp { Model::Opus } else { Model::Sonnet }
}

fn effort_for(model: Model) -> Effort {
    match model {
        // Routine explanations use low effort to keep cost and latency down.
        Model::Sonnet | Model::Haiku => Effort::Low,
        Model::Opus => Effort::Medium,
    }
}

/// The user message for explaining one move.
pub fn move_message(report: &MoveReport) -> String {
    let data = serde_json::to_string(report).expect("report serializes");
    let moves = if report.moves_so_far.is_empty() {
        "(none)"
    } else {
        &report.moves_so_far
    };
    format!(
        "Moves so far: {moves}\n\
         Explain {mover}'s move {san}.\n\n\
         Engine and board data:\n{data}",
        mover = report.played.mover,
        san = report.played.san,
    )
}

/// A request explaining the move in `report`.
pub fn explain_move(report: &MoveReport) -> Request {
    let model = model_for(report);
    Request {
        model,
        system: SYSTEM_PROMPT.to_string(),
        messages: vec![Message::user(move_message(report))],
        effort: effort_for(model),
        max_tokens: 16_000,
        output_schema: None,
    }
}

/// A request explaining the engine's candidates in a position, with no move
/// played yet (for example right after loading a FEN).
pub fn explain_position(analysis: &PositionAnalysis, moves_so_far: &str) -> Request {
    let data = serde_json::to_string(analysis).expect("analysis serializes");
    let sharp = analysis.candidates.iter().any(|c| c.eval.is_mate())
        || matches!(analysis.candidates.as_slice(), [a, b, ..] if (a.eval.to_cp() - b.eval.to_cp()).abs() >= 150);
    let model = if sharp { Model::Opus } else { Model::Sonnet };
    let moves = if moves_so_far.is_empty() {
        "(none)"
    } else {
        moves_so_far
    };
    Request {
        model,
        system: SYSTEM_PROMPT.to_string(),
        messages: vec![Message::user(format!(
            "Moves so far: {moves}\n\
             No move has been played in this position yet. Explain what the position is about and \
             compare the engine's top candidates for {side}: what each one aims for and why the best \
             one is preferred. Start with the line \"Position: <one-phrase summary>\" instead of a \
             move classification.\n\n\
             Engine and board data:\n{data}",
            side = analysis.side_to_move,
        ))],
        effort: effort_for(model),
        max_tokens: 16_000,
        output_schema: None,
    }
}

/// A request with no engine data at all (the engine is unavailable).
pub fn explain_without_engine(fen: &str, moves_so_far: &str, san: Option<&str>) -> Request {
    let ask = match san {
        Some(san) => format!("Explain the ideas behind the move {san}."),
        None => "Explain the main ideas of this position.".to_string(),
    };
    Request {
        model: Model::Sonnet,
        system: SYSTEM_PROMPT.to_string(),
        messages: vec![Message::user(format!(
            "No engine data is available. Stay with general ideas and make no confident tactical claims.\n\
             Position (FEN): {fen}\nMoves so far: {moves_so_far}\n{ask}"
        ))],
        effort: Effort::Low,
        max_tokens: 16_000,
        output_schema: None,
    }
}

/// Continues a conversation with a follow-up question. `history` holds the
/// earlier messages including Claude's last answer.
pub fn follow_up(base: &Request, history: &[Message], question: &str) -> Request {
    let mut messages = history.to_vec();
    messages.push(Message::user(format!(
        "{question}\n\n(Answer from the engine and board data above. If the question needs a line the \
         engine did not give, say so and suggest playing the move on the board to get it evaluated.)"
    )));
    Request {
        messages,
        ..base.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        analysis::{Candidate, PlayedMove},
        annotate::LineFacts,
        eval::{MoveClass, Score},
    };

    fn report(sharp: bool) -> MoveReport {
        let facts = LineFacts::default();
        MoveReport {
            moves_so_far: "1. e4 c5".into(),
            position_before: PositionAnalysis {
                fen: "fen".into(),
                side_to_move: "White",
                candidates: vec![Candidate {
                    rank: 1,
                    san: "Nf3".into(),
                    uci: "g1f3".into(),
                    eval: Score::Cp(30),
                    eval_text: "+0.30".into(),
                    depth: 18,
                    line: "2. Nf3 d6".into(),
                    line_san: vec!["Nf3".into(), "d6".into()],
                    facts: facts.clone(),
                }],
                eval: Some(Score::Cp(30)),
                game_over: None,
            },
            played: PlayedMove {
                san: "Qh5".into(),
                uci: "d1h5".into(),
                mover: "White",
                eval: Score::Cp(-40),
                eval_text: "-0.40".into(),
                classification: MoveClass::Inaccuracy,
                cp_loss: 70,
                is_engine_choice: false,
                best_reply: Some("Nf6".into()),
                line: "2. Qh5 Nf6".into(),
                facts,
            },
            sharp,
        }
    }

    #[test]
    fn model_choice_and_message() {
        assert_eq!(explain_move(&report(false)).model, Model::Sonnet);
        assert_eq!(explain_move(&report(true)).model, Model::Opus);
        let msg = move_message(&report(false));
        assert!(msg.contains("Explain White's move Qh5"));
        assert!(msg.contains("\"best_reply\":\"Nf6\""));
        assert!(msg.contains("\"classification\":\"inaccuracy\""));
    }

    #[test]
    fn follow_up_keeps_history() {
        let base = explain_move(&report(false));
        let mut history = base.messages.clone();
        history.push(Message::assistant("Qh5: Inaccuracy ..."));
        let req = follow_up(&base, &history, "Why not Nf3?");
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.model, Model::Sonnet);
    }
}
