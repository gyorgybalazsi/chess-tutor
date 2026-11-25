# chess-tutor

Chess analysis board in Rust that pairs Stockfish evaluations with plain-language explanations from Claude.

It works like the Chess.com and Lichess analysis boards: start from the initial position, or load a position or a game from FEN, PGN or a screenshot, then play legal moves for both colours. Stockfish evaluates every move you try, and Claude explains what the move gains and what it gives up. Every explanation rests on what the engine actually calculated, not on Claude's own move calculation.

## Features

- **Analysis board**: click a piece, then its target square. Alternatives branch into variations, shown indented in the move list; any variation can be promoted to the main line. ←/→/Home/End navigate.
- **Every move evaluated**: the move is compared with the engine's best move in the same position and classified as best, good, inaccuracy, mistake or blunder. The eval bar, the engine's top lines (MultiPV) and a best-move arrow update as you go.
- **Explanations**: one Claude request per move, containing the engine's candidates, the opponent's best reply, and board facts computed with shakmaty for every move of each line (attacks, defences, pins, discovered attacks, files opened, hanging pieces, trades, repetitions, material). You can ask follow-up questions about an explanation.
- **Inputs**: paste a FEN, a PGN (variations are kept) or bare movetext like `1. e4 c5 2. Nf3`; load a screenshot from Chess.com, Duolingo or similar apps; or pass a PGN file or FEN on the command line.
- **Export**: copy the current FEN, or the whole tree as PGN with variations.

## Requirements

- Rust 1.85 or later (edition 2024).
- [Stockfish](https://stockfishchess.org/download/). The app looks for `STOCKFISH_PATH`, then `stockfish` on `PATH`, then common install locations. The path can also be set in Settings.
- For explanations: an Anthropic API key stored in 1Password, and the [1Password CLI](https://developer.1password.com/docs/cli/) (`op`).

## API key

The app reads the key from 1Password at startup and keeps it only in memory. The key never goes into config files, logs or the repository. Tell the app where the key lives with a secret reference (not the key itself):

```sh
export CHESS_TUTOR_OP_REF="op://Private/Anthropic/credential"
```

At startup it runs `op read "$CHESS_TUTOR_OP_REF"`. For development without 1Password, `ANTHROPIC_API_KEY` is accepted as a fallback when `CHESS_TUTOR_OP_REF` is not set. Without a key, the board and engine work normally and the Explain button stays disabled.

## Running

```sh
cargo run --release                       # start from the initial position
cargo run --release -- game.pgn           # load a game
cargo run --release -- "<FEN>"            # load a position
```

To see exactly what would be sent to Claude for a move, without calling the API:

```sh
cargo run --release -p chess-core --example analyse -- "<FEN>" e2e4
```

Set `CHESS_TUTOR_DEBUG_UCI=1` to print the UCI conversation with Stockfish to stderr.

## Architecture

A Cargo workspace with two crates:

- `crates/chess-core`: all move logic and engine access, no UI.
  - `tree`: the move tree (main line plus variations), PGN export.
  - `import`: FEN and PGN parsing, validated with shakmaty and pgn-reader.
  - `engine`: Stockfish as a separate process over UCI.
  - `eval`: scores, winning chances and move classification.
  - `annotate`: per-move board facts and repetition detection.
  - `analysis`: runs the engine checks and builds one report per move.
  - `prompt`: the system prompt and request builders.
  - `claude`: a small blocking client for the Messages API.
  - `screenshot`: screenshot to FEN/PGN with Claude.
  - `secrets`: loading the key from 1Password.
- `crates/tutor-ui`: the egui app. One background thread owns the Stockfish process; each Claude request runs on its own thread.

An MCP server is left for a later version, as a thin wrapper over `chess-core` for use in Claude Code or Claude Desktop.

## How explanations work

The engine calculates and Claude translates. For each move:

1. The position before the move is analysed with MultiPV (3 candidates, depth 18 by default).
2. The played move is evaluated at the same depth. If it is not the engine's choice, the opponent's best reply comes from the engine; that reply is usually why the move is worse.
3. Each line is walked with shakmaty to note what every move attacks, defends, pins or opens, whether pieces are left hanging, which trades happen and whether positions repeat.
4. The move is classified from the drop in winning chances (the logistic curve Lichess uses): within 10 centipawns of the best move is *best*; a drop under 10 points of winning chances is *good*, under 20 an *inaccuracy*, under 30 a *mistake*, otherwise a *blunder*.
5. Everything goes to Claude in one request. The system prompt tells Claude to make no claim that is not in the data, and to stay with general ideas when there is no engine data.

## Models and cost

| Use | Model | Effort |
| --- | --- | --- |
| Normal explanations | Claude Sonnet 5.5 | low |
| Sharp tactical positions (a mate on the board, a gap of 1.5 pawns or more between the top two moves, or a move losing 2 pawns or more) | Claude Opus 5.5 | medium |
| Screenshot to FEN | Claude Haiku 5.5 | low |

A typical explanation is about 2,000 input and 500 output tokens: roughly $0.009 with Sonnet 5.5 and $0.018 with Opus 5.5 at list prices, plus any thinking tokens. The system prompt is cached, which adds up in follow-up conversations. A screenshot is converted to a FEN once; only the FEN is used afterwards. The status bar shows the running cost of the session.

Explanation requests opt into server-side fallbacks (`fallbacks: "default"`), so a request one model declines is retried on another.

## Licence

GPL-3.0-or-later, because shakmaty and pgn-reader are GPL-3.0 and compiled into the app. See [LICENSE](LICENSE). Stockfish is not bundled; if you distribute a build that bundles the Stockfish binary, also link to [its source](https://github.com/official-stockfish/Stockfish).
