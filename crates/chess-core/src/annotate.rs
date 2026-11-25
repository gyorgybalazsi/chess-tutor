//! Concrete, checkable facts about each move of a line, computed with
//! shakmaty: what it attacks, defends, pins or opens, and whether positions
//! repeat. These are handed to Claude so its explanation describes what is
//! really on the board instead of what it imagines.

use serde::Serialize;
use shakmaty::{Bitboard, Chess, Color, File, Move, Piece, Position, Role, Square, attacks, san::SanPlus};

use crate::tree::{move_to_uci, position_hash, uci_to_move};

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct MoveFacts {
    pub san: String,
    pub uci: String,
    pub mover: &'static str,
    pub piece: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captures: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub checkmate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub castles: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotes_to: Option<&'static str>,
    /// Enemy pieces the moved piece newly attacks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attacks: Vec<String>,
    /// Enemy pieces newly attacked by other pieces because this one moved away.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub discovered_attacks: Vec<String>,
    /// Own pieces the moved piece defends from its new square.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub defends: Vec<String>,
    /// Enemy pieces pinned to their king or queen after the move that were not before.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<String>,
    /// The mover's own pieces that this move leaves pinned.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub walks_into_pin: Vec<String>,
    /// Files that became open or half-open.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub opens: Vec<String>,
    /// The mover's pieces left attacked and undefended after the move.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub leaves_hanging: Vec<String>,
    /// Enemy pieces attacked and undefended after the move.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enemy_hanging: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct LineFacts {
    pub moves: Vec<MoveFacts>,
    /// Exchanges in the line, e.g. "knights traded on d4".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trades: Vec<String>,
    /// Set when a position in the line already occurred in the game or
    /// earlier in the line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repetition: Option<String>,
    /// Material count at the end of the line, e.g. "White is up a pawn".
    pub material_at_end: String,
}

pub fn color_name(c: Color) -> &'static str {
    if c.is_white() { "White" } else { "Black" }
}

pub fn role_name(r: Role) -> &'static str {
    match r {
        Role::Pawn => "pawn",
        Role::Knight => "knight",
        Role::Bishop => "bishop",
        Role::Rook => "rook",
        Role::Queen => "queen",
        Role::King => "king",
    }
}

fn role_value(r: Role) -> i32 {
    match r {
        Role::Pawn => 1,
        Role::Knight | Role::Bishop => 3,
        Role::Rook => 5,
        Role::Queen => 9,
        Role::King => 0,
    }
}

fn describe(piece: Piece, sq: Square) -> String {
    format!(
        "{} {} on {}",
        color_name(piece.color).to_lowercase(),
        role_name(piece.role),
        sq
    )
}

fn describe_set(pos: &Chess, squares: Bitboard) -> Vec<String> {
    squares
        .into_iter()
        .filter_map(|sq| pos.board().piece_at(sq).map(|p| describe(p, sq)))
        .collect()
}

/// Pieces of `color` that are attacked by the opponent and not defended.
fn hanging(pos: &Chess, color: Color) -> Bitboard {
    let board = pos.board();
    let occupied = board.occupied();
    let mut out = Bitboard::EMPTY;
    for sq in board.by_color(color) & !board.kings() {
        let attacked = board.attacks_to(sq, !color, occupied).any();
        let defended = board.attacks_to(sq, color, occupied).any();
        if attacked && !defended {
            out.add(sq);
        }
    }
    out
}

/// Enemy pieces pinned by `attacker`'s sliders to their king or queen.
fn pinned_by(pos: &Chess, attacker: Color) -> Bitboard {
    let board = pos.board();
    let occupied = board.occupied();
    let defender = !attacker;
    let targets = board.by_color(defender) & (board.kings() | board.queens());
    let mut pinned = Bitboard::EMPTY;
    for slider in board.by_color(attacker) & board.sliders() {
        let role = board.role_at(slider).expect("slider square is occupied");
        for target in targets {
            let ray = attacks::ray(slider, target);
            if ray.is_empty() {
                continue;
            }
            let straight = slider.file() == target.file() || slider.rank() == target.rank();
            let can_slide = match role {
                Role::Rook => straight,
                Role::Bishop => !straight,
                Role::Queen => true,
                _ => false,
            };
            // A queen cannot be "pinned" to a queen by a queen.
            if !can_slide || (role == Role::Queen && board.role_at(target) == Some(Role::Queen)) {
                continue;
            }
            let blockers = attacks::between(slider, target) & occupied;
            if let Some(sq) = blockers.single_square()
                && board.color_at(sq) == Some(defender)
                && board.role_at(sq) != Some(Role::King)
            {
                pinned.add(sq);
            }
        }
    }
    pinned
}

fn file_state(pos: &Chess, file: File) -> (bool, bool) {
    let pawns = pos.board().pawns() & Bitboard::from_file(file);
    (
        (pawns & pos.board().white()).any(),
        (pawns & pos.board().black()).any(),
    )
}

/// Describes one move played from `before`.
pub fn move_facts(before: &Chess, m: Move) -> MoveFacts {
    let mover = before.turn();
    let mut after = before.clone();
    let san = SanPlus::from_move_and_play_unchecked(&mut after, m).to_string();
    let board_before = before.board();
    let board_after = after.board();
    let enemy_after = board_after.by_color(!mover) & !board_after.kings();
    let own_after = board_after.by_color(mover) & !board_after.kings();

    let to = m.to();
    let moved_piece_sq = match m {
        Move::Castle { king, rook } => {
            // The rook is the piece that usually does something after castling.
            let side = if rook.file() > king.file() {
                File::F
            } else {
                File::D
            };
            Square::from_coords(side, king.rank())
        }
        _ => to,
    };
    let attacks_now = board_after.attacks_from(moved_piece_sq);
    let attacks_before = m
        .from()
        .map(|f| board_before.attacks_from(f))
        .unwrap_or(Bitboard::EMPTY);
    let new_attacks = attacks_now & enemy_after & !attacks_before;

    // Discovered attacks: other own pieces that attack more enemies now.
    let mut discovered = Bitboard::EMPTY;
    for sq in board_after.by_color(mover) & board_after.sliders() {
        if sq == moved_piece_sq {
            continue;
        }
        let now = board_after.attacks_from(sq)
            & (enemy_after | (board_after.kings() & board_after.by_color(!mover)));
        let was = board_before.attacks_from(sq);
        discovered |= now & !was;
    }

    let pins = pinned_by(&after, mover) & !pinned_by(before, mover);

    let mut opens = Vec::new();
    let touched_files: Vec<File> = [m.from().map(|s| s.file()), Some(to.file())]
        .into_iter()
        .flatten()
        .collect();
    for file in touched_files {
        let (w0, b0) = file_state(before, file);
        let (w1, b1) = file_state(&after, file);
        let name = file.char();
        if (w0 || b0) && !w1 && !b1 {
            opens.push(format!("opens the {name}-file completely"));
        } else if w0 && !w1 && b1 {
            opens.push(format!("half-opens the {name}-file for White"));
        } else if b0 && !b1 && w1 {
            opens.push(format!("half-opens the {name}-file for Black"));
        }
    }
    opens.dedup();

    MoveFacts {
        san,
        uci: move_to_uci(m),
        mover: color_name(mover),
        piece: role_name(m.role()),
        captures: m.capture().map(|r| {
            let sq = if m.is_en_passant() {
                Square::from_coords(to.file(), m.from().expect("en passant has a from square").rank())
            } else {
                to
            };
            format!("{} on {}", role_name(r), sq)
        }),
        check: after.is_check(),
        checkmate: after.is_checkmate(),
        castles: m
            .castling_side()
            .map(|s| if s.is_king_side() { "kingside" } else { "queenside" }),
        promotes_to: m.promotion().map(role_name),
        attacks: describe_set(&after, new_attacks),
        discovered_attacks: describe_set(&after, discovered),
        defends: describe_set(&after, attacks_now & own_after),
        pins: describe_set(&after, pins),
        walks_into_pin: describe_set(&after, pinned_by(&after, !mover) & !pinned_by(before, !mover)),
        opens,
        leaves_hanging: describe_set(&after, hanging(&after, mover)),
        enemy_hanging: describe_set(&after, hanging(&after, !mover)),
    }
}

pub fn material_balance(pos: &Chess) -> i32 {
    let board = pos.board();
    let mut total = 0;
    for sq in board.occupied() {
        let p = board.piece_at(sq).expect("occupied");
        let v = role_value(p.role);
        total += if p.color.is_white() { v } else { -v };
    }
    total
}

pub fn describe_material(pos: &Chess) -> String {
    match material_balance(pos) {
        0 => "material is level".to_string(),
        n => {
            let side = if n > 0 { "White" } else { "Black" };
            let n = n.abs();
            let unit = if n == 1 { "pawn" } else { "pawns" };
            format!("{side} is up {n} {unit} worth of material")
        }
    }
}

/// Walks a line of UCI moves from `start`, stopping at the first illegal
/// move. `history` holds the hashes of positions that occurred in the game
/// before `start` (including `start` itself), for repetition detection.
pub fn line_facts(start: &Chess, uci_line: &[String], history: &[u64]) -> (Vec<String>, LineFacts) {
    let mut pos = start.clone();
    let mut sans = Vec::new();
    let mut moves = Vec::new();
    let mut trades = Vec::new();
    let mut seen: Vec<u64> = history.to_vec();
    if seen.last() != Some(&position_hash(start)) {
        seen.push(position_hash(start));
    }
    let mut repetition = None;
    let mut last_capture: Option<(Square, Role)> = None;

    for uci in uci_line {
        let Ok(m) = uci_to_move(&pos, uci) else { break };
        let facts = move_facts(&pos, m);
        sans.push(facts.san.clone());

        if let Some(captured) = m.capture() {
            if let Some((sq, first)) = last_capture
                && sq == m.to()
            {
                let what = if first == captured {
                    format!("{}s", role_name(first))
                } else {
                    format!("{} for {}", role_name(first), role_name(captured))
                };
                trades.push(format!("{what} traded on {sq}"));
            }
            last_capture = Some((m.to(), captured));
        } else {
            last_capture = None;
        }

        pos.play_unchecked(m);
        moves.push(facts);
        let h = position_hash(&pos);
        let count = seen.iter().filter(|&&x| x == h).count() + 1;
        seen.push(h);
        if count >= 2 && repetition.is_none() {
            let san = sans.last().cloned().unwrap_or_default();
            repetition = Some(if count >= 3 {
                format!(
                    "after {san} the same position occurs for the third time: draw by threefold repetition"
                )
            } else {
                format!(
                    "after {san} the position repeats one already seen; if the moves keep recurring it is a draw by repetition"
                )
            });
        }
    }

    let material_at_end = describe_material(&pos);
    (
        sans,
        LineFacts {
            moves,
            trades,
            repetition,
            material_at_end,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::parse_fen;

    fn facts(fen: &str, uci: &str) -> MoveFacts {
        let pos = parse_fen(fen).unwrap();
        move_facts(&pos, uci_to_move(&pos, uci).unwrap())
    }

    #[test]
    fn detects_pin() {
        // Bb5 pins the c6 knight to the black king.
        let f = facts(
            "r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - 2 3",
            "f1b5",
        );
        assert_eq!(f.san, "Bb5");
        assert!(f.attacks.contains(&"black knight on c6".to_string()), "{f:?}");
        assert!(f.pins.is_empty(), "d7 still blocks the diagonal");
        let f = facts(
            "r1bqkbnr/ppp2ppp/2np4/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - 0 4",
            "f1b5",
        );
        assert_eq!(f.pins, ["black knight on c6"], "{f:?}");
        // ...d6 in the Ruy Lopez walks into the same pin.
        let f = facts(
            "r1bqkbnr/pppp1ppp/2n5/1B2p3/4P3/5N2/PPPP1PPP/RNBQK2R b KQkq - 3 3",
            "d7d6",
        );
        assert_eq!(f.walks_into_pin, ["black knight on c6"], "{f:?}");
    }

    #[test]
    fn detects_capture_open_file_and_hanging() {
        // 3...exd4: captures a pawn, half-opens the e-file for Black.
        let f = facts(
            "rnbqkbnr/pppp1ppp/8/4p3/3PP3/8/PPP2PPP/RNBQKBNR b KQkq - 0 2",
            "e5d4",
        );
        assert_eq!(f.captures.as_deref(), Some("pawn on d4"));
        assert!(
            f.opens.contains(&"half-opens the e-file for Black".to_string()),
            "{f:?}"
        );
        // Qh5 in the opening attacks e5 and f7.
        let f = facts(
            "rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2",
            "d1h5",
        );
        assert!(f.attacks.contains(&"black pawn on e5".to_string()));
        assert!(f.enemy_hanging.contains(&"black pawn on e5".to_string()), "{f:?}");
    }

    #[test]
    fn detects_mate_and_castling() {
        let f = facts(
            "rnbqkbnr/ppppp2p/5p2/6p1/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 3",
            "d1h5",
        );
        assert!(f.check && f.checkmate);
        let f = facts(
            "r1bqk1nr/pppp1ppp/2n5/2b1p3/2B1P3/5N2/PPPP1PPP/RNBQK2R w KQkq - 4 4",
            "e1g1",
        );
        assert_eq!(f.castles, Some("kingside"));
        assert_eq!(f.san, "O-O");
    }

    #[test]
    fn line_trades_and_repetition() {
        let pos = parse_fen("4k3/8/8/3n4/8/2N5/8/4K3 w - - 0 1").unwrap();
        let line: Vec<String> = ["c3d5", "e8d7"].iter().map(|s| s.to_string()).collect();
        let (sans, lf) = line_facts(&pos, &line, &[]);
        assert_eq!(sans, ["Nxd5", "Kd7"]);
        assert!(lf.material_at_end.contains("White is up 3"));

        let pos = parse_fen("4k3/8/8/8/8/8/8/4K2R w - - 0 1").unwrap();
        let line: Vec<String> = ["h1h2", "e8d8", "h2h1", "d8e8", "h1h2", "e8d8", "h2h1", "d8e8"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (_, lf) = line_facts(&pos, &line, &[]);
        assert!(lf.repetition.unwrap().contains("repeats"));
        let (_, lf) = line_facts(&pos, &line, &[position_hash(&pos)]);
        assert!(lf.repetition.is_some());
    }

    #[test]
    fn recapture_is_a_trade() {
        // Quiet move between the captures: no trade.
        let pos = parse_fen("4k3/4p3/2n5/3n4/8/2N5/8/4K3 w - - 0 1").unwrap();
        let line: Vec<String> = ["c3d5", "e7e6", "d5c7"].iter().map(|s| s.to_string()).collect();
        let (_, lf) = line_facts(&pos, &line, &[]);
        assert!(lf.trades.is_empty());
        // Nxd5 cxd5: knights traded on d5.
        let pos = parse_fen("4k3/8/2p5/3n4/8/2N5/8/4K3 w - - 0 1").unwrap();
        let line: Vec<String> = ["c3d5", "c6d5"].iter().map(|s| s.to_string()).collect();
        let (_, lf) = line_facts(&pos, &line, &[]);
        assert_eq!(lf.trades, ["knights traded on d5"]);
    }
}
