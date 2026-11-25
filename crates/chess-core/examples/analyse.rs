//! Prints the engine report and the Claude prompt for a move, without
//! calling Claude. Useful for checking what the tutor would send.
//!
//! cargo run -p chess-core --example analyse -- "<FEN>" [uci-move] [depth]

use std::time::Instant;

use chess_core::{
    analysis::{AnalysisConfig, analyse_move, analyse_position},
    engine::{Engine, SearchLimit, find_stockfish},
    import::parse_fen,
    prompt,
    tree::{position_hash, uci_to_move},
};

fn main() {
    let mut args = std::env::args().skip(1);
    let fen = args.next().expect("usage: analyse <FEN> [uci-move] [depth]");
    let mv = args.next();
    let depth = args.next().and_then(|d| d.parse().ok()).unwrap_or(18);
    let pos = parse_fen(&fen).expect("valid FEN");
    let mut engine = Engine::spawn(&find_stockfish().expect("Stockfish not found")).expect("engine starts");
    let cfg = AnalysisConfig {
        limit: SearchLimit::Depth(depth),
        ..Default::default()
    };
    let history = [position_hash(&pos)];

    let t = Instant::now();
    let before = analyse_position(&mut engine, &pos, &history, &cfg).expect("analysis");
    eprintln!("position analysed in {:?}", t.elapsed());
    match mv {
        Some(uci) => {
            let m = uci_to_move(&pos, &uci).expect("legal move");
            let t = Instant::now();
            let report = analyse_move(&mut engine, &pos, &before, m, "", &history, &cfg).expect("analysis");
            eprintln!("move analysed in {:?}", t.elapsed());
            let req = prompt::explain_move(&report);
            println!("model: {}\n\n{}", req.model.id(), prompt::move_message(&report));
        }
        None => println!("{}", serde_json::to_string_pretty(&before).expect("json")),
    }
}
