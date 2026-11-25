//! The application window: board on the left, engine lines, explanation and
//! move list on the right.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, mpsc},
};

use chess_core::{
    analysis::{AnalysisConfig, MoveReport, PositionAnalysis},
    claude::{ClaudeClient, Message, Request},
    engine::{SearchLimit, find_stockfish},
    eval::MoveClass,
    import::load_text,
    prompt,
    screenshot::{ScreenshotResult, with_other_side_to_move},
    secrets::{KeySource, OP_REF_VAR, load_api_key},
    shakmaty::{Move, Position},
    tree::{GameTree, NodeId, fen_of, uci_to_move},
};
use eframe::egui::{self, Color32, RichText};

use crate::{
    board::{BoardInput, BoardView, board_ui},
    worker::{
        ClaudeEvent, EngineEvent, EngineJob, ParentInfo, spawn_claude_request, spawn_engine_worker,
        spawn_screenshot_request,
    },
};

#[derive(Default)]
struct NodeState {
    /// Engine view of the position at this node.
    position: Option<PositionAnalysis>,
    /// How the move into this node compares with the engine's choice.
    report: Option<MoveReport>,
    engine_pending: bool,
    engine_error: Option<String>,
    chat: Option<Chat>,
    claude_pending: bool,
    claude_error: Option<String>,
}

struct Chat {
    base: Request,
    messages: Vec<Message>,
    /// (question, answer); the first entry has no question.
    turns: Vec<(Option<String>, String)>,
    model_label: &'static str,
    cost: f64,
}

struct Settings {
    depth: u32,
    multipv: u32,
    auto_explain: bool,
    show_arrow: bool,
    stockfish_path: String,
}

enum ScreenshotState {
    Idle,
    Reading,
    Review {
        result: Box<ScreenshotResult>,
        fen: String,
    },
    Failed(String),
}

pub struct TutorApp {
    tree: GameTree,
    generation: u64,
    nodes: HashMap<NodeId, NodeState>,
    view: BoardView,
    settings: Settings,
    show_settings: bool,
    engine_tx: mpsc::Sender<EngineJob>,
    engine_rx: mpsc::Receiver<EngineEvent>,
    engine_status: Result<String, String>,
    claude: Option<Arc<ClaudeClient>>,
    claude_status: String,
    claude_tx: mpsc::Sender<ClaudeEvent>,
    claude_rx: mpsc::Receiver<ClaudeEvent>,
    session_cost: f64,
    import_text: Option<String>,
    import_error: Option<String>,
    screenshot: ScreenshotState,
    question: String,
    glyphs: bool,
    toast: Option<String>,
}

impl TutorApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> TutorApp {
        let glyphs = crate::install_chess_font(&cc.egui_ctx);
        let stockfish = find_stockfish();
        let (engine_tx, engine_rx) = spawn_engine_worker(stockfish.clone(), cc.egui_ctx.clone());
        let (claude_tx, claude_rx) = mpsc::channel();
        // The key is read once, from 1Password, and kept only in memory.
        let (claude, claude_status) = match load_api_key() {
            Ok((key, source)) => (
                Some(Arc::new(ClaudeClient::new(key))),
                match source {
                    KeySource::OnePassword => "Claude: key loaded from 1Password".to_string(),
                    KeySource::Environment => "Claude: key from ANTHROPIC_API_KEY".to_string(),
                },
            ),
            Err(e) => (None, format!("Claude unavailable: {e}")),
        };
        // Optional start position: `chess-tutor game.pgn` or `chess-tutor "<FEN>"`.
        let mut toast = None;
        let tree = match std::env::args().nth(1) {
            Some(arg) => {
                let text = std::fs::read_to_string(&arg).unwrap_or(arg);
                load_text(&text).unwrap_or_else(|e| {
                    toast = Some(format!("Could not load the start position: {e}"));
                    GameTree::default()
                })
            }
            None => GameTree::default(),
        };
        TutorApp {
            tree,
            generation: 0,
            nodes: HashMap::new(),
            view: BoardView::default(),
            settings: Settings {
                depth: 18,
                multipv: 3,
                auto_explain: true,
                show_arrow: true,
                stockfish_path: stockfish.map(|p| p.display().to_string()).unwrap_or_default(),
            },
            show_settings: false,
            engine_tx,
            engine_rx,
            engine_status: Err("starting engine…".into()),
            claude,
            claude_status,
            claude_tx,
            claude_rx,
            session_cost: 0.0,
            import_text: None,
            import_error: None,
            screenshot: ScreenshotState::Idle,
            question: String::new(),
            glyphs,
            toast,
        }
    }

    fn config(&self) -> AnalysisConfig {
        AnalysisConfig {
            limit: SearchLimit::Depth(self.settings.depth),
            multipv: self.settings.multipv,
            line_plies: 8,
        }
    }

    fn reset_analysis(&mut self) {
        self.generation += 1;
        self.nodes.clear();
        self.view = BoardView {
            flipped: self.view.flipped,
            ..Default::default()
        };
    }

    fn load_tree(&mut self, tree: GameTree) {
        self.tree = tree;
        self.reset_analysis();
    }

    fn engine_running(&self) -> bool {
        self.engine_status.is_ok()
    }

    /// Asks the engine for whatever the current node is missing.
    fn ensure_analysis(&mut self) {
        if !self.engine_running() {
            return;
        }
        let id = self.tree.current();
        let node = self.tree.node(id);
        let state = self.nodes.entry(id).or_default();
        let needs_report = node.mv.is_some() && state.report.is_none();
        if state.engine_pending || state.engine_error.is_some() || (state.position.is_some() && !needs_report)
        {
            return;
        }
        state.engine_pending = true;
        let parent = match (node.parent, node.mv) {
            (Some(p), Some(mv)) if needs_report => Some(ParentInfo {
                id: p,
                position: self.tree.node(p).position.clone(),
                mv,
                history: self.tree.position_hashes(p),
                moves_so_far: self.tree.numbered_history(p),
            }),
            _ => None,
        };
        let _ = self.engine_tx.send(EngineJob::Analyse {
            generation: self.generation,
            node: id,
            position: node.position.clone(),
            history: self.tree.position_hashes(id),
            parent,
            config: self.config(),
        });
    }

    fn handle_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.engine_rx.try_recv() {
            match ev {
                EngineEvent::Ready(name) => {
                    self.engine_status = Ok(name);
                    self.reset_analysis();
                }
                EngineEvent::Failed(msg) => self.engine_status = Err(msg),
                EngineEvent::Position {
                    generation,
                    node,
                    analysis,
                } if generation == self.generation => {
                    let s = self.nodes.entry(node).or_default();
                    s.position = Some(analysis);
                    if self.tree.node(node).mv.is_none() || s.report.is_some() {
                        s.engine_pending = false;
                    }
                }
                EngineEvent::Move {
                    generation,
                    node,
                    report,
                } if generation == self.generation => {
                    self.nodes.entry(node).or_default().report = Some(*report);
                }
                EngineEvent::Dropped { generation, node } if generation == self.generation => {
                    self.nodes.entry(node).or_default().engine_pending = false;
                }
                EngineEvent::Error {
                    generation,
                    node,
                    message,
                } if generation == self.generation => {
                    let s = self.nodes.entry(node).or_default();
                    s.engine_pending = false;
                    s.engine_error = Some(message);
                }
                _ => {}
            }
        }
        while let Ok(ev) = self.claude_rx.try_recv() {
            match ev {
                ClaudeEvent::Explanation {
                    generation,
                    node,
                    request,
                    result,
                } => {
                    if generation != self.generation {
                        continue;
                    }
                    let s = self.nodes.entry(node).or_default();
                    s.claude_pending = false;
                    match result {
                        Ok(reply) => {
                            let cost = reply.usage.cost(reply.model);
                            self.session_cost += cost;
                            let question = (request.messages.len() > 1).then(|| self.question_from(&request));
                            let s = self.nodes.entry(node).or_default();
                            let mut messages = request.messages.clone();
                            messages.push(Message::assistant(reply.text.clone()));
                            match &mut s.chat {
                                Some(chat) if question.is_some() => {
                                    chat.messages = messages;
                                    chat.turns.push((question, reply.text));
                                    chat.cost += cost;
                                }
                                _ => {
                                    s.chat = Some(Chat {
                                        base: request,
                                        messages,
                                        turns: vec![(None, reply.text)],
                                        model_label: reply.model.label(),
                                        cost,
                                    });
                                }
                            }
                            s.claude_error = None;
                        }
                        Err(e) => s.claude_error = Some(e),
                    }
                }
                ClaudeEvent::Screenshot(result) => {
                    self.screenshot = match result {
                        Ok(result) => {
                            self.session_cost += result.usage.cost(chess_core::claude::Model::Haiku);
                            let fen = result.reading.fen.clone();
                            ScreenshotState::Review {
                                result: Box::new(result),
                                fen,
                            }
                        }
                        Err(e) => ScreenshotState::Failed(e),
                    };
                }
            }
        }
        let _ = ctx;
    }

    fn question_from(&self, request: &Request) -> String {
        // The last user message starts with the question itself.
        request
            .messages
            .last()
            .and_then(|m| m.content.as_str())
            .map(|s| s.split("\n\n(Answer from").next().unwrap_or(s).to_string())
            .unwrap_or_default()
    }

    fn request_explanation(&mut self, ctx: &egui::Context, node: NodeId) {
        let Some(client) = self.claude.clone() else {
            return;
        };
        let engine_running = self.engine_running();
        let s = self.nodes.entry(node).or_default();
        if s.claude_pending {
            return;
        }
        let request = if let Some(report) = &s.report {
            prompt::explain_move(report)
        } else if let (Some(pos), None) = (&s.position, self.tree.node(node).mv) {
            prompt::explain_position(pos, &self.tree.numbered_history(node))
        } else if !engine_running {
            let n = self.tree.node(node);
            let parent_hist = n
                .parent
                .map(|p| self.tree.numbered_history(p))
                .unwrap_or_default();
            prompt::explain_without_engine(&fen_of(&n.position), &parent_hist, n.san.as_deref())
        } else {
            return;
        };
        s.claude_pending = true;
        s.claude_error = None;
        spawn_claude_request(
            client,
            request,
            self.generation,
            node,
            self.claude_tx.clone(),
            ctx.clone(),
        );
    }

    fn ask_follow_up(&mut self, ctx: &egui::Context, node: NodeId, question: String) {
        let Some(client) = self.claude.clone() else {
            return;
        };
        let s = self.nodes.entry(node).or_default();
        let Some(chat) = &s.chat else { return };
        let request = prompt::follow_up(&chat.base, &chat.messages, &question);
        s.claude_pending = true;
        spawn_claude_request(
            client,
            request,
            self.generation,
            node,
            self.claude_tx.clone(),
            ctx.clone(),
        );
    }

    fn play(&mut self, m: Move) {
        if self.tree.play(m).is_ok() {
            self.view.selected = None;
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("New game").clicked() {
                self.load_tree(GameTree::default());
            }
            if ui.button("Load FEN / PGN…").clicked() {
                self.import_text = Some(String::new());
                self.import_error = None;
            }
            let shot = ui.add_enabled(self.claude.is_some(), egui::Button::new("Load screenshot…"));
            if shot.clicked() {
                self.pick_screenshot(ctx);
            }
            ui.separator();
            if ui.button("Copy FEN").clicked() {
                ctx.copy_text(self.tree.current_fen());
                self.toast = Some("FEN copied".into());
            }
            if ui.button("Copy PGN").clicked() {
                ctx.copy_text(self.tree.to_pgn());
                self.toast = Some("PGN copied".into());
            }
            if ui.button("Flip board").clicked() {
                self.view.flipped = !self.view.flipped;
            }
            ui.separator();
            if ui.button("Settings").clicked() {
                self.show_settings = !self.show_settings;
            }
        });
    }

    fn pick_screenshot(&mut self, ctx: &egui::Context) {
        let Some(client) = self.claude.clone() else {
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif"])
            .pick_file()
        else {
            return;
        };
        match std::fs::read(&path) {
            Ok(bytes) => {
                self.screenshot = ScreenshotState::Reading;
                spawn_screenshot_request(client, bytes, self.claude_tx.clone(), ctx.clone());
            }
            Err(e) => {
                self.screenshot = ScreenshotState::Failed(format!("could not read {}: {e}", path.display()))
            }
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            match &self.engine_status {
                Ok(name) => ui.label(format!("Engine: {name}, depth {}", self.settings.depth)),
                Err(e) => ui.colored_label(Color32::from_rgb(200, 80, 60), format!("Engine: {e}")),
            };
            ui.separator();
            ui.label(&self.claude_status);
            ui.separator();
            ui.label(format!("Claude cost this session: ${:.4}", self.session_cost));
            if let Some(t) = &self.toast {
                ui.separator();
                ui.label(t);
            }
        });
    }

    fn right_panel(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let id = self.tree.current();
        let node = self.tree.node(id).clone();
        let mut action: Option<Action> = None;

        // Header: the move just played and its verdict.
        let state = self.nodes.get(&id);
        let report = state.and_then(|s| s.report.as_ref());
        match (&node.san, report) {
            (Some(san), Some(r)) => {
                let parent = &self.tree.node(node.parent.expect("move has parent")).position;
                let number = parent.fullmoves().get();
                let dots = if parent.turn().is_white() { "." } else { "..." };
                ui.horizontal(|ui| {
                    ui.heading(format!("{number}{dots} {san}"));
                    let class = r.played.classification;
                    ui.label(RichText::new(class.label()).strong().color(class_color(class)));
                    ui.label(format!("eval {}", r.played.eval_text));
                });
                if !r.played.is_engine_choice
                    && let Some(best) = r.position_before.best()
                {
                    ui.label(format!(
                        "Engine preferred {} ({}), {} centipawns better.",
                        best.san, best.eval_text, r.played.cp_loss
                    ));
                }
                if let Some(reply) = &r.played.best_reply {
                    ui.label(format!("Best reply: {reply}   Line: {}", r.played.line));
                }
            }
            (Some(san), None) => {
                ui.heading(san);
            }
            (None, _) => {
                ui.heading("Start position");
            }
        }
        if let Some(s) = state {
            if s.engine_pending {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Stockfish is thinking…");
                });
            }
            if let Some(e) = &s.engine_error {
                ui.colored_label(Color32::from_rgb(200, 80, 60), format!("Engine error: {e}"));
            }
        }
        if let Some(over) = state
            .and_then(|s| s.position.as_ref())
            .and_then(|p| p.game_over.as_ref())
        {
            ui.label(RichText::new(format!("Game over: {over}")).strong());
        }

        ui.separator();
        ui.label(RichText::new("Explanation").strong());
        let can_explain = self.claude.is_some()
            && (report.is_some()
                || (node.mv.is_none() && state.and_then(|s| s.position.as_ref()).is_some())
                || !self.engine_running());
        egui::ScrollArea::vertical().id_salt("explanation").max_height(ui.available_height() * 0.5).show(ui, |ui| {
            let state = self.nodes.get(&id);
            match state.and_then(|s| s.chat.as_ref()) {
                Some(chat) => {
                    for (q, a) in &chat.turns {
                        if let Some(q) = q {
                            ui.label(RichText::new(format!("You: {q}")).italics());
                        }
                        ui.label(a);
                        ui.add_space(6.0);
                    }
                    ui.label(
                        RichText::new(format!("{} · ${:.4}", chat.model_label, chat.cost)).small().weak(),
                    );
                }
                None => {
                    if self.claude.is_none() {
                        ui.label(format!(
                            "Explanations need an Anthropic API key. Set {OP_REF_VAR} to its 1Password reference \
                             (for example op://Private/Anthropic/credential) and restart."
                        ));
                    } else if !self.settings.auto_explain || node.mv.is_none() {
                        ui.label(RichText::new("Press Explain for Claude's explanation.").weak());
                    }
                }
            }
            if state.is_some_and(|s| s.claude_pending) {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Claude is writing…");
                });
            }
            if let Some(e) = state.and_then(|s| s.claude_error.as_ref()) {
                ui.colored_label(Color32::from_rgb(200, 80, 60), e);
            }
        });
        ui.horizontal(|ui| {
            let has_chat = self.nodes.get(&id).is_some_and(|s| s.chat.is_some());
            let label = if has_chat { "Explain again" } else { "Explain" };
            if ui.add_enabled(can_explain, egui::Button::new(label)).clicked() {
                action = Some(Action::Explain(id));
            }
            if has_chat {
                let edit =
                    ui.add(egui::TextEdit::singleline(&mut self.question).hint_text("Ask a follow-up…"));
                let send = ui.button("Ask").clicked()
                    || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if send && !self.question.trim().is_empty() {
                    action = Some(Action::FollowUp(id, std::mem::take(&mut self.question)));
                }
            }
        });

        ui.separator();
        ui.label(RichText::new("Engine lines here").strong());
        if let Some(pos) = self.nodes.get(&id).and_then(|s| s.position.as_ref()) {
            for c in &pos.candidates {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&c.eval_text).monospace().strong());
                    if ui.link(&c.san).on_hover_text("Play this move").clicked() {
                        action = Some(Action::PlayUci(c.uci.clone()));
                    }
                    ui.label(RichText::new(&c.line).weak());
                });
            }
        }

        ui.separator();
        ui.label(RichText::new("Moves").strong());
        egui::ScrollArea::vertical().id_salt("moves").show(ui, |ui| {
            if let Some(&first) = self.tree.node(GameTree::ROOT).children.first() {
                let mut clicked = None;
                self.render_sequence(ui, first, true, &mut clicked);
                if let Some(n) = clicked {
                    action = Some(Action::Goto(n));
                }
            } else {
                ui.label(RichText::new("Play a move on the board.").weak());
            }
            if id != GameTree::ROOT
                && !self.tree.is_mainline(id)
                && ui.small_button("Promote line to main line").clicked()
            {
                action = Some(Action::Promote(id));
            }
        });

        match action {
            Some(Action::Explain(n)) => self.request_explanation(ctx, n),
            Some(Action::FollowUp(n, q)) => self.ask_follow_up(ctx, n, q),
            Some(Action::PlayUci(u)) => {
                if let Ok(m) = uci_to_move(self.tree.position(), &u) {
                    self.play(m);
                }
            }
            Some(Action::Goto(n)) => self.tree.set_current(n),
            Some(Action::Promote(n)) => self.tree.promote_to_mainline(n),
            None => {}
        }
    }

    /// Renders a line starting with the move `first`; variations (siblings)
    /// are shown indented under the move they replace.
    fn render_sequence(
        &self,
        ui: &mut egui::Ui,
        first: NodeId,
        show_alts_of_first: bool,
        clicked: &mut Option<NodeId>,
    ) {
        let tree = &self.tree;
        let mut n = first;
        let mut alts = show_alts_of_first;
        loop {
            let mut run = Vec::new();
            let mut has_alts = false;
            loop {
                run.push(n);
                let parent = tree.node(n).parent.expect("move has parent");
                if alts && tree.node(parent).children.len() > 1 {
                    has_alts = true;
                    break;
                }
                alts = true;
                match tree.node(n).children.first() {
                    Some(&c) => n = c,
                    None => break,
                }
            }
            ui.horizontal_wrapped(|ui| {
                for (i, &m) in run.iter().enumerate() {
                    let node = tree.node(m);
                    let parent = &tree.node(node.parent.expect("move has parent")).position;
                    let num = parent.fullmoves().get();
                    if parent.turn().is_white() {
                        ui.label(RichText::new(format!("{num}.")).weak());
                    } else if i == 0 {
                        ui.label(RichText::new(format!("{num}...")).weak());
                    }
                    let mut text = RichText::new(node.san.as_deref().unwrap_or("?"));
                    if let Some(class) = self
                        .nodes
                        .get(&m)
                        .and_then(|s| s.report.as_ref())
                        .map(|r| r.played.classification)
                        && matches!(
                            class,
                            MoveClass::Inaccuracy | MoveClass::Mistake | MoveClass::Blunder
                        )
                    {
                        text = text.color(class_color(class));
                    }
                    if ui.selectable_label(m == tree.current(), text).clicked() {
                        *clicked = Some(m);
                    }
                }
            });
            let last = *run.last().expect("run is not empty");
            if has_alts {
                let parent = tree.node(last).parent.expect("move has parent");
                for &v in &tree.node(parent).children[1..] {
                    ui.indent(("var", v), |ui| {
                        self.render_sequence(ui, v, false, clicked);
                    });
                }
            }
            match tree.node(last).children.first() {
                Some(&c) if has_alts => {
                    n = c;
                    alts = true;
                }
                _ => return,
            }
        }
    }

    fn import_window(&mut self, ctx: &egui::Context) {
        let Some(text) = &mut self.import_text else {
            return;
        };
        let mut open = true;
        let mut load = false;
        egui::Window::new("Load FEN or PGN")
            .open(&mut open)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label("Paste a FEN, a PGN, or moves like 1. e4 c5 2. Nf3.");
                ui.add(
                    egui::TextEdit::multiline(text)
                        .desired_rows(10)
                        .desired_width(f32::INFINITY)
                        .code_editor(),
                );
                if let Some(e) = &self.import_error {
                    ui.colored_label(Color32::from_rgb(200, 80, 60), e);
                }
                load = ui.button("Load").clicked();
            });
        if load {
            match load_text(text) {
                Ok(tree) => {
                    self.import_text = None;
                    self.load_tree(tree);
                }
                Err(e) => self.import_error = Some(e.to_string()),
            }
        } else if !open {
            self.import_text = None;
        }
    }

    fn screenshot_window(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut load: Option<GameTree> = None;
        let mut error: Option<String> = None;
        match &mut self.screenshot {
            ScreenshotState::Idle => return,
            ScreenshotState::Reading => {
                egui::Window::new("Reading screenshot")
                    .collapsible(false)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Claude is reading the board…");
                        });
                    });
            }
            ScreenshotState::Failed(e) => {
                let e = e.clone();
                egui::Window::new("Screenshot")
                    .collapsible(false)
                    .show(ctx, |ui| {
                        ui.colored_label(Color32::from_rgb(200, 80, 60), e);
                        close = ui.button("Close").clicked();
                    });
            }
            ScreenshotState::Review { result, fen } => {
                egui::Window::new("Check the position")
                    .collapsible(false)
                    .default_width(520.0)
                    .show(ctx, |ui| {
                        let r = &result.reading;
                        ui.label(format!(
                            "Orientation: {} · Side to move: {} · Castling: {} · Confidence: {}",
                            r.orientation.replace('_', " "),
                            r.side_to_move,
                            r.castling,
                            r.confidence
                        ));
                        if !r.assumptions.is_empty() {
                            ui.label(RichText::new("Assumptions:").strong());
                            for a in &r.assumptions {
                                ui.label(format!("• {a}"));
                            }
                        }
                        if result.used_pgn {
                            ui.label(format!("Move list read: {}", r.pgn));
                        }
                        ui.label("FEN (edit if anything is wrong):");
                        ui.add(
                            egui::TextEdit::singleline(fen)
                                .desired_width(f32::INFINITY)
                                .code_editor(),
                        );
                        if let Err(e) = &result.tree {
                            ui.colored_label(Color32::from_rgb(200, 80, 60), e);
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Load").clicked() {
                                let unchanged = *fen == r.fen;
                                match (&result.tree, unchanged) {
                                    (Ok(tree), true) => load = Some(tree.clone()),
                                    _ => match load_text(fen) {
                                        Ok(t) => load = Some(t),
                                        Err(e) => error = Some(e.to_string()),
                                    },
                                }
                            }
                            if ui.button("Switch side to move").clicked()
                                && let Ok(t) = load_text(fen)
                            {
                                match with_other_side_to_move(t.position()) {
                                    Some(p) => *fen = fen_of(&p),
                                    None => {
                                        error = Some(
                                            "the other side cannot be to move here (a king is in check)"
                                                .into(),
                                        )
                                    }
                                }
                            }
                            close = ui.button("Cancel").clicked();
                        });
                    });
            }
        }
        if let Some(t) = load {
            self.screenshot = ScreenshotState::Idle;
            self.load_tree(t);
        } else if let Some(e) = error {
            self.toast = Some(e);
        } else if close {
            self.screenshot = ScreenshotState::Idle;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let mut changed = false;
        let mut restart = false;
        egui::Window::new("Settings").open(&mut open).show(ctx, |ui| {
            changed |= ui
                .add(egui::Slider::new(&mut self.settings.depth, 8..=30).text("search depth"))
                .changed();
            changed |= ui
                .add(egui::Slider::new(&mut self.settings.multipv, 1..=5).text("candidate moves"))
                .changed();
            ui.checkbox(
                &mut self.settings.auto_explain,
                "Explain every move automatically",
            );
            ui.checkbox(
                &mut self.settings.show_arrow,
                "Show the engine's best move as an arrow",
            );
            ui.separator();
            ui.label("Stockfish executable:");
            ui.text_edit_singleline(&mut self.settings.stockfish_path);
            restart = ui.button("Restart engine").clicked();
        });
        if changed {
            self.reset_analysis();
        }
        if restart {
            self.engine_status = Err("starting engine…".into());
            let _ = self.engine_tx.send(EngineJob::Restart(PathBuf::from(
                self.settings.stockfish_path.trim(),
            )));
        }
        self.show_settings = open;
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        ctx.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) {
                self.tree.back();
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                self.tree.forward();
            }
            if i.key_pressed(egui::Key::Home) {
                self.tree.to_start();
            }
            if i.key_pressed(egui::Key::End) {
                self.tree.to_end();
            }
        });
    }
}

enum Action {
    Explain(NodeId),
    FollowUp(NodeId, String),
    PlayUci(String),
    Goto(NodeId),
    Promote(NodeId),
}

fn class_color(class: MoveClass) -> Color32 {
    match class {
        MoveClass::Best => Color32::from_rgb(90, 170, 90),
        MoveClass::Good => Color32::from_rgb(120, 160, 200),
        MoveClass::Inaccuracy => Color32::from_rgb(220, 180, 60),
        MoveClass::Mistake => Color32::from_rgb(230, 130, 40),
        MoveClass::Blunder => Color32::from_rgb(220, 60, 50),
    }
}

impl eframe::App for TutorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_events(ctx);
        self.keyboard(ctx);
        self.ensure_analysis();

        // Explain the move automatically once its engine report is in.
        let id = self.tree.current();
        if self.settings.auto_explain
            && self.claude.is_some()
            && let Some(s) = self.nodes.get(&id)
            && s.report.is_some()
            && s.chat.is_none()
            && !s.claude_pending
            && s.claude_error.is_none()
        {
            self.request_explanation(ctx, id);
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| self.top_bar(ctx, ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        egui::SidePanel::right("analysis")
            .resizable(true)
            .default_width(460.0)
            .min_width(340.0)
            .show(ctx, |ui| self.right_panel(ctx, ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            let id = self.tree.current();
            let node = self.tree.node(id);
            let state = self.nodes.get(&id);
            let analysis = state.and_then(|s| s.position.as_ref());
            let best = if self.settings.show_arrow {
                analysis
                    .and_then(|a| a.best())
                    .and_then(|c| uci_to_move(&node.position, &c.uci).ok())
            } else {
                None
            };
            let eval = analysis.and_then(|a| a.eval);
            let position = node.position.clone();
            let last_move = node.mv;
            let played = board_ui(
                ui,
                &mut self.view,
                BoardInput {
                    position: &position,
                    last_move,
                    best,
                    eval,
                    glyphs: self.glyphs,
                },
            );
            ui.horizontal(|ui| {
                if ui.button("⏮").on_hover_text("Start (Home)").clicked() {
                    self.tree.to_start();
                }
                if ui.button("◀").on_hover_text("Back (←)").clicked() {
                    self.tree.back();
                }
                if ui.button("▶").on_hover_text("Forward (→)").clicked() {
                    self.tree.forward();
                }
                if ui.button("⏭").on_hover_text("End (End)").clicked() {
                    self.tree.to_end();
                }
                ui.label(format!(
                    "{} to move",
                    if position.turn().is_white() {
                        "White"
                    } else {
                        "Black"
                    }
                ));
            });
            if let Some(m) = played {
                self.play(m);
            }
        });

        self.import_window(ctx);
        self.screenshot_window(ctx);
        self.settings_window(ctx);
    }
}
