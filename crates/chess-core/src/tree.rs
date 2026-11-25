//! The move tree behind the analysis board: a main line plus variations.

use shakmaty::{
    CastlingMode, Chess, EnPassantMode, Move, Position,
    fen::Fen,
    san::{San, SanPlus},
    uci::UciMove,
    zobrist::Zobrist64,
};

pub type NodeId = usize;

#[derive(Clone, Debug)]
pub struct Node {
    pub parent: Option<NodeId>,
    /// The first child continues the line; the others are variations.
    pub children: Vec<NodeId>,
    /// The move that led here (`None` for the root).
    pub mv: Option<Move>,
    /// SAN with check suffix, e.g. `Nf3`, `Qxf7#`.
    pub san: Option<String>,
    pub position: Chess,
    /// Half-moves since the root.
    pub depth: u32,
}

impl Node {
    pub fn uci(&self) -> Option<String> {
        self.mv
            .map(|m| UciMove::from_move(m, CastlingMode::Standard).to_string())
    }
}

#[derive(Clone, Debug)]
pub struct GameTree {
    nodes: Vec<Node>,
    current: NodeId,
    /// PGN tags kept from an imported game (White, Black, Event, ...).
    pub headers: Vec<(String, String)>,
}

impl Default for GameTree {
    fn default() -> Self {
        GameTree::new(Chess::default())
    }
}

impl GameTree {
    pub fn new(start: Chess) -> GameTree {
        GameTree {
            nodes: vec![Node {
                parent: None,
                children: Vec::new(),
                mv: None,
                san: None,
                position: start,
                depth: 0,
            }],
            current: 0,
            headers: Vec::new(),
        }
    }

    pub const ROOT: NodeId = 0;

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() == 1
    }

    pub fn current(&self) -> NodeId {
        self.current
    }

    pub fn current_node(&self) -> &Node {
        &self.nodes[self.current]
    }

    pub fn position(&self) -> &Chess {
        &self.nodes[self.current].position
    }

    pub fn start_position(&self) -> &Chess {
        &self.nodes[Self::ROOT].position
    }

    pub fn set_current(&mut self, id: NodeId) {
        assert!(id < self.nodes.len(), "node {id} does not exist");
        self.current = id;
    }

    /// Plays a legal move from the current node and moves to it. If that
    /// move was already explored, the existing node is reused; otherwise it
    /// becomes a new variation.
    pub fn play(&mut self, m: Move) -> Result<NodeId, IllegalMove> {
        let id = self.add_move(self.current, m)?;
        self.current = id;
        Ok(id)
    }

    /// Adds a move below `parent` without changing the current node.
    pub fn add_move(&mut self, parent: NodeId, m: Move) -> Result<NodeId, IllegalMove> {
        if let Some(&existing) = self.nodes[parent]
            .children
            .iter()
            .find(|&&c| self.nodes[c].mv == Some(m))
        {
            return Ok(existing);
        }
        let before = &self.nodes[parent].position;
        if !before.is_legal(m) {
            return Err(IllegalMove(format!("{m:?}")));
        }
        let mut after = before.clone();
        let san = SanPlus::from_move_and_play_unchecked(&mut after, m).to_string();
        let id = self.nodes.len();
        let depth = self.nodes[parent].depth + 1;
        self.nodes.push(Node {
            parent: Some(parent),
            children: Vec::new(),
            mv: Some(m),
            san: Some(san),
            position: after,
            depth,
        });
        self.nodes[parent].children.push(id);
        Ok(id)
    }

    /// Plays a move given in SAN (`Nf3`, `exd5`, `O-O`, `e8=Q+`).
    pub fn play_san(&mut self, san: &str) -> Result<NodeId, IllegalMove> {
        let parsed: San = SanPlus::from_ascii(san.trim().as_bytes())
            .map_err(|_| IllegalMove(san.to_string()))?
            .san;
        let m = parsed
            .to_move(self.position())
            .map_err(|_| IllegalMove(san.to_string()))?;
        self.play(m)
    }

    /// Plays a move given in UCI notation (`e2e4`, `e7e8q`).
    pub fn play_uci(&mut self, uci: &str) -> Result<NodeId, IllegalMove> {
        let m = uci_to_move(self.position(), uci)?;
        self.play(m)
    }

    pub fn back(&mut self) -> bool {
        match self.nodes[self.current].parent {
            Some(p) => {
                self.current = p;
                true
            }
            None => false,
        }
    }

    /// Follows the first child (the continuation of the current line).
    pub fn forward(&mut self) -> bool {
        match self.nodes[self.current].children.first() {
            Some(&c) => {
                self.current = c;
                true
            }
            None => false,
        }
    }

    pub fn to_start(&mut self) {
        self.current = Self::ROOT;
    }

    pub fn to_end(&mut self) {
        while self.forward() {}
    }

    /// Node ids from the root to `id`, inclusive.
    pub fn path_to(&self, id: NodeId) -> Vec<NodeId> {
        let mut path = vec![id];
        let mut n = id;
        while let Some(p) = self.nodes[n].parent {
            path.push(p);
            n = p;
        }
        path.reverse();
        path
    }

    /// SAN moves from the root to `id`.
    pub fn san_history(&self, id: NodeId) -> Vec<String> {
        self.path_to(id)
            .into_iter()
            .filter_map(|n| self.nodes[n].san.clone())
            .collect()
    }

    /// SAN history with move numbers, e.g. `1. e4 c5 2. Nf3`.
    pub fn numbered_history(&self, id: NodeId) -> String {
        numbered_line(self.start_position(), &self.san_history(id))
    }

    /// Zobrist hashes of every position from the root to `id`, for
    /// repetition detection.
    pub fn position_hashes(&self, id: NodeId) -> Vec<u64> {
        self.path_to(id)
            .into_iter()
            .map(|n| position_hash(&self.nodes[n].position))
            .collect()
    }

    /// Whether `id` lies on the main line (first child at every step).
    pub fn is_mainline(&self, id: NodeId) -> bool {
        let mut n = id;
        while let Some(p) = self.nodes[n].parent {
            if self.nodes[p].children.first() != Some(&n) {
                return false;
            }
            n = p;
        }
        true
    }

    /// Makes the variation containing `id` the main line at every branch point.
    pub fn promote_to_mainline(&mut self, id: NodeId) {
        let mut n = id;
        while let Some(p) = self.nodes[n].parent {
            let children = &mut self.nodes[p].children;
            if let Some(i) = children.iter().position(|&c| c == n) {
                let c = children.remove(i);
                children.insert(0, c);
            }
            n = p;
        }
    }

    pub fn start_fen(&self) -> String {
        fen_of(self.start_position())
    }

    pub fn current_fen(&self) -> String {
        fen_of(self.position())
    }

    /// Exports the whole tree, variations included, as PGN.
    pub fn to_pgn(&self) -> String {
        let mut out = String::new();
        let start_fen = self.start_fen();
        let standard_start = start_fen == fen_of(&Chess::default());
        let mut has_result = false;
        for (k, v) in &self.headers {
            if k == "FEN" || k == "SetUp" {
                continue;
            }
            has_result |= k == "Result";
            out.push_str(&format!("[{k} \"{}\"]\n", v.replace('"', "'")));
        }
        if !has_result {
            out.push_str("[Result \"*\"]\n");
        }
        if !standard_start {
            out.push_str("[SetUp \"1\"]\n");
            out.push_str(&format!("[FEN \"{start_fen}\"]\n"));
        }
        out.push('\n');
        let mut movetext = String::new();
        self.write_movetext(Self::ROOT, &mut movetext, true);
        let result = self
            .headers
            .iter()
            .find(|(k, _)| k == "Result")
            .map(|(_, v)| v.as_str())
            .unwrap_or("*");
        movetext.push_str(result);
        out.push_str(movetext.trim_start());
        out.push('\n');
        out
    }

    fn write_movetext(&self, from: NodeId, out: &mut String, mut need_number: bool) {
        let mut node = from;
        loop {
            let children = &self.nodes[node].children;
            let Some(&main) = children.first() else {
                return;
            };
            self.write_move(main, out, need_number);
            need_number = false;
            for &var in &children[1..] {
                out.push_str("( ");
                self.write_move(var, out, true);
                self.write_movetext(var, out, false);
                out.push_str(") ");
                need_number = true;
            }
            node = main;
        }
    }

    fn write_move(&self, id: NodeId, out: &mut String, force_number: bool) {
        let n = &self.nodes[id];
        let parent = &self.nodes[n.parent.expect("moves have parents")].position;
        let number = parent.fullmoves().get();
        if parent.turn().is_white() {
            out.push_str(&format!("{number}. "));
        } else if force_number {
            out.push_str(&format!("{number}... "));
        }
        out.push_str(n.san.as_deref().unwrap_or("?"));
        out.push(' ');
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("illegal or unreadable move: {0}")]
pub struct IllegalMove(pub String);

pub fn uci_to_move(pos: &Chess, uci: &str) -> Result<Move, IllegalMove> {
    let u: UciMove = uci.trim().parse().map_err(|_| IllegalMove(uci.to_string()))?;
    u.to_move(pos).map_err(|_| IllegalMove(uci.to_string()))
}

pub fn move_to_uci(m: Move) -> String {
    UciMove::from_move(m, CastlingMode::Standard).to_string()
}

pub fn fen_of(pos: &Chess) -> String {
    Fen::from_position(pos, EnPassantMode::Legal).to_string()
}

pub fn position_hash(pos: &Chess) -> u64 {
    pos.zobrist_hash::<Zobrist64>(EnPassantMode::Legal).0
}

/// Formats SAN moves played from `start` with move numbers.
pub fn numbered_line(start: &Chess, sans: &[String]) -> String {
    let mut out = String::new();
    let mut number = start.fullmoves().get();
    let mut white = start.turn().is_white();
    for (i, san) in sans.iter().enumerate() {
        if white {
            out.push_str(&format!("{number}. "));
        } else if i == 0 {
            out.push_str(&format!("{number}... "));
        }
        out.push_str(san);
        out.push(' ');
        if !white {
            number += 1;
        }
        white = !white;
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_and_branch() {
        let mut t = GameTree::default();
        let e4 = t.play_san("e4").unwrap();
        t.play_san("e5").unwrap();
        t.back();
        let c5 = t.play_san("c5").unwrap();
        assert_eq!(t.node(e4).children.len(), 2);
        assert!(!t.is_mainline(c5));
        // Replaying an explored move reuses the node.
        t.back();
        assert_eq!(t.play_san("c5").unwrap(), c5);
        t.promote_to_mainline(c5);
        assert!(t.is_mainline(c5));
        assert_eq!(t.numbered_history(c5), "1. e4 c5");
    }

    #[test]
    fn illegal_moves_are_rejected() {
        let mut t = GameTree::default();
        assert!(t.play_san("e5").is_err());
        assert!(t.play_uci("e2e5").is_err());
        assert!(t.play_uci("e2e4").is_ok());
    }

    #[test]
    fn pgn_export_with_variation() {
        let mut t = GameTree::default();
        t.play_san("e4").unwrap();
        t.play_san("e5").unwrap();
        t.play_san("Nf3").unwrap();
        t.back();
        t.back();
        t.play_san("c5").unwrap();
        let pgn = t.to_pgn();
        assert!(pgn.contains("1. e4 e5 ( 1... c5 ) 2. Nf3 *"), "{pgn}");
    }

    #[test]
    fn numbered_from_black() {
        let pos = Chess::default()
            .play(uci_to_move(&Chess::default(), "e2e4").unwrap())
            .unwrap();
        let line = numbered_line(&pos, &["c5".into(), "Nf3".into(), "d6".into()]);
        assert_eq!(line, "1... c5 2. Nf3 d6");
    }
}
