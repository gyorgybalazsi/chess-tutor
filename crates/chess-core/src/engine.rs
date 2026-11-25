//! Stockfish over UCI. The engine runs as a separate process; nothing is
//! linked into the app.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use shakmaty::{Chess, Move, Position};

use crate::{
    eval::Score,
    tree::{fen_of, move_to_uci},
};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("could not start the engine at {path}: {source}")]
    Spawn { path: String, source: std::io::Error },
    #[error("engine I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the engine closed unexpectedly")]
    Closed,
    #[error("the engine returned no analysis")]
    NoOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchLimit {
    Depth(u32),
    MoveTimeMs(u64),
}

impl SearchLimit {
    fn go_args(self) -> String {
        match self {
            SearchLimit::Depth(d) => format!("depth {d}"),
            SearchLimit::MoveTimeMs(ms) => format!("movetime {ms}"),
        }
    }
}

/// One principal variation from the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineLine {
    pub multipv: u32,
    pub depth: u32,
    /// From the side to move's point of view, as UCI reports it.
    pub score: Score,
    /// Moves in UCI notation, starting with the candidate move.
    pub pv: Vec<String>,
}

/// Finds a Stockfish binary: `STOCKFISH_PATH`, then `stockfish` on `PATH`,
/// then common install locations.
pub fn find_stockfish() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STOCKFISH_PATH") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = if cfg!(windows) {
        "stockfish.exe"
    } else {
        "stockfish"
    };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    [
        "/usr/games/stockfish",
        "/usr/local/bin/stockfish",
        "/opt/homebrew/bin/stockfish",
        "/usr/bin/stockfish",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

/// Set `CHESS_TUTOR_DEBUG_UCI=1` to print the UCI conversation to stderr.
fn debug_uci() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CHESS_TUTOR_DEBUG_UCI").is_some())
}

pub struct Engine {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    name: String,
    multipv: u32,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("name", &self.name).finish()
    }
}

impl Engine {
    pub fn spawn(path: &Path) -> Result<Engine, EngineError> {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| EngineError::Spawn {
                path: path.display().to_string(),
                source,
            })?;
        let stdin = child.stdin.take().ok_or(EngineError::Closed)?;
        let stdout = BufReader::new(child.stdout.take().ok_or(EngineError::Closed)?);
        let mut engine = Engine {
            child,
            stdin,
            stdout,
            name: String::from("UCI engine"),
            multipv: 1,
        };
        engine.send("uci")?;
        loop {
            let line = engine.read_line()?;
            if let Some(name) = line.strip_prefix("id name ") {
                engine.name = name.trim().to_string();
            }
            if line.trim() == "uciok" {
                break;
            }
        }
        engine.is_ready()?;
        Ok(engine)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn send(&mut self, cmd: &str) -> Result<(), EngineError> {
        if debug_uci() {
            eprintln!("> {cmd}");
        }
        writeln!(self.stdin, "{cmd}")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_line(&mut self) -> Result<String, EngineError> {
        let mut line = String::new();
        if self.stdout.read_line(&mut line)? == 0 {
            return Err(EngineError::Closed);
        }
        if debug_uci() && !line.starts_with("info") {
            eprint!("< {line}");
        }
        Ok(line)
    }

    pub fn set_option(&mut self, name: &str, value: &str) -> Result<(), EngineError> {
        self.send(&format!("setoption name {name} value {value}"))
    }

    pub fn is_ready(&mut self) -> Result<(), EngineError> {
        self.send("isready")?;
        while self.read_line()?.trim() != "readyok" {}
        Ok(())
    }

    pub fn new_game(&mut self) -> Result<(), EngineError> {
        self.send("ucinewgame")?;
        self.is_ready()
    }

    /// Searches `pos` and returns up to `multipv` lines, best first.
    pub fn analyse(
        &mut self,
        pos: &Chess,
        multipv: u32,
        limit: SearchLimit,
    ) -> Result<Vec<EngineLine>, EngineError> {
        self.search(pos, multipv.max(1), limit)
    }

    /// Evaluates one move so its score is comparable with the engine's best
    /// move. The move is played and the resulting position searched one ply
    /// shallower (Stockfish ignores the depth limit when `searchmoves` names
    /// a single move). The returned score is from the mover's point of view
    /// and the line starts with `m`; its second move is the opponent's best
    /// reply.
    pub fn analyse_move(
        &mut self,
        pos: &Chess,
        m: Move,
        limit: SearchLimit,
    ) -> Result<EngineLine, EngineError> {
        let mut after = pos.clone();
        after.play_unchecked(m);
        let limit = match limit {
            SearchLimit::Depth(d) => SearchLimit::Depth(d.saturating_sub(1).max(1)),
            other => other,
        };
        let reply = self
            .search(&after, 1, limit)?
            .into_iter()
            .next()
            .ok_or(EngineError::NoOutput)?;
        let mut pv = vec![move_to_uci(m)];
        pv.extend(reply.pv);
        Ok(EngineLine {
            multipv: 1,
            depth: reply.depth + 1,
            score: reply.score.negate(),
            pv,
        })
    }

    fn search(
        &mut self,
        pos: &Chess,
        multipv: u32,
        limit: SearchLimit,
    ) -> Result<Vec<EngineLine>, EngineError> {
        let legal = pos.legal_moves().len() as u32;
        let multipv = multipv.min(legal.max(1));
        if multipv != self.multipv {
            self.set_option("MultiPV", &multipv.to_string())?;
            self.multipv = multipv;
        }
        self.send(&format!("position fen {}", fen_of(pos)))?;
        self.send(&format!("go {}", limit.go_args()))?;

        let mut lines: BTreeMap<u32, EngineLine> = BTreeMap::new();
        let mut stopped = false;
        loop {
            let line = self.read_line()?;
            if line.starts_with("bestmove") {
                break;
            }
            if let Some(info) = parse_info(&line) {
                // Guard against engines that overshoot the depth limit.
                if let SearchLimit::Depth(d) = limit
                    && info.depth > d
                {
                    if !stopped {
                        self.send("stop")?;
                        stopped = true;
                    }
                    continue;
                }
                lines.insert(info.multipv, info);
            }
        }
        if lines.is_empty() {
            return Err(EngineError::NoOutput);
        }
        Ok(lines.into_values().collect())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.send("quit");
        let _ = self.child.wait();
    }
}

/// Parses a UCI `info` line that carries a score and a PV. Bound-only scores
/// (`lowerbound`/`upperbound`) are skipped because they are not final.
pub fn parse_info(line: &str) -> Option<EngineLine> {
    let mut tokens = line.split_whitespace();
    if tokens.next()? != "info" {
        return None;
    }
    let mut multipv = 1;
    let mut depth = None;
    let mut score = None;
    let mut pv = Vec::new();
    while let Some(tok) = tokens.next() {
        match tok {
            "multipv" => multipv = tokens.next()?.parse().ok()?,
            "depth" => depth = Some(tokens.next()?.parse().ok()?),
            "score" => {
                let kind = tokens.next()?;
                let value: i32 = tokens.next()?.parse().ok()?;
                score = Some(match kind {
                    "cp" => Score::Cp(value),
                    "mate" => Score::Mate(value),
                    _ => return None,
                });
            }
            "lowerbound" | "upperbound" => return None,
            "pv" => {
                pv.extend(tokens.by_ref().map(str::to_string));
            }
            _ => {}
        }
    }
    if pv.is_empty() {
        return None;
    }
    Some(EngineLine {
        multipv,
        depth: depth?,
        score: score?,
        pv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_info_lines() {
        let line = "info depth 18 seldepth 24 multipv 2 score cp -35 nodes 1 nps 1 hashfull 3 tbhits 0 time 10 pv e7e5 g1f3 b8c6";
        let info = parse_info(line).unwrap();
        assert_eq!(info.multipv, 2);
        assert_eq!(info.depth, 18);
        assert_eq!(info.score, Score::Cp(-35));
        assert_eq!(info.pv, ["e7e5", "g1f3", "b8c6"]);

        let mate = parse_info("info depth 5 score mate -2 pv e1e2 d8d2").unwrap();
        assert_eq!(mate.score, Score::Mate(-2));
        assert_eq!(mate.multipv, 1);

        assert!(parse_info("info depth 10 score cp 20 lowerbound pv e2e4").is_none());
        assert!(parse_info("info string NNUE evaluation enabled").is_none());
        assert!(parse_info("bestmove e2e4").is_none());
    }
}
