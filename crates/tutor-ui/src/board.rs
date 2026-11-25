//! The interactive chessboard: click a piece, then click its target square.

use chess_core::{
    eval::{Score, win_percent},
    shakmaty::{Chess, Color, File, Move, Position, Rank, Role, Square},
};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};

const LIGHT: Color32 = Color32::from_rgb(240, 217, 181);
const DARK: Color32 = Color32::from_rgb(181, 136, 99);
const LAST_MOVE: Color32 = Color32::from_rgba_premultiplied(155, 199, 0, 105);
const SELECTED: Color32 = Color32::from_rgba_premultiplied(20, 85, 30, 110);
const ARROW: Color32 = Color32::from_rgba_premultiplied(0, 90, 200, 170);

#[derive(Default)]
pub struct BoardView {
    pub flipped: bool,
    pub selected: Option<Square>,
    /// Moves waiting for the user to pick a promotion piece.
    pub promotion_choices: Vec<Move>,
}

/// King's destination for castling moves (shakmaty encodes castling as
/// king-takes-rook), else the plain target square.
pub fn destination(m: &Move) -> Square {
    match *m {
        Move::Castle { king, rook } => {
            let file = if rook.file() > king.file() {
                File::G
            } else {
                File::C
            };
            Square::from_coords(file, king.rank())
        }
        _ => m.to(),
    }
}

fn square_rect(board: Rect, sq: Square, flipped: bool) -> Rect {
    let size = board.width() / 8.0;
    let (col, row) = if flipped {
        (7 - sq.file().to_u32(), sq.rank().to_u32())
    } else {
        (sq.file().to_u32(), 7 - sq.rank().to_u32())
    };
    Rect::from_min_size(
        board.min + vec2(col as f32 * size, row as f32 * size),
        Vec2::splat(size),
    )
}

fn square_at(board: Rect, p: Pos2, flipped: bool) -> Option<Square> {
    if !board.contains(p) {
        return None;
    }
    let size = board.width() / 8.0;
    let col = (((p.x - board.min.x) / size) as u32).min(7);
    let row = (((p.y - board.min.y) / size) as u32).min(7);
    let (file, rank) = if flipped { (7 - col, row) } else { (col, 7 - row) };
    Some(Square::from_coords(File::new(file), Rank::new(rank)))
}

fn glyph(role: Role, outline: bool) -> char {
    match (role, outline) {
        (Role::King, false) => '♚',
        (Role::Queen, false) => '♛',
        (Role::Rook, false) => '♜',
        (Role::Bishop, false) => '♝',
        (Role::Knight, false) => '♞',
        (Role::Pawn, false) => '♟',
        (Role::King, true) => '♔',
        (Role::Queen, true) => '♕',
        (Role::Rook, true) => '♖',
        (Role::Bishop, true) => '♗',
        (Role::Knight, true) => '♘',
        (Role::Pawn, true) => '♙',
    }
}

fn draw_piece(painter: &egui::Painter, rect: Rect, color: Color, role: Role, glyphs: bool) {
    let size = rect.width();
    if glyphs {
        let font = FontId::proportional(size * 0.82);
        let center = rect.center() + vec2(0.0, size * 0.02);
        match color {
            Color::White => {
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    glyph(role, false),
                    font.clone(),
                    Color32::WHITE,
                );
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    glyph(role, true),
                    font,
                    Color32::BLACK,
                );
            }
            Color::Black => {
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    glyph(role, false),
                    font,
                    Color32::from_gray(20),
                );
            }
        }
    } else {
        // No font with chess symbols: draw a disc with the piece letter.
        let (fill, text) = match color {
            Color::White => (Color32::from_gray(250), Color32::BLACK),
            Color::Black => (Color32::from_gray(30), Color32::WHITE),
        };
        painter.circle_filled(rect.center(), size * 0.38, fill);
        painter.circle_stroke(
            rect.center(),
            size * 0.38,
            Stroke::new(1.5_f32, Color32::from_gray(60)),
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            role.upper_char(),
            FontId::proportional(size * 0.45),
            text,
        );
    }
}

pub struct BoardInput<'a> {
    pub position: &'a Chess,
    pub last_move: Option<Move>,
    /// Engine's best move in the position, drawn as an arrow.
    pub best: Option<Move>,
    /// White's point of view.
    pub eval: Option<Score>,
    pub glyphs: bool,
}

/// Draws the eval bar and the board. Returns a move when the user completed one.
pub fn board_ui(ui: &mut egui::Ui, view: &mut BoardView, input: BoardInput<'_>) -> Option<Move> {
    let bar_w = 18.0;
    let avail = ui.available_size();
    let side = (avail.x - bar_w - 8.0).min(avail.y - 40.0).max(160.0);
    let (outer, response) = ui.allocate_exact_size(vec2(side + bar_w + 8.0, side), Sense::click());
    let bar = Rect::from_min_size(outer.min, vec2(bar_w, side));
    let board = Rect::from_min_size(outer.min + vec2(bar_w + 8.0, 0.0), Vec2::splat(side));
    let painter = ui.painter_at(outer);

    draw_eval_bar(&painter, bar, input.eval, view.flipped);

    for i in 0..64u32 {
        let sq = Square::new(i);
        let r = square_rect(board, sq, view.flipped);
        painter.rect_filled(r, 0.0, if sq.is_light() { LIGHT } else { DARK });
    }
    if let Some(m) = input.last_move {
        if let Some(from) = m.from() {
            painter.rect_filled(square_rect(board, from, view.flipped), 0.0, LAST_MOVE);
        }
        painter.rect_filled(square_rect(board, destination(&m), view.flipped), 0.0, LAST_MOVE);
    }
    if let Some(sel) = view.selected {
        painter.rect_filled(square_rect(board, sel, view.flipped), 0.0, SELECTED);
    }

    // Coordinates.
    let sq_size = side / 8.0;
    let coord_font = FontId::proportional((sq_size * 0.18).max(9.0));
    for i in 0..8u32 {
        let file_sq = Square::from_coords(
            File::new(i),
            if view.flipped { Rank::Eighth } else { Rank::First },
        );
        let r = square_rect(board, file_sq, view.flipped);
        let c = if file_sq.is_light() { DARK } else { LIGHT };
        painter.text(
            r.right_bottom() - vec2(3.0, 2.0),
            Align2::RIGHT_BOTTOM,
            File::new(i).char(),
            coord_font.clone(),
            c,
        );
        let rank_sq = Square::from_coords(if view.flipped { File::H } else { File::A }, Rank::new(i));
        let r = square_rect(board, rank_sq, view.flipped);
        let c = if rank_sq.is_light() { DARK } else { LIGHT };
        painter.text(
            r.left_top() + vec2(3.0, 2.0),
            Align2::LEFT_TOP,
            Rank::new(i).char(),
            coord_font.clone(),
            c,
        );
    }

    let pos = input.position;
    for sq in pos.board().occupied() {
        let piece = pos.board().piece_at(sq).expect("occupied square");
        draw_piece(
            &painter,
            square_rect(board, sq, view.flipped),
            piece.color,
            piece.role,
            input.glyphs,
        );
    }

    // Legal targets of the selected piece.
    let legal = pos.legal_moves();
    if let Some(sel) = view.selected {
        for m in legal.iter().filter(|m| m.from() == Some(sel)) {
            let r = square_rect(board, destination(m), view.flipped);
            if pos.board().piece_at(m.to()).is_some() && !m.is_castle() {
                painter.circle_stroke(r.center(), sq_size * 0.42, Stroke::new(sq_size * 0.07, SELECTED));
            } else {
                painter.circle_filled(r.center(), sq_size * 0.15, SELECTED);
            }
        }
    }

    if let Some(m) = input.best
        && let Some(from) = m.from()
    {
        let a = square_rect(board, from, view.flipped).center();
        let b = square_rect(board, destination(&m), view.flipped).center();
        painter.arrow(a, b - a, Stroke::new(sq_size * 0.12, ARROW));
    }

    painter.rect_stroke(
        board,
        0.0,
        Stroke::new(1.0_f32, Color32::from_gray(80)),
        StrokeKind::Outside,
    );

    let mut played = None;
    if response.clicked()
        && view.promotion_choices.is_empty()
        && let Some(target) = response
            .interact_pointer_pos()
            .and_then(|p| square_at(board, p, view.flipped))
    {
        if let Some(sel) = view.selected {
            let matching: Vec<Move> = legal
                .iter()
                .filter(|m| m.from() == Some(sel) && (destination(m) == target || m.to() == target))
                .copied()
                .collect();
            match matching.len() {
                0 => {
                    let own = pos.board().color_at(target) == Some(pos.turn());
                    view.selected = own.then_some(target);
                }
                1 => {
                    played = Some(matching[0]);
                    view.selected = None;
                }
                _ => {
                    view.promotion_choices = matching;
                    view.selected = None;
                }
            }
        } else if pos.board().color_at(target) == Some(pos.turn()) {
            view.selected = Some(target);
        }
    }
    if response.secondary_clicked() {
        view.selected = None;
    }

    if !view.promotion_choices.is_empty() {
        ui.horizontal(|ui| {
            ui.label("Promote to:");
            for m in view.promotion_choices.clone() {
                let name = match m.promotion() {
                    Some(Role::Queen) => "Queen",
                    Some(Role::Rook) => "Rook",
                    Some(Role::Bishop) => "Bishop",
                    Some(Role::Knight) => "Knight",
                    _ => "?",
                };
                if ui.button(name).clicked() {
                    played = Some(m);
                    view.promotion_choices.clear();
                }
            }
            if ui.button("Cancel").clicked() {
                view.promotion_choices.clear();
            }
        });
    }
    played
}

fn draw_eval_bar(painter: &egui::Painter, bar: Rect, eval: Option<Score>, flipped: bool) {
    painter.rect_filled(bar, 2.0, Color32::from_gray(40));
    let Some(eval) = eval else { return };
    let white_share = (win_percent(eval.to_cp()) / 100.0) as f32;
    let h = bar.height() * white_share;
    let white_rect = if flipped {
        Rect::from_min_size(bar.min, vec2(bar.width(), h))
    } else {
        Rect::from_min_size(pos2(bar.min.x, bar.max.y - h), vec2(bar.width(), h))
    };
    painter.rect_filled(white_rect, 2.0, Color32::from_gray(235));
    let text = eval.display();
    let white_better = eval.to_cp() >= 0;
    let at_bottom = white_better != flipped;
    let (anchor, p) = if at_bottom {
        (Align2::CENTER_BOTTOM, pos2(bar.center().x, bar.max.y - 2.0))
    } else {
        (Align2::CENTER_TOP, pos2(bar.center().x, bar.min.y + 2.0))
    };
    let color = if white_better {
        Color32::BLACK
    } else {
        Color32::WHITE
    };
    let short = text.trim_start_matches('+');
    painter.text(p, anchor, short, FontId::proportional(9.0), color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_mapping_round_trips() {
        let board = Rect::from_min_size(pos2(10.0, 20.0), Vec2::splat(400.0));
        for flipped in [false, true] {
            for i in 0..64 {
                let sq = Square::new(i);
                let r = square_rect(board, sq, flipped);
                assert_eq!(square_at(board, r.center(), flipped), Some(sq));
            }
        }
        // a1 is bottom-left from White's side.
        let a1 = square_rect(board, Square::A1, false);
        assert!(a1.min.x < 20.0 && a1.max.y > 410.0);
    }

    #[test]
    fn castling_destination() {
        let pos =
            chess_core::import::parse_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1").unwrap();
        let dests: Vec<Square> = pos
            .legal_moves()
            .iter()
            .filter(|m| m.is_castle())
            .map(destination)
            .collect();
        assert!(dests.contains(&Square::G1) && dests.contains(&Square::C1));
    }
}
