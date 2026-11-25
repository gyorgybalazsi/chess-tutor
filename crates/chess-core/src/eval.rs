//! Scores, win probabilities and move classification.

use serde::Serialize;
use shakmaty::Color;

/// Centipawn value used to stand in for a forced mate when comparing scores.
pub const MATE_CP: i32 = 100_000;

/// An engine score. Unless stated otherwise it is from White's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Score {
    /// Centipawns.
    Cp(i32),
    /// Mate in this many moves; negative when the other side mates.
    Mate(i32),
}

impl Score {
    pub fn negate(self) -> Score {
        match self {
            Score::Cp(cp) => Score::Cp(-cp),
            Score::Mate(n) => Score::Mate(-n),
        }
    }

    /// Converts a score from White's point of view to `color`'s point of view
    /// (or back again: the conversion is its own inverse).
    pub fn for_color(self, color: Color) -> Score {
        match color {
            Color::White => self,
            Color::Black => self.negate(),
        }
    }

    /// A single number for comparisons: mates map to huge values, with a
    /// faster mate being larger.
    pub fn to_cp(self) -> i32 {
        match self {
            Score::Cp(cp) => cp,
            Score::Mate(n) if n > 0 => MATE_CP - n * 100,
            Score::Mate(n) if n < 0 => -MATE_CP - n * 100,
            // Mate(0): the side whose view this is has been mated.
            Score::Mate(_) => -MATE_CP,
        }
    }

    pub fn is_mate(self) -> bool {
        matches!(self, Score::Mate(_))
    }

    /// Human readable, in pawns: `+0.35`, `-1.20`, `#3`, `-#2`.
    pub fn display(self) -> String {
        match self {
            Score::Cp(cp) if cp >= MATE_CP => "1-0".to_string(),
            Score::Cp(cp) if cp <= -MATE_CP => "0-1".to_string(),
            Score::Cp(cp) => format!("{:+.2}", f64::from(cp) / 100.0),
            Score::Mate(n) if n >= 0 => format!("#{n}"),
            Score::Mate(n) => format!("-#{}", -n),
        }
    }
}

/// Winning chances in percent (0–100) for the side the centipawn value
/// belongs to. Same logistic curve Lichess uses for its accuracy metric.
pub fn win_percent(cp: i32) -> f64 {
    let cp = f64::from(cp.clamp(-1000, 1000));
    50.0 + 50.0 * (2.0 / (1.0 + (-0.003_682_08 * cp).exp()) - 1.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveClass {
    Best,
    Good,
    Inaccuracy,
    Mistake,
    Blunder,
}

impl MoveClass {
    pub fn label(self) -> &'static str {
        match self {
            MoveClass::Best => "Best",
            MoveClass::Good => "Good",
            MoveClass::Inaccuracy => "Inaccuracy",
            MoveClass::Mistake => "Mistake",
            MoveClass::Blunder => "Blunder",
        }
    }
}

/// Classifies a move by comparing it with the engine's best move in the same
/// position. Both scores are from the mover's point of view.
///
/// A move is `Best` when it is the engine's choice or within 10 centipawns of
/// it. Otherwise the drop in winning chances decides: under 10 points is
/// `Good`, under 20 an `Inaccuracy`, under 30 a `Mistake`, and 30 or more a
/// `Blunder`. Using winning chances rather than raw centipawns means that
/// dropping from +8 to +6 is not called a blunder, while +1 to -1 is.
pub fn classify(best: Score, played: Score, is_engine_choice: bool) -> MoveClass {
    let loss = (best.to_cp() - played.to_cp()).max(0);
    if is_engine_choice || loss <= 10 {
        return MoveClass::Best;
    }
    // Letting a forced mate slip, or walking into one, is always serious.
    if best.is_mate() && best.to_cp() > 0 && !(played.is_mate() && played.to_cp() > 0) {
        return if played.to_cp() >= 300 {
            MoveClass::Mistake
        } else {
            MoveClass::Blunder
        };
    }
    if played.is_mate() && played.to_cp() < 0 && !(best.is_mate() && best.to_cp() < 0) {
        return MoveClass::Blunder;
    }
    let drop = win_percent(best.to_cp()) - win_percent(played.to_cp());
    if drop < 10.0 {
        MoveClass::Good
    } else if drop < 20.0 {
        MoveClass::Inaccuracy
    } else if drop < 30.0 {
        MoveClass::Mistake
    } else {
        MoveClass::Blunder
    }
}

/// Centipawn loss of the played move compared with the best one, capped so a
/// missed mate does not produce absurd numbers.
pub fn cp_loss(best: Score, played: Score) -> i32 {
    (best.to_cp().clamp(-2000, 2000) - played.to_cp().clamp(-2000, 2000)).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        assert_eq!(Score::Cp(35).display(), "+0.35");
        assert_eq!(Score::Cp(-120).display(), "-1.20");
        assert_eq!(Score::Cp(0).display(), "+0.00");
        assert_eq!(Score::Mate(3).display(), "#3");
        assert_eq!(Score::Mate(-2).display(), "-#2");
    }

    #[test]
    fn mate_ordering() {
        assert!(Score::Mate(1).to_cp() > Score::Mate(5).to_cp());
        assert!(Score::Mate(5).to_cp() > Score::Cp(2000).to_cp());
        assert!(Score::Mate(-5).to_cp() > Score::Mate(-1).to_cp());
        assert!(Score::Mate(-5).to_cp() < Score::Cp(-2000).to_cp());
    }

    #[test]
    fn classification() {
        assert_eq!(classify(Score::Cp(30), Score::Cp(30), true), MoveClass::Best);
        assert_eq!(classify(Score::Cp(30), Score::Cp(25), false), MoveClass::Best);
        assert_eq!(classify(Score::Cp(30), Score::Cp(0), false), MoveClass::Good);
        assert_eq!(
            classify(Score::Cp(30), Score::Cp(-100), false),
            MoveClass::Inaccuracy
        );
        assert_eq!(
            classify(Score::Cp(30), Score::Cp(-250), false),
            MoveClass::Mistake
        );
        assert_eq!(
            classify(Score::Cp(30), Score::Cp(-500), false),
            MoveClass::Blunder
        );
        // Big centipawn loss in a won position is not a blunder.
        assert_eq!(classify(Score::Cp(900), Score::Cp(700), false), MoveClass::Good);
        assert_eq!(classify(Score::Mate(2), Score::Cp(50), false), MoveClass::Blunder);
        assert_eq!(classify(Score::Cp(0), Score::Mate(-3), false), MoveClass::Blunder);
    }
}
