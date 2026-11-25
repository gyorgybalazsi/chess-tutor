//! Background work: one thread owns the Stockfish process, and each Claude
//! request runs on its own short-lived thread, so the board never blocks.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
};

use chess_core::{
    analysis::{AnalysisConfig, MoveReport, PositionAnalysis, analyse_move, analyse_position},
    claude::{ClaudeClient, Reply, Request},
    engine::Engine,
    screenshot::{ScreenshotResult, read_screenshot},
    shakmaty::{Chess, Move},
    tree::NodeId,
};

/// The move that led to the node being analysed.
pub struct ParentInfo {
    pub id: NodeId,
    pub position: Chess,
    pub mv: Move,
    pub history: Vec<u64>,
    pub moves_so_far: String,
}

#[allow(clippy::large_enum_variant)]
pub enum EngineJob {
    Analyse {
        generation: u64,
        node: NodeId,
        position: Chess,
        history: Vec<u64>,
        parent: Option<ParentInfo>,
        config: AnalysisConfig,
    },
    Restart(PathBuf),
}

pub enum EngineEvent {
    Ready(String),
    Failed(String),
    Position {
        generation: u64,
        node: NodeId,
        analysis: PositionAnalysis,
    },
    Move {
        generation: u64,
        node: NodeId,
        report: Box<MoveReport>,
    },
    /// The job was skipped because a newer one arrived; ask again if needed.
    Dropped {
        generation: u64,
        node: NodeId,
    },
    Error {
        generation: u64,
        node: NodeId,
        message: String,
    },
}

pub fn spawn_engine_worker(
    path: Option<PathBuf>,
    ctx: eframe::egui::Context,
) -> (mpsc::Sender<EngineJob>, mpsc::Receiver<EngineEvent>) {
    let (job_tx, job_rx) = mpsc::channel::<EngineJob>();
    let (ev_tx, ev_rx) = mpsc::channel::<EngineEvent>();
    thread::spawn(move || {
        let send = |ev: EngineEvent| {
            let _ = ev_tx.send(ev);
            ctx.request_repaint();
        };
        let start = |path: Option<PathBuf>| -> Option<Engine> {
            let Some(path) = path else {
                send(EngineEvent::Failed(
                    "Stockfish not found. Install it, or set STOCKFISH_PATH or the path in Settings.".into(),
                ));
                return None;
            };
            match Engine::spawn(&path) {
                Ok(e) => {
                    send(EngineEvent::Ready(e.name().to_string()));
                    Some(e)
                }
                Err(e) => {
                    send(EngineEvent::Failed(e.to_string()));
                    None
                }
            }
        };
        let mut engine = start(path);
        let mut cache: HashMap<(u64, NodeId), PositionAnalysis> = HashMap::new();

        while let Ok(first) = job_rx.recv() {
            // Only the newest analysis request matters when the user clicks
            // through moves quickly; older ones are reported as dropped.
            let mut pending = vec![first];
            while let Ok(more) = job_rx.try_recv() {
                pending.push(more);
            }
            let mut restart = None;
            let mut latest = None;
            for job in pending {
                match job {
                    EngineJob::Restart(p) => restart = Some(EngineJob::Restart(p)),
                    analyse => {
                        if let Some(EngineJob::Analyse { generation, node, .. }) = latest.replace(analyse) {
                            send(EngineEvent::Dropped { generation, node });
                        }
                    }
                }
            }
            for job in restart.into_iter().chain(latest) {
                match job {
                    EngineJob::Restart(path) => {
                        drop(engine.take());
                        cache.clear();
                        engine = start(Some(path));
                    }
                    EngineJob::Analyse {
                        generation,
                        node,
                        position,
                        history,
                        parent,
                        config,
                    } => {
                        let Some(eng) = engine.as_mut() else {
                            send(EngineEvent::Error {
                                generation,
                                node,
                                message: "engine not running".into(),
                            });
                            continue;
                        };
                        cache.retain(|(g, _), _| *g == generation);
                        let result = (|| -> Result<(), String> {
                            if let Some(p) = parent {
                                let before = match cache.get(&(generation, p.id)) {
                                    Some(a) => a.clone(),
                                    None => {
                                        let a = analyse_position(eng, &p.position, &p.history, &config)
                                            .map_err(|e| e.to_string())?;
                                        cache.insert((generation, p.id), a.clone());
                                        send(EngineEvent::Position {
                                            generation,
                                            node: p.id,
                                            analysis: a.clone(),
                                        });
                                        a
                                    }
                                };
                                let report = analyse_move(
                                    eng,
                                    &p.position,
                                    &before,
                                    p.mv,
                                    &p.moves_so_far,
                                    &p.history,
                                    &config,
                                )
                                .map_err(|e| e.to_string())?;
                                send(EngineEvent::Move {
                                    generation,
                                    node,
                                    report: Box::new(report),
                                });
                            }
                            let a = match cache.get(&(generation, node)) {
                                Some(a) => a.clone(),
                                None => {
                                    let a = analyse_position(eng, &position, &history, &config)
                                        .map_err(|e| e.to_string())?;
                                    cache.insert((generation, node), a.clone());
                                    a
                                }
                            };
                            send(EngineEvent::Position {
                                generation,
                                node,
                                analysis: a,
                            });
                            Ok(())
                        })();
                        if let Err(message) = result {
                            send(EngineEvent::Error {
                                generation,
                                node,
                                message,
                            });
                            // The engine may have died; it is restarted on the
                            // next Restart job from the settings window.
                        }
                    }
                }
            }
        }
    });
    (job_tx, ev_rx)
}

pub enum ClaudeEvent {
    Explanation {
        generation: u64,
        node: NodeId,
        request: Request,
        result: Result<Reply, String>,
    },
    Screenshot(Result<ScreenshotResult, String>),
}

pub fn spawn_claude_request(
    client: Arc<ClaudeClient>,
    request: Request,
    generation: u64,
    node: NodeId,
    tx: mpsc::Sender<ClaudeEvent>,
    ctx: eframe::egui::Context,
) {
    thread::spawn(move || {
        let result = client.send(&request).map_err(|e| e.to_string());
        let _ = tx.send(ClaudeEvent::Explanation {
            generation,
            node,
            request,
            result,
        });
        ctx.request_repaint();
    });
}

pub fn spawn_screenshot_request(
    client: Arc<ClaudeClient>,
    image: Vec<u8>,
    tx: mpsc::Sender<ClaudeEvent>,
    ctx: eframe::egui::Context,
) {
    thread::spawn(move || {
        let result = read_screenshot(&client, &image).map_err(|e| e.to_string());
        let _ = tx.send(ClaudeEvent::Screenshot(result));
        ctx.request_repaint();
    });
}
